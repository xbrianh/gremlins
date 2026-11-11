use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub(crate) mod python;
pub(crate) mod rust;

use python::PythonPackage;
use rust::RustPackage;

/// Supported source extensions.
const EXTENSIONS: &[&str] = &["rs", "py"];

/// A discovered package, either Rust or Python.
#[derive(Clone, Debug)]
pub enum PackageInfo {
    Rust(RustPackage),
    Python(PythonPackage),
}

impl PackageInfo {
    /// Package name suitable for display.
    pub fn name(&self) -> &str {
        match self {
            PackageInfo::Rust(r) => &r.pkg_name,
            PackageInfo::Python(p) => &p.name,
        }
    }

    /// Unique key that disambiguates packages across languages.
    /// Used to decide which dependency graphs to union for scoring.
    pub fn key(&self) -> String {
        match self {
            PackageInfo::Rust(r) => format!("rust:{}", r.pkg_name),
            PackageInfo::Python(p) => format!("python:{}", p.dir.display()),
        }
    }

    /// The directory where source files live (for source collection and
    /// directory enumeration).
    pub fn src_dir(&self) -> PathBuf {
        match self {
            PackageInfo::Rust(r) => r.dir.join("src"),
            PackageInfo::Python(p) => p.dir.clone(),
        }
    }
}

/// Return the set of recognized source-file extensions.
pub fn extensions() -> &'static [&'static str] {
    EXTENSIONS
}

/// Collect source files under `dir` matching any recognized extension.
pub fn collect_source_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_source_files_impl(dir, &mut files)?;
    Ok(files)
}

/// Directories that should never be recursed into for source collection
/// or package discovery.
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "__pycache__",
    "venv",
    ".venv",
    "env",
    ".env",
    "node_modules",
    "site-packages",
    "dist-packages",
    ".egg",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    "target",
];

fn should_skip_dir(name: &str) -> bool {
    name.starts_with('.') || SKIP_DIRS.contains(&name)
}

fn collect_source_files_impl(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            let skip = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(should_skip_dir);
            if !skip {
                collect_source_files_impl(&path, out)?;
            }
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| EXTENSIONS.contains(&e))
        {
            out.push(path);
        }
    }
    Ok(())
}

/// Discover all packages (Rust + Python) anchored at `base`.
/// Best-effort: failures for one language are non-fatal.
pub fn discover_packages(base: &Path) -> Vec<PackageInfo> {
    let mut packages = Vec::new();

    match rust::discover_rust_packages(base) {
        Ok(rust_pkgs) => packages.extend(rust_pkgs.into_iter().map(PackageInfo::Rust)),
        Err(e) => eprintln!("gremlins-drag: Rust package discovery skipped: {e}"),
    }

    match python::discover_python_packages(base) {
        Ok(py_pkgs) => packages.extend(py_pkgs.into_iter().map(PackageInfo::Python)),
        Err(e) => eprintln!("gremlins-drag: Python package discovery skipped: {e}"),
    }

    packages
}

/// Map a source file path back to its module identifier and the owning
/// package key.
pub fn file_to_module(file: &Path, packages: &[PackageInfo]) -> Option<(String, String)> {
    if let Some(result) = rust::file_to_rust_module(file, packages) {
        return Some(result);
    }
    python::file_to_python_module(file, packages)
}

/// Deduplicate `target_modules` so each module identifier appears only once.
///
/// When a workspace contains a library crate and a binary crate that share
/// the same target name (e.g. `gremlins` lib + `gremlins-cli` bin named
/// `gremlins`), both map to the same module identifier.  This function keeps
/// the library target over the binary and warns about each dropped file.
pub(crate) fn dedup_target_modules(
    target_modules: &mut Vec<(String, PathBuf, String)>,
    packages: &[PackageInfo],
) {
    // Map module name → index of the best entry seen so far.
    let mut best: HashMap<String, usize> = HashMap::new();
    let mut drop_indices: Vec<usize> = Vec::new();

    for (i, (module, _file, pkg_key)) in target_modules.iter().enumerate() {
        if let Some(&prev) = best.get(module) {
            let prev_is_bin = is_bin(&target_modules[prev].2, packages);
            let this_is_bin = is_bin(pkg_key, packages);
            if prev_is_bin && !this_is_bin {
                // Replace: keep this library entry, drop the previous binary.
                drop_indices.push(prev);
                best.insert(module.clone(), i);
            } else {
                // Keep the previous entry, drop this one.
                drop_indices.push(i);
            }
        } else {
            best.insert(module.clone(), i);
        }
    }

    // Remove in reverse order so earlier indices stay valid.
    drop_indices.sort_unstable_by(|a, b| b.cmp(a));
    for idx in drop_indices {
        let (_module, file, _pkg_key) = target_modules.remove(idx);
        eprintln!(
            "warning: skipping duplicate module from {} (already covered by another crate)",
            file.display()
        );
    }
}

/// Does `pkg_key` refer to a Rust binary target?
fn is_bin(pkg_key: &str, packages: &[PackageInfo]) -> bool {
    packages
        .iter()
        .any(|p| p.key() == pkg_key && matches!(p, PackageInfo::Rust(r) if r.is_bin))
}

/// Produce the language-specific dependency-graph edges for the given package.
pub fn dependency_graph(pkg: &PackageInfo) -> Result<Vec<(String, String)>, String> {
    match pkg {
        PackageInfo::Rust(r) => rust::rust_dependency_graph(r),
        PackageInfo::Python(p) => python::python_dependency_graph(&p.root_dir, &p.dir),
    }
}

/// Enumerate the source files that belong to a package's module tree.
///
/// For Rust, this is the crate's `src/` directory; for Python, the package
/// directory itself.  The caller maps each file back to a module with
/// [`file_to_module`] when building the full token-count map.
pub fn package_files(pkg: &PackageInfo) -> std::io::Result<Vec<PathBuf>> {
    match pkg {
        PackageInfo::Rust(r) => {
            let src_dir = r.dir.join("src");
            if src_dir.is_dir() {
                collect_source_files(&src_dir)
            } else {
                Ok(Vec::new())
            }
        }
        PackageInfo::Python(p) => collect_source_files(&p.dir),
    }
}

/// Return the code-fence language identifier for a source file.
pub fn code_fence(file: &Path) -> &'static str {
    match file.extension().and_then(|e| e.to_str()) {
        Some("py") => "python",
        _ => "rust",
    }
}

// ---------------------------------------------------------------------------
// Directory-segment extraction (for filesystem trie)
// ---------------------------------------------------------------------------

/// Map a source file to its directory segments relative to the package's
/// source root. Returns `None` when the file isn't inside the anchor dir.
///
/// These segments are used to build the filesystem-based drag tree. Each
/// segment represents one directory level in the source tree.
pub fn file_to_dir_segments(file: &Path, pkg: &PackageInfo) -> Option<Vec<String>> {
    match pkg {
        PackageInfo::Rust(r) => rust::file_to_rust_dir_segments(file, r),
        PackageInfo::Python(p) => python::file_to_python_dir_segments(file, p),
    }
}

/// Enumerate every directory under the package's source tree that
/// contains at least one recognized source file (transitively).
/// Returns absolute paths sorted for determinism.
pub fn enumerate_source_dirs(pkg: &PackageInfo) -> std::io::Result<Vec<PathBuf>> {
    let all_files = collect_source_files(&pkg.src_dir())?;
    let mut dir_set: HashSet<PathBuf> = HashSet::new();
    // Include the source root itself.
    dir_set.insert(pkg.src_dir());
    for f in &all_files {
        // Add the file's parent and every ancestor up to (and including)
        // the source root, so intermediate directories are scored too.
        let mut cur = f.parent();
        while let Some(dir) = cur {
            dir_set.insert(dir.to_path_buf());
            if dir == pkg.src_dir() {
                break;
            }
            cur = dir.parent();
        }
    }
    let mut dirs: Vec<PathBuf> = dir_set.into_iter().collect();
    dirs.sort();
    Ok(dirs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumerate_source_dirs_includes_intermediate_directories() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let pkg_dir = tmp.path().join("pkg");
        std::fs::create_dir_all(pkg_dir.join("a/b")).expect("create dirs");
        std::fs::write(pkg_dir.join("a/b/c.rs"), "// c").expect("write c.rs");
        std::fs::write(pkg_dir.join("top.py"), "# top").expect("write top.py");

        // Python package fixture: dir == src_dir == pkg_dir.
        let pkg = PackageInfo::Python(python::PythonPackage {
            name: "pkg".to_string(),
            root_dir: tmp.path().to_path_buf(),
            dir: pkg_dir.clone(),
        });

        let dirs = enumerate_source_dirs(&pkg).expect("enumerate");
        assert!(dirs.contains(&pkg_dir));
        assert!(dirs.contains(&pkg_dir.join("a")));
        assert!(dirs.contains(&pkg_dir.join("a/b")));
    }

    // ── dedup_target_modules ─────────────────────────────────────────

    fn rust_pkg(name: &str, target_name: &str, is_bin: bool) -> PackageInfo {
        PackageInfo::Rust(rust::RustPackage {
            dir: PathBuf::from(format!("/{name}")),
            pkg_name: name.to_string(),
            target_name: target_name.to_string(),
            is_bin,
        })
    }

    #[test]
    fn dedup_keeps_unique_modules() {
        let mut modules = vec![
            ("foo".into(), PathBuf::from("a/foo.rs"), "rust:a".into()),
            ("bar".into(), PathBuf::from("b/bar.rs"), "rust:b".into()),
        ];
        let packages = vec![rust_pkg("a", "a", false), rust_pkg("b", "b", false)];
        dedup_target_modules(&mut modules, &packages);
        assert_eq!(modules.len(), 2);
    }

    #[test]
    fn dedup_lib_beats_bin() {
        let mut modules = vec![
            (
                "mylib".into(),
                PathBuf::from("bin/main.rs"),
                "rust:mybin".into(),
            ),
            (
                "mylib".into(),
                PathBuf::from("lib/lib.rs"),
                "rust:mylib".into(),
            ),
        ];
        let packages = vec![
            rust_pkg("mybin", "mylib", true),
            rust_pkg("mylib", "mylib", false),
        ];
        dedup_target_modules(&mut modules, &packages);
        assert_eq!(modules.len(), 1);
        // The library entry should survive.
        assert!(modules[0].1.to_string_lossy().contains("lib.rs"));
    }

    #[test]
    fn dedup_first_wins_when_both_same_kind() {
        let mut modules = vec![
            ("dup".into(), PathBuf::from("first.rs"), "rust:a".into()),
            ("dup".into(), PathBuf::from("second.rs"), "rust:b".into()),
        ];
        let packages = vec![rust_pkg("a", "a", false), rust_pkg("b", "b", false)];
        dedup_target_modules(&mut modules, &packages);
        assert_eq!(modules.len(), 1);
        // First occurrence should survive when both are libraries.
        assert!(modules[0].1.to_string_lossy().contains("first.rs"));
    }
}
