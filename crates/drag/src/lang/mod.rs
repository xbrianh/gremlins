use std::collections::HashSet;
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

    /// Package root directory for use as agent working directory.
    #[allow(dead_code)]
    pub fn dir(&self) -> &Path {
        match self {
            PackageInfo::Rust(r) => &r.dir,
            PackageInfo::Python(p) => &p.dir,
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
}
