//! Heuristic file-structure discovery.
//!
//! `LandscapeExplorer` lets agents see the shape of a file — function, type,
//! heading, or top-level key locations — without reading the body. Everything
//! here is deliberately regex/line oriented; no native parsing dependencies.

pub(crate) mod cache;
mod error;
pub(crate) mod heuristic;

pub use error::OutlineError;

use std::path::{Path, PathBuf};

pub const FN: &str = "fn";
pub const STRUCT: &str = "struct";
pub const IMPL: &str = "impl";
pub const TRAIT: &str = "trait";
pub const ENUM: &str = "enum";
pub const MOD: &str = "mod";
pub const CLASS: &str = "class";
pub const METHOD: &str = "method";
pub const HEADING: &str = "heading";
pub const KEY: &str = "key";

const MAX_OUTLINE_BYTES: u64 = 1024 * 1024;
const SKIP_DIR_NAMES: &[&str] = &[
    "target",
    "node_modules",
    "vendor",
    "dist",
    "build",
    "__pycache__",
];

/// One discovered structural item and its 1-based line number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineItem {
    pub kind: &'static str,
    pub name: String,
    pub line: usize,
}

impl OutlineItem {
    pub fn new(kind: &'static str, name: impl Into<String>, line: usize) -> Self {
        Self {
            kind,
            name: name.into(),
            line,
        }
    }
}

/// A file path together with the outline items found inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileOutline {
    pub path: PathBuf,
    pub items: Vec<OutlineItem>,
}

/// Concrete outliner used by agent tooling.
///
/// Construction is deliberately trivial: the explorer carries no state, so a
/// unit struct is enough for both single-file outlines and directory builds.
#[derive(Debug, Clone, Copy, Default)]
pub struct LandscapeExplorer;

impl LandscapeExplorer {
    /// Outline a single file. Unknown extensions yield an empty outline, not an
    /// error. Non-UTF-8 files yield [`OutlineError::Utf8`].
    pub fn outline_file(&self, path: &Path) -> Result<Vec<OutlineItem>, OutlineError> {
        let extension = path_extension(path);
        if !heuristic::recognizes(&extension) {
            return Ok(Vec::new());
        }

        let bytes = std::fs::read(path)?;
        let content = std::str::from_utf8(&bytes)?;
        Ok(heuristic::scan(&extension, content))
    }

    /// Walk a directory tree and collect outlines for every recognized file.
    ///
    /// `depth_limit` counts nested directory levels: 0 only scans files in
    /// `root` itself, 1 also scans one subdirectory level, and so on. Symlinks
    /// are not followed. Hidden and vendor directories, unrecognized files, and
    /// oversized files are skipped. Files with no recognizable items are
    /// omitted.
    ///
    /// When `cache_path` is `Some`, unchanged files (matching `mtime` and
    /// `size`) are served from the cache file at that path and the refreshed
    /// cache is written back after the walk. When it is `None`, behavior is
    /// exactly as before: every recognized file is parsed.
    pub fn build(
        &self,
        root: &Path,
        depth_limit: usize,
        cache_path: Option<&Path>,
    ) -> Result<Vec<FileOutline>, OutlineError> {
        let mut cache = cache_path.map(cache::LandscapeCache::load);
        let canonical_cache = cache_path.and_then(|path| std::fs::canonicalize(path).ok());
        let mut out = Vec::new();
        self.build_dir(
            root,
            0,
            depth_limit,
            cache.as_mut(),
            canonical_cache.as_deref(),
            &mut out,
        )?;
        out.sort_by(|a, b| a.path.cmp(&b.path));
        if let (Some(cache), Some(path)) = (cache, cache_path) {
            // Caching is an optimization: a failed write never fails the build.
            let _ = cache.save(path);
        }
        Ok(out)
    }

    fn build_dir(
        &self,
        dir: &Path,
        depth: usize,
        depth_limit: usize,
        mut cache: Option<&mut cache::LandscapeCache>,
        canonical_cache: Option<&Path>,
        out: &mut Vec<FileOutline>,
    ) -> Result<(), OutlineError> {
        if depth > depth_limit {
            return Ok(());
        }

        let entries = std::fs::read_dir(dir)?;
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .collect();
        paths.sort();

        for path in paths {
            let Ok(metadata) = path.symlink_metadata() else {
                continue;
            };
            if metadata.is_dir() {
                if should_skip_dir(&path) {
                    continue;
                }
                self.build_dir(
                    &path,
                    depth + 1,
                    depth_limit,
                    cache.as_deref_mut(),
                    canonical_cache,
                    out,
                )?;
            } else if metadata.is_file() {
                if !is_outlineable_file(&path) || metadata.len() > MAX_OUTLINE_BYTES {
                    continue;
                }
                if canonical_cache.is_some_and(|canonical| same_file(canonical, &path)) {
                    continue;
                }
                self.collect_file(&path, cache.as_deref_mut(), out);
            }
        }
        Ok(())
    }

    /// Add `path` to `out`, using the cache when its fingerprint is known and
    /// matches, and reparsing otherwise.
    fn collect_file(
        &self,
        path: &Path,
        cache: Option<&mut cache::LandscapeCache>,
        out: &mut Vec<FileOutline>,
    ) {
        if let Some(cache) = cache {
            let fingerprint = file_fingerprint(path);
            if let Some((mtime_ns, size)) = fingerprint {
                if let Some(items) = cache.get(path, mtime_ns, size) {
                    if !items.is_empty() {
                        out.push(FileOutline {
                            path: path.to_path_buf(),
                            items,
                        });
                    }
                    return;
                }
            }

            let Ok(items) = self.outline_file(path) else {
                cache.remove(path);
                return;
            };
            // Cache every successful parse, even one with no items, so an
            // unchanged empty file is not re-parsed on the next build.
            if let Some((mtime_ns, size)) = fingerprint {
                cache.insert(path, mtime_ns, size, items.clone());
            }
            if !items.is_empty() {
                out.push(FileOutline {
                    path: path.to_path_buf(),
                    items,
                });
            }
        } else {
            if let Ok(items) = self.outline_file(path) {
                if !items.is_empty() {
                    out.push(FileOutline {
                        path: path.to_path_buf(),
                        items,
                    });
                }
            }
        }
    }
}

fn path_extension(path: &Path) -> String {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .unwrap_or_default()
}

fn is_outlineable_file(path: &Path) -> bool {
    heuristic::recognizes(&path_extension(path))
}

fn should_skip_dir(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name.starts_with('.') || SKIP_DIR_NAMES.contains(&name)
}

/// Resolve a file's cache fingerprint: `(mtime_ns, size)`.
///
/// Stat errors (including a vanished file) are cache misses, not build errors:
/// the caller simply falls back to a fresh parse and stores nothing.
fn file_fingerprint(path: &Path) -> Option<(u64, u64)> {
    let metadata = std::fs::metadata(path).ok()?;
    let mtime = metadata.modified().ok()?;
    let mtime_ns = mtime.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos() as u64;
    Some((mtime_ns, metadata.len()))
}

/// Whether `path` names the same on-disk file as the already-canonicalized
/// cache blob `canonical_cache`.
///
/// The cheap file-name check avoids canonicalizing every visited file; the
/// canonical comparison is only performed for files that share the cache
/// blob's name (typically just one).
fn same_file(canonical_cache: &Path, path: &Path) -> bool {
    if path.file_name() != canonical_cache.file_name() {
        return false;
    }
    match std::fs::canonicalize(path) {
        Ok(canonical) => canonical == canonical_cache,
        Err(_) => false,
    }
}

/// One-line legend mapping the compact kind abbreviations used in the block
/// back to the full [`FN`], [`STRUCT`], [`HEADING`], and similar constants.
///
/// The full constants stay human-readable for tool output; this legend only
/// exists because the landscape block favors density over self-evidence.
const KIND_LEGEND: &str = "k:key fn struct impl trait enum mod c:class m:method h:heading";

/// Format the captured outlines as the structural landscape block injected
/// into an agent stage's system prompt.
///
/// Paths under `root` are emitted relative to it (with `root` itself named in
/// a preamble comment); anything outside `root` keeps its absolute path.
/// Files with fewer than three outline items are skipped: they carry too
/// little structural signal for the per-file overhead.
///
/// An empty landscape (no recognized files, no outlineable items, or no
/// file with at least three items) formats to an empty string so callers can
/// skip the block entirely.
pub(crate) fn format_landscape_block(outlines: &[FileOutline], root: &Path) -> String {
    let outlines: Vec<&FileOutline> = outlines
        .iter()
        .filter(|outline| outline.items.len() >= 3)
        .collect();
    if outlines.is_empty() {
        return String::new();
    }

    let mut block = String::new();
    block.push_str("<!-- BEGIN LANDSCAPE BLOCK -->\n");
    block.push_str(
        "The block below is a structural outline captured by the harness at stage start.\n",
    );
    block.push_str("Line numbers are exact at capture time but may drift if files are edited.\n");
    block.push_str("Use OutlineFile for current line numbers after edits.\n\n");
    block.push_str(&format!("<!-- root: {} -->\n", root.display()));
    block.push_str(&format!("<!-- {KIND_LEGEND} -->\n\n"));

    for outline in outlines {
        let path = match outline.path.strip_prefix(root) {
            Ok(relative) if !relative.as_os_str().is_empty() => relative.display().to_string(),
            _ => outline.path.display().to_string(),
        };
        block.push_str(&format!("{path}:\n"));
        for item in &outline.items {
            block.push_str(&format!(
                "{}|{}|{}\n",
                item.line,
                kind_abbrev(item.kind),
                item.name
            ));
        }
        block.push('\n');
    }

    block.push_str("<!-- END LANDSCAPE BLOCK -->\n");
    block
}

/// Map a full kind constant to its compact landscape abbreviation.
///
/// The kind constants themselves are untouched; this mapping exists only for
/// the landscape block and deliberately keeps the already-short Rust kinds.
fn kind_abbrev(kind: &str) -> &str {
    match kind {
        CLASS => "c",
        METHOD => "m",
        HEADING => "h",
        KEY => "k",
        _ => kind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outline_items(explorer: &LandscapeExplorer, path: &Path) -> Vec<OutlineItem> {
        explorer.outline_file(path).unwrap()
    }

    #[test]
    fn format_landscape_block_is_empty_without_outlines() {
        assert_eq!(format_landscape_block(&[], Path::new("/repo")), "");
    }

    #[test]
    fn format_landscape_block_skips_empty_outlines() {
        let outline = FileOutline {
            path: PathBuf::from("src/main.rs"),
            items: Vec::new(),
        };
        assert_eq!(
            format_landscape_block(std::slice::from_ref(&outline), Path::new("/repo")),
            ""
        );
    }

    #[test]
    fn format_landscape_block_skips_files_with_fewer_than_three_items() {
        let root = Path::new("/repo");
        let small = FileOutline {
            path: root.join("small.rs"),
            items: vec![OutlineItem::new(FN, "small", 1)],
        };
        let big = FileOutline {
            path: root.join("big.rs"),
            items: vec![
                OutlineItem::new(FN, "first", 1),
                OutlineItem::new(STRUCT, "Big", 2),
                OutlineItem::new(IMPL, "Big", 3),
            ],
        };
        let block = format_landscape_block(&[small, big], root);
        assert!(!block.contains("small.rs"));
        assert!(block.contains("big.rs:\n"));
    }

    #[test]
    fn format_landscape_block_formats_files_and_items() {
        let root = Path::new("/repo");
        let outline = FileOutline {
            path: root.join("src/main.rs"),
            items: vec![
                OutlineItem::new(FN, "main", 12),
                OutlineItem::new(STRUCT, "Config", 45),
                OutlineItem::new(HEADING, "Overview", 3),
            ],
        };
        let block = format_landscape_block(std::slice::from_ref(&outline), root);
        assert!(block.starts_with("<!-- BEGIN LANDSCAPE BLOCK -->\n"));
        assert!(block.contains(&format!("<!-- root: {} -->\n", root.display())));
        assert!(block
            .contains("<!-- k:key fn struct impl trait enum mod c:class m:method h:heading -->\n"));
        assert!(block.lines().any(|line| line == "src/main.rs:"));
        assert!(!block.lines().any(|line| line == "/repo/src/main.rs:"));
        assert!(block.contains("12|fn|main\n"));
        assert!(block.contains("45|struct|Config\n"));
        assert!(block.contains("3|h|Overview\n"));
        assert!(block.ends_with("<!-- END LANDSCAPE BLOCK -->\n"));
    }

    #[test]
    fn format_landscape_block_keeps_absolute_path_outside_root() {
        let root = Path::new("/repo");
        let outline = FileOutline {
            path: PathBuf::from("/elsewhere/main.rs"),
            items: vec![
                OutlineItem::new(FN, "main", 1),
                OutlineItem::new(STRUCT, "Config", 2),
                OutlineItem::new(IMPL, "Config", 3),
            ],
        };
        let block = format_landscape_block(std::slice::from_ref(&outline), root);
        assert!(block.lines().any(|line| line == "/elsewhere/main.rs:"));
        assert!(!block.lines().any(|line| line == "main.rs:"));
    }

    #[test]
    fn repo_root_landscape_block_is_under_300kb() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .to_path_buf();
        let outlines = LandscapeExplorer.build(&root, usize::MAX, None).unwrap();
        let block = format_landscape_block(&outlines, &root);
        assert!(
            block.len() < 300 * 1024,
            "landscape block is {} bytes, expected under 307200",
            block.len()
        );
    }

    #[test]
    fn outline_rust_file_includes_all_kinds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.rs");
        std::fs::write(
            &path,
            "fn main() {}\nstruct Foo;\nimpl Foo {}\ntrait Shape {}\nmod inner;\npub enum Color {}\n",
        )
        .unwrap();
        let items = outline_items(&LandscapeExplorer, &path);
        let kinds: Vec<&str> = items.iter().map(|i| i.kind).collect();
        assert_eq!(kinds, vec![FN, STRUCT, IMPL, TRAIT, MOD, ENUM]);
    }

    #[test]
    fn outline_markdown_returns_heading_items() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("doc.md");
        std::fs::write(&path, "# One\n## Two\n").unwrap();
        let items = outline_items(&LandscapeExplorer, &path);
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|i| i.kind == HEADING));
    }

    #[test]
    fn outline_unknown_extension_skips_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.txt");
        // The file does not exist, but the unknown extension is rejected before
        // the read is attempted, so the call succeeds with an empty outline.
        let items = outline_items(&LandscapeExplorer, &path);
        assert!(items.is_empty());
    }

    #[test]
    fn outline_missing_file_is_read_error() {
        let path = PathBuf::from("/definitely/not/present/outline-test.rs");
        let result = LandscapeExplorer.outline_file(&path);
        assert!(matches!(result, Err(OutlineError::Read(_))));
    }

    #[test]
    fn outline_binary_file_is_utf8_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("binary.rs");
        std::fs::write(&path, [0xff, 0xfe, 0x00, 0x01]).unwrap();
        let result = LandscapeExplorer.outline_file(&path);
        assert!(matches!(result, Err(OutlineError::Utf8(_))));
    }

    #[test]
    fn build_collects_nested_files_and_respects_depth() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("a/b")).unwrap();
        std::fs::write(root.path().join("top.rs"), "fn top() {}\n").unwrap();
        std::fs::write(root.path().join("a/mid.rs"), "fn mid() {}\n").unwrap();
        std::fs::write(root.path().join("a/b/deep.md"), "# Deep\n").unwrap();

        let depth0 = LandscapeExplorer.build(root.path(), 0, None).unwrap();
        assert_eq!(depth0.len(), 1);
        assert_eq!(depth0[0].path, root.path().join("top.rs"));

        let depth1 = LandscapeExplorer.build(root.path(), 1, None).unwrap();
        let paths: Vec<&Path> = depth1.iter().map(|f| f.path.as_path()).collect();
        assert!(paths.contains(&root.path().join("top.rs").as_path()));
        assert!(paths.contains(&root.path().join("a/mid.rs").as_path()));
        assert!(!paths.contains(&root.path().join("a/b/deep.md").as_path()));

        let all = LandscapeExplorer.build(root.path(), 10, None).unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn build_skips_hidden_vendor_and_unrecognized_files() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("vendor")).unwrap();
        std::fs::create_dir_all(root.path().join(".git")).unwrap();
        std::fs::write(root.path().join("keep.rs"), "fn keep() {}\n").unwrap();
        std::fs::write(root.path().join("vendor/skip.rs"), "fn skip() {}\n").unwrap();
        std::fs::write(root.path().join(".git/config.rs"), "fn skip() {}\n").unwrap();
        std::fs::write(root.path().join("notes.txt"), "fn skip() {}\n").unwrap();

        let all = LandscapeExplorer.build(root.path(), 10, None).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].path, root.path().join("keep.rs"));
    }

    #[test]
    fn build_missing_root_is_error() {
        let path = PathBuf::from("/definitely/not/present/build-root");
        let result = LandscapeExplorer.build(&path, 0, None);
        assert!(matches!(result, Err(OutlineError::Read(_))));
    }

    #[test]
    fn build_with_and_without_cache_produce_identical_results() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(root.path().join("b.md"), "# B\n").unwrap();

        let uncached = LandscapeExplorer.build(root.path(), 10, None).unwrap();
        let cache_path = root.path().join("landscape.cache.json");
        let cached = LandscapeExplorer
            .build(root.path(), 10, Some(&cache_path))
            .unwrap();

        assert_eq!(uncached, cached);
        assert!(cache_path.exists());

        // A second cached build returns identical results even though the
        // cache blob itself now lives inside the walked root.
        let recached = LandscapeExplorer
            .build(root.path(), 10, Some(&cache_path))
            .unwrap();
        assert_eq!(uncached, recached);
    }

    #[test]
    fn build_cache_reuses_untouched_files_and_invalidates_touched() {
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("a.rs");
        let b = root.path().join("b.rs");
        // Same byte length, so rewriting `a` below keeps its size stable.
        std::fs::write(&a, "fn a() {}\n").unwrap();
        std::fs::write(&b, "fn b() {}\n").unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let cache_path = cache_dir.path().join("landscape.cache.json");

        let first = LandscapeExplorer
            .build(root.path(), 10, Some(&cache_path))
            .unwrap();
        assert_eq!(first.len(), 2);

        // Rewrite `a` with a different name but an unchanged fingerprint:
        // content and size stay the same, and mtime is restored afterwards. A
        // cache hit must therefore report the old name.
        let a_mtime = std::fs::metadata(&a).unwrap().modified().unwrap();
        std::fs::write(&a, "fn A() {}\n").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&a)
            .unwrap()
            .set_modified(a_mtime)
            .unwrap();

        // Rewrite `b` with different-length content so its fingerprint (size,
        // and usually mtime) changes; the reparse must pick up the new name
        // even if timestamp resolution does not advance.
        std::fs::write(&b, "fn touched() {}\n").unwrap();

        let second = LandscapeExplorer
            .build(root.path(), 10, Some(&cache_path))
            .unwrap();
        assert_eq!(second.len(), 2);

        let names = |path: &Path| -> Vec<String> {
            second
                .iter()
                .find(|outline| outline.path == path)
                .map(|outline| outline.items.iter().map(|item| item.name.clone()).collect())
                .unwrap_or_default()
        };
        assert_eq!(names(&a), vec!["a"]);
        assert_eq!(names(&b), vec!["touched"]);
    }

    #[test]
    fn build_corrupt_cache_is_treated_as_empty_and_rewritten() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.rs"), "fn a() {}\n").unwrap();
        let cache_path = root.path().join("landscape.cache.json");
        std::fs::write(&cache_path, "{ this is not valid json").unwrap();

        let result = LandscapeExplorer
            .build(root.path(), 10, Some(&cache_path))
            .unwrap();
        assert_eq!(result.len(), 1);

        // The next build must have overwritten the corrupt blob with a valid one.
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cache_path).unwrap()).unwrap();
        assert!(value.is_object());
    }
}
