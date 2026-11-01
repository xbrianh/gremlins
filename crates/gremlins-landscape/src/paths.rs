//! Minimal path utilities extracted from `gremlins::core::paths` so this
//! crate stays self-contained.
use std::path::{Path, PathBuf};

/// Resolve the real path by canonicalizing the deepest ancestor that exists
/// on disk, then re-appending the non-existent suffix. Falls back to the
/// original path if even the filesystem root can't be canonicalized.
pub(crate) fn canonicalize_or_ancestor(p: &Path) -> PathBuf {
    match p.canonicalize() {
        Ok(c) => c,
        Err(_) => {
            let mut ancestor = p;
            loop {
                match ancestor.canonicalize() {
                    Ok(c) => {
                        let suffix = p.strip_prefix(ancestor).unwrap_or(p);
                        // Resolve dangling symlinks in the suffix.
                        let components: Vec<_> = suffix.components().collect();
                        let mut probe = c.clone();
                        for (i, comp) in components.iter().enumerate() {
                            probe = probe.join(comp);
                            if probe.is_symlink() {
                                if let Ok(target) = std::fs::read_link(&probe) {
                                    let mut resolved = if target.is_absolute() {
                                        target
                                    } else {
                                        probe.parent().unwrap().join(&target)
                                    };
                                    for comp in &components[i + 1..] {
                                        resolved = resolved.join(comp);
                                    }
                                    return canonicalize_or_ancestor(&resolved);
                                }
                                break;
                            }
                        }
                        return c.join(suffix);
                    }
                    Err(_) => match ancestor.parent() {
                        Some(parent) if !parent.as_os_str().is_empty() => ancestor = parent,
                        _ => return p.to_path_buf(),
                    },
                }
            }
        }
    }
}

/// Check whether `p` is contained within any of the `roots`.
/// All paths are canonicalized before comparison.
pub(crate) fn within_worktree(p: &Path, roots: &[PathBuf]) -> bool {
    let p = canonicalize_or_ancestor(p);
    roots.iter().any(|root| {
        let r = canonicalize_or_ancestor(root);
        p.starts_with(&r)
    })
}