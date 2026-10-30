use std::path::{Component, Path, PathBuf};

pub(crate) fn project_root() -> PathBuf {
    crate::config::project_root()
}

pub(crate) fn worktree_root(cwd: Option<&Path>) -> PathBuf {
    match cwd {
        Some(cwd) => canonicalize_or_ancestor(cwd),
        None => canonicalize_or_ancestor(&project_root()),
    }
}

pub(crate) fn resolve(file_path: &str, cwd: Option<&Path>) -> PathBuf {
    let p = PathBuf::from(file_path);
    if p.is_absolute() {
        return p;
    }
    let base = match cwd {
        Some(cwd) => canonicalize_or_ancestor(cwd),
        None => canonicalize_or_ancestor(&project_root()),
    };
    base.join(p)
}

pub(crate) fn normalize_dots(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

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
                        // canonicalize() walks past symlinks whose target
                        // doesn't exist yet. A dangling symlink pointing
                        // outside would pass containment as an ordinary
                        // name. Check each suffix prefix; if we find a
                        // symlink, resolve it and recurse so the real
                        // target is seen by the containment check.
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

pub(crate) fn normalize_path(p: &Path, cwd: Option<&Path>) -> Option<PathBuf> {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        let base = match cwd {
            Some(cwd) => canonicalize_or_ancestor(cwd),
            None => canonicalize_or_ancestor(&project_root()),
        };
        base.join(p)
    };
    let canonical = canonicalize_or_ancestor(&abs);
    // normalize_dots handles `.`/`..` in the non-existent suffix only —
    // canonicalize_or_ancestor already resolved them in the existing prefix.
    // Lexical `..` in a not-yet-existent suffix is the safe conservative choice.
    Some(normalize_dots(&canonical))
}

pub(crate) fn within_worktree(p: &Path, roots: &[PathBuf], cwd: Option<&Path>) -> bool {
    let p_norm = match normalize_path(p, cwd) {
        Some(n) => n,
        None => return false,
    };
    roots.iter().any(|root| match normalize_path(root, cwd) {
        Some(r) => p_norm.starts_with(&r),
        None => false,
    })
}

pub(crate) fn roots_suffix(roots: &[PathBuf], cwd: Option<&Path>) -> String {
    // Normalized, to match the paths the containment checks actually compare
    // against — raw roots read misleadingly under symlinked ancestors
    // (/var -> /private/var).
    let mut parts: Vec<String> = roots
        .iter()
        .filter_map(|r| normalize_path(r, cwd))
        .map(|r| r.display().to_string())
        .collect();
    parts.sort();
    parts.dedup();
    if parts.is_empty() {
        return String::new();
    }
    format!("\n→ sandbox roots: {}", parts.join(", "))
}

/// Lexical-leaf containment for the bash path-argument check ONLY.
///
/// Resolves symlinked ancestor *directories* (so `/var` -> `/private/var` and
/// any symlinked parent dir are judged by their real location) but does NOT
/// follow a final-component symlink. A venv interpreter is created as
/// `.venv/bin/python -> /opt/homebrew/.../python3.14`: the symlink node lives
/// inside the worktree even though its target does not. Executing it is not a
/// containment breach — bash can already run out-of-tree binaries via PATH
/// (git, cargo, ruff, make) and read/write anywhere via `python -c`. Likewise,
/// this permits leaf-symlink *reads* through bash (e.g. `cat somelink` where
/// `somelink -> /etc/passwd`) that `bash_check` previously denied. This is
/// acceptable because the real
/// data boundary is [`io_enforce`] on the Read/Write/Edit tools, which stays
/// fully canonical. This check is a usability guardrail: an early, clear denial
/// for obviously-stray path arguments, not a sandbox.
///
/// DO NOT "harden" this to canonicalize the final component. Doing so re-breaks
/// agent self-verify (`.venv/bin/python` becomes "path outside worktree") — the
/// exact regression from #1197 that removed `enforce_allows_venv_symlink_in_cwd`
/// — while adding zero real containment, because execution is not gated here.
/// See `bash_check_allows_venv_symlink_leaf`.
pub(crate) fn within_worktree_lexical(p: &Path, canonical_roots: &[PathBuf]) -> bool {
    let resolved = match (p.parent(), p.file_name()) {
        (Some(parent), Some(name)) => canonicalize_or_ancestor(parent).join(name),
        _ => canonicalize_or_ancestor(p),
    };
    let norm = normalize_dots(&resolved);
    canonical_roots.iter().any(|cr| norm.starts_with(cr))
}

pub(crate) fn expand_user(s: &str) -> String {
    if s == "~" || s.starts_with("~/") {
        let home = crate::config::home_dir();
        let home_str = home.to_string_lossy();
        // If home resolved to the sentinel fallback (no HOME set, no
        // platform dir), preserve ~ literally rather than expanding to ".".
        if home_str.is_empty() || home_str == "." {
            return s.to_string();
        }
        if s == "~" {
            return home_str.into_owned();
        }
        return format!("{home_str}{}", &s[1..]);
    }
    s.to_string()
}

pub(crate) fn enforce(roots: &[PathBuf], pth: &str, cwd: Option<&Path>) -> Option<String> {
    let p = resolve(pth, cwd);
    if !within_worktree(&p, roots, cwd) {
        return Some(format!(
            "Error: path outside sandbox: {pth}{}",
            roots_suffix(roots, cwd)
        ));
    }
    None
}

/// Symlink-aware containment check for file I/O. Uses
/// [`canonicalize_or_ancestor`] to resolve the real path, including dangling
/// symlinks that would otherwise escape containment. Returns Some(error) or
/// None if the path is safe.
///
/// Fail-closed by design: a root that cannot be fully canonicalized is
/// partially resolved via [`canonicalize_or_ancestor`] (existing prefix
/// canonicalized, non-existent suffix kept lexical) rather than being
/// dropped. If the path does not fall under any root `allowed` stays false
/// and the path is denied. This is stricter and safer than the old
/// single-root behavior, which returned `None` (allow) on canonicalization
/// failure — a "paranoid, shouldn't happen" branch that was effectively
/// fail-open.
pub(crate) fn io_enforce(path: &Path, roots: &[PathBuf]) -> Option<String> {
    let real = normalize_dots(&canonicalize_or_ancestor(path));
    let mut allowed = false;
    for root in roots {
        let real_root = normalize_dots(&canonicalize_or_ancestor(root));
        if real.starts_with(&real_root) {
            allowed = true;
            break;
        }
    }
    if !allowed {
        return Some(format!(
            "Error: path outside sandbox (resolved): {}{}",
            real.display(),
            roots_suffix(roots, None)
        ));
    }
    None
}
