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
        PackageInfo::Rust(r) => {
            // `r.dir` is the canonicalized directory containing the package's
            // Cargo.toml. `cargo modules` resolves the workspace by walking up
            // from that manifest, so the package manifest is a valid anchor
            // even when the process CWD is outside the workspace.
            let manifest_path = r.dir.join("Cargo.toml");
            rust::rust_dependency_graph(&r.pkg_name, &manifest_path)
        }
        PackageInfo::Python(p) => python::python_dependency_graph(&p.root_dir, &p.dir),
    }
}

/// Return the code-fence language identifier for a source file.
pub fn code_fence(file: &Path) -> &'static str {
    match file.extension().and_then(|e| e.to_str()) {
        Some("py") => "python",
        _ => "rust",
    }
}
