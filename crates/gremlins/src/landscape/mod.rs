//! Heuristic file-structure discovery.
//!
//! `LandscapeExplorer` lets agents see the shape of a file — function, type,
//! heading, or top-level key locations — without reading the body. Everything
//! here is deliberately regex/line oriented; no native parsing dependencies.

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
    pub fn build(&self, root: &Path, depth_limit: usize) -> Result<Vec<FileOutline>, OutlineError> {
        let mut out = Vec::new();
        self.build_dir(root, 0, depth_limit, &mut out)?;
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    fn build_dir(
        &self,
        dir: &Path,
        depth: usize,
        depth_limit: usize,
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
                self.build_dir(&path, depth + 1, depth_limit, out)?;
            } else if metadata.is_file() {
                if !is_outlineable_file(&path) || metadata.len() > MAX_OUTLINE_BYTES {
                    continue;
                }
                if let Ok(items) = self.outline_file(&path) {
                    if !items.is_empty() {
                        out.push(FileOutline { path, items });
                    }
                }
            }
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn outline_items(explorer: &LandscapeExplorer, path: &Path) -> Vec<OutlineItem> {
        explorer.outline_file(path).unwrap()
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

        let depth0 = LandscapeExplorer.build(root.path(), 0).unwrap();
        assert_eq!(depth0.len(), 1);
        assert_eq!(depth0[0].path, root.path().join("top.rs"));

        let depth1 = LandscapeExplorer.build(root.path(), 1).unwrap();
        let paths: Vec<&Path> = depth1.iter().map(|f| f.path.as_path()).collect();
        assert!(paths.contains(&root.path().join("top.rs").as_path()));
        assert!(paths.contains(&root.path().join("a/mid.rs").as_path()));
        assert!(!paths.contains(&root.path().join("a/b/deep.md").as_path()));

        let all = LandscapeExplorer.build(root.path(), 10).unwrap();
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

        let all = LandscapeExplorer.build(root.path(), 10).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].path, root.path().join("keep.rs"));
    }

    #[test]
    fn build_missing_root_is_error() {
        let path = PathBuf::from("/definitely/not/present/build-root");
        let result = LandscapeExplorer.build(&path, 0);
        assert!(matches!(result, Err(OutlineError::Read(_))));
    }
}
