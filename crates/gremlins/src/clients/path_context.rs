//! Tool-dispatch cache for pre-resolved file paths.
//!
//! Scans the operator prompt for file-path tokens and pre-computes landscape
//! outlines for the paths that exist inside the project root. During tool
//! dispatch, a matching Landscape call is served straight from this cache
//! instead of walking the file, short-circuiting the street-level discovery
//! round-trip.
//!
//! Construction is split into two phases so no blocking I/O happens on the
//! async task:
//!
//! 1. [`PathContext::new`] is lightweight — it only tokenizes the prompt and
//!    deduplicates the candidates. No filesystem access.
//! 2. [`PathContext::resolve`] does all the I/O (canonicalize, size check,
//!    outline). Call it inside `spawn_blocking`.
//!
//! Resolution is project-root-only with fail-closed containment: relative
//! paths are joined onto the project root, absolute paths are re-anchored on
//! the project root as a fallback, and anything that canonicalizes outside
//! the root, contains `..`, or doesn't exist is silently skipped. Unresolved
//! tokens produce no entries — they just fall through to normal dispatch.
//!
//! Nothing here mutates the operator message: the cache is a read-only
//! side-table consulted by the dispatch layer.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::core::paths::expand_user;
use crate::landscape::LandscapeExplorer;

/// Maximum file size in bytes to pass to `LandscapeExplorer::outline_file`.
const MAX_OUTLINE_FILE_BYTES: u64 = 1024 * 1024;

/// Maximum number of candidate tokens kept (mirrors the old TUI matcher cap).
const MAX_CANDIDATES: usize = 5;

/// Matches path-shaped tokens in a prompt:
/// - `/`- or `.`-prefixed paths (absolutes, dotfiles, `~/…`),
/// - extension-based paths (`src/lib.rs`),
/// - bare directory paths (`crates/gremlins/src/`).
const CANDIDATE_PATTERN: &str = r"(?:^|\s)(~?[/.][\w./~-]+|[\w./-]+\.\w{1,10}|[\w-]+/[\w./-]*)\b";

/// Pre-resolved file-path context: a map from canonical file path to its
/// pre-computed landscape outline (or the display path for files with no
/// recognizable structure).
///
/// [`candidates`](Self::candidates) holds the raw prompt tokens gathered by
/// [`PathContext::new`]; [`resolved`](Self::resolved) is populated by
/// [`PathContext::resolve`] and then consulted read-only during dispatch.
#[derive(Clone, Debug, Default)]
pub(crate) struct PathContext {
    /// Deduplicated prompt tokens, preserved in first-seen order.
    candidates: Vec<String>,
    /// Canonical path -> pre-computed outline.
    resolved: HashMap<PathBuf, String>,
}

impl PathContext {
    /// Phase one: extract file-path tokens from `prompt` and deduplicate them.
    ///
    /// Purely lexical — no filesystem access, safe to call on the async task.
    /// A regex-compilation failure yields an empty context.
    pub(crate) fn new(prompt: &str) -> Self {
        let re = match regex::Regex::new(CANDIDATE_PATTERN) {
            Ok(r) => r,
            Err(_) => return Self::default(),
        };

        let mut candidates: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for cap in re.captures_iter(prompt) {
            let Some(m) = cap.get(1) else { continue };
            if candidates.len() >= MAX_CANDIDATES {
                break;
            }
            let token = m.as_str();
            if seen.insert(token.to_string()) {
                candidates.push(token.to_string());
            }
        }

        Self {
            candidates,
            resolved: HashMap::new(),
        }
    }

    /// Phase two: resolve the collected candidates against `project_root`.
    ///
    /// Does all filesystem I/O and must be called from a blocking context.
    /// Only files that exist inside the project root are cached; symlink
    /// escapes, `..` traversal, oversized files, and nonexistent paths are
    /// silently skipped.
    pub(crate) fn resolve(&mut self, project_root: &Path) {
        self.resolved.clear();

        let canonical_root = match project_root.canonicalize() {
            Ok(p) => p,
            Err(_) => return,
        };

        let explorer = LandscapeExplorer;
        for candidate in &self.candidates {
            let full = match resolve_candidate(candidate, project_root) {
                Some(p) => p,
                None => continue,
            };

            // Verify the file exists and is within the project root. The
            // canonicalization also resolves symlinks, so a symlink pointing
            // outside the root fails containment here.
            let canonical = match full.canonicalize() {
                Ok(p) => p,
                Err(_) => continue,
            };
            if !canonical.starts_with(&canonical_root) {
                continue;
            }

            // Directories are not outline-able here; only cache real files.
            let meta = match std::fs::metadata(&canonical) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if !meta.is_file() || meta.len() > MAX_OUTLINE_FILE_BYTES {
                continue;
            }

            let outline_str = match explorer.outline_file(&canonical, false) {
                Ok(items) if !items.is_empty() => items
                    .iter()
                    .map(|item| format!("{}|{}|{}", item.line, item.kind, item.name))
                    .collect::<Vec<_>>()
                    .join(" "),
                Ok(_) => canonical.display().to_string(),
                Err(_) => continue,
            };

            self.resolved.insert(canonical, outline_str);
        }
    }

    /// Look up a path. The caller must supply the already-canonicalized path
    /// (tool dispatch passes paths through `resolve()` which canonicalizes).
    pub(crate) fn get(&self, canonical_path: &Path) -> Option<&str> {
        self.resolved.get(canonical_path).map(|s| s.as_str())
    }

    /// Whether this context has any resolved paths.
    pub(crate) fn is_empty(&self) -> bool {
        self.resolved.is_empty()
    }
}

/// Turn a raw prompt token into a concrete path on disk.
///
/// `~`-prefixed tokens are expanded first. Relative tokens (and `~` expansions)
/// are joined onto `project_root`. Absolute tokens are used as-is, but if that
/// path does not exist they are re-anchored on `project_root` — an agent often
/// writes `/crates/foo/src/lib.rs` meaning "relative to the project root".
///
/// Returns `None` for `..` traversal (fail-closed) or when no candidate exists.
fn resolve_candidate(candidate: &str, project_root: &Path) -> Option<PathBuf> {
    if candidate.contains("..") {
        return None;
    }

    let expanded = expand_user(candidate);
    let path = PathBuf::from(&expanded);

    let mut attempts: Vec<PathBuf> = Vec::new();
    if path.is_absolute() {
        attempts.push(path.clone());
        // Re-interpret a leading `/` as project-root-relative.
        if let Ok(rel) = path.strip_prefix("/") {
            if !rel.as_os_str().is_empty() {
                attempts.push(project_root.join(rel));
            }
        }
    } else {
        attempts.push(project_root.join(&path));
    }

    attempts.into_iter().find(|p| p.exists())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn candidates_of(prompt: &str) -> Vec<String> {
        PathContext::new(prompt).candidates
    }

    #[test]
    fn tokenizes_extension_and_directory_paths() {
        let got = candidates_of(
            "check src/lib.rs and crates/gremlins/src/ for the fix, plus .github/workflows/ci.yml",
        );
        assert_eq!(
            got,
            vec![
                "src/lib.rs".to_string(),
                "crates/gremlins/src".to_string(),
                ".github/workflows/ci.yml".to_string(),
            ]
        );
    }

    #[test]
    fn tokenizes_tilde_prefixed_paths() {
        let got = candidates_of("read ~/notes.md while you are at it");
        assert_eq!(got, vec!["~/notes.md".to_string()]);
    }

    #[test]
    fn tokenizes_dot_prefixed_paths() {
        let got = candidates_of("look at .gitignore and ./Cargo.toml");
        assert_eq!(
            got,
            vec![".gitignore".to_string(), "./Cargo.toml".to_string()]
        );
    }

    #[test]
    fn tokenizes_extensionless_paths() {
        let got = candidates_of("open src/Makefile next");
        assert_eq!(got, vec!["src/Makefile".to_string()]);
    }

    #[test]
    fn tokenization_deduplicates_preserving_order() {
        let got = candidates_of("src/lib.rs then src/lib.rs again src/main.rs");
        assert_eq!(
            got,
            vec!["src/lib.rs".to_string(), "src/main.rs".to_string()]
        );
    }

    #[test]
    fn tokenization_is_capped() {
        let got =
            candidates_of("a/one.rs b/two.rs c/three.rs d/four.rs e/five.rs f/six.rs g/seven.rs");
        assert_eq!(got.len(), MAX_CANDIDATES);
        assert_eq!(got[0], "a/one.rs");
    }

    #[test]
    fn resolves_existing_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn hello() {}\n").unwrap();
        fs::write(root.join("src/Makefile"), "all:\n\techo hi\n").unwrap();

        let mut ctx = PathContext::new("look at src/lib.rs and src/Makefile");
        ctx.resolve(root);

        let canonical_lib = root.join("src/lib.rs").canonicalize().unwrap();
        let canonical_make = root.join("src/Makefile").canonicalize().unwrap();
        assert_eq!(ctx.resolved.len(), 2);
        assert!(ctx.get(&canonical_lib).is_some());
        assert!(ctx.get(&canonical_make).is_some());
        assert!(!ctx.is_empty());
    }

    #[test]
    fn resolves_absolute_path_against_project_root() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn hello() {}\n").unwrap();

        // A `/`-rooted token that doesn't exist on the real filesystem is
        // re-anchored on the project root.
        let mut ctx = PathContext::new("see /src/lib.rs");
        ctx.resolve(root);

        let canonical = root.join("src/lib.rs").canonicalize().unwrap();
        assert!(ctx.get(&canonical).is_some());
    }

    #[test]
    fn nonexistent_paths_produce_empty_cache() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut ctx = PathContext::new("read src/does_not_exist.rs");
        ctx.resolve(tmp.path());

        assert!(ctx.is_empty());
        assert!(ctx.resolved.is_empty());
    }

    #[test]
    fn parent_dir_paths_are_rejected() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        fs::write(root.join("secret.rs"), "fn s() {}\n").unwrap();
        fs::create_dir_all(root.join("sub")).unwrap();

        let mut ctx = PathContext::new("read ../secret.rs and sub/../secret.rs");
        ctx.resolve(root);

        assert!(ctx.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escaping_project_root_is_rejected() {
        let outside = tempfile::TempDir::new().unwrap();
        fs::write(outside.path().join("secret.rs"), "fn s() {}\n").unwrap();

        let project = tempfile::TempDir::new().unwrap();
        let link = project.path().join("link.rs");
        std::os::unix::fs::symlink(outside.path().join("secret.rs"), &link).unwrap();

        let mut ctx = PathContext::new("inspect link.rs");
        ctx.resolve(project.path());

        // The symlink canonicalizes outside the root, so nothing is cached.
        assert!(ctx.is_empty());
    }
}
