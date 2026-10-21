use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A Python package discovered on disk.
#[derive(Clone, Debug)]
pub struct PythonPackage {
    /// Display name (directory name containing __init__.py).
    pub name: String,
    /// Topmost ancestor directory *without* `__init__.py`.
    /// This is the root from which module paths are computed.
    pub root_dir: PathBuf,
    /// The directory containing `__init__.py`.
    pub dir: PathBuf,
}

// ---------------------------------------------------------------------------
// Package discovery
// ---------------------------------------------------------------------------

/// Walk from `base` for Python packages.
///
/// A package is any directory containing `__init__.py`. Discovery starts from
/// the nearest ancestor of `base` that itself is *not* a Python package
/// (i.e. lacks `__init__.py`). This ensures the root used for module-path
/// computation agrees with the embedded extractor's root.
pub(super) fn discover_python_packages(base: &Path) -> Result<Vec<PythonPackage>, String> {
    let start = if base.is_dir() {
        base.to_path_buf()
    } else {
        base.parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| base.to_path_buf())
    };
    let start = start
        .canonicalize()
        .map_err(|e| format!("canonicalize base: {e}"))?;

    // Walk up from the start to find the nearest non-package ancestor.
    let discovery_root = find_nearest_non_package_ancestor(&start);

    let mut packages = Vec::new();
    find_python_packages(&discovery_root, &discovery_root, &mut packages);
    // Deduplicate by root_dir + name (a package may be found via multiple paths).
    dedup_packages(&mut packages);
    Ok(packages)
}

/// Walk up from `dir` until a directory without `__init__.py` is found.
fn find_nearest_non_package_ancestor(dir: &Path) -> PathBuf {
    let mut current = dir.to_path_buf();
    loop {
        if !current.join("__init__.py").is_file() {
            return current;
        }
        if !current.pop() {
            return dir.to_path_buf();
        }
    }
}

/// Recursively find Python packages. `root` is the nearest ancestor that
/// does *not* contain `__init__.py`.
fn find_python_packages(dir: &Path, root: &Path, out: &mut Vec<PythonPackage>) {
    let init = dir.join("__init__.py");
    let is_pkg = init.is_file();

    // If this directory is not a package, it can serve as the root for the
    // packages below it. If it *is* a package, the root stays unchanged so
    // nested packages still resolve against the topmost non-package ancestor.
    let child_root = if is_pkg { root } else { dir };

    if is_pkg {
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        out.push(PythonPackage {
            name,
            root_dir: root.to_path_buf(),
            dir: dir.to_path_buf(),
        });
    }

    // Recurse into subdirectories.
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if super::should_skip_dir(name) {
                    continue;
                }
            }
            find_python_packages(&path, child_root, out);
        }
    }
}

/// Remove duplicates by (root_dir, name). Keep the one with the shortest dir
/// (closest to root).
fn dedup_packages(packages: &mut Vec<PythonPackage>) {
    let mut keep = vec![true; packages.len()];
    let mut by_key: BTreeMap<(PathBuf, String), (usize, usize)> = BTreeMap::new(); // (index, dir_len)
    for (i, pkg) in packages.iter().enumerate() {
        let key = (pkg.root_dir.clone(), pkg.name.clone());
        let dir_len = pkg.dir.as_os_str().len();
        match by_key.get(&key) {
            Some(&(other_i, other_len)) => {
                if dir_len < other_len {
                    keep[other_i] = false;
                    by_key.insert(key, (i, dir_len));
                } else {
                    keep[i] = false;
                }
            }
            None => {
                by_key.insert(key, (i, dir_len));
            }
        }
    }

    let mut out = Vec::new();
    for (i, pkg) in packages.drain(..).enumerate() {
        if keep[i] {
            out.push(pkg);
        }
    }
    *packages = out;
}

// ---------------------------------------------------------------------------
// File → module mapping
// ---------------------------------------------------------------------------

/// Map a `.py` file to its dotted module path, given the known Python packages.
/// Returns `(module_path, pkg_key)`.
pub(super) fn file_to_python_module(
    file: &Path,
    packages: &[super::PackageInfo],
) -> Option<(String, String)> {
    // Only consider .py files.
    if file.extension().and_then(|e| e.to_str()) != Some("py") {
        return None;
    }

    for pkg in packages {
        if let super::PackageInfo::Python(ppkg) = pkg {
            // The file must live inside this specific package directory. This
            // keeps sibling packages that share a `root_dir` (e.g. a `src/`
            // layout) from claiming each other's files.
            if file.strip_prefix(&ppkg.dir).is_err() {
                continue;
            }
            let rel = file.strip_prefix(&ppkg.root_dir).ok()?;
            let key = format!("python:{}", ppkg.dir.display());
            return Some((rel_to_module(rel, &ppkg.name)?, key));
        }
    }
    None
}

/// Convert a root-relative Python file path to a dotted module path.
fn rel_to_module(rel: &Path, pkg_name: &str) -> Option<String> {
    let rel_str = rel.to_string_lossy();
    let stripped = rel_str.strip_suffix(".py")?;

    // __init__.py → parent module
    if let Some(parent) = stripped.strip_suffix("/__init__") {
        if parent.is_empty() {
            return Some(pkg_name.to_string());
        }
        return Some(parent.replace('/', "."));
    }

    // Handle the pathological case where the package root itself is the
    // working directory (rel has no package-name segment).
    if stripped == "__init__" {
        return Some(pkg_name.to_string());
    }

    let dotted = stripped.replace('/', ".");

    // When root_dir != dir (e.g. src/ layout), the rel already starts with
    // the package name. When root_dir == dir (flat layout), it doesn't.
    if !dotted.starts_with(&format!("{pkg_name}.")) && dotted != pkg_name {
        return Some(format!("{pkg_name}.{dotted}"));
    }
    Some(dotted)
}

// ---------------------------------------------------------------------------
// Dependency graph via python3 + ast
// ---------------------------------------------------------------------------

/// The embedded Python script that extracts import edges.
const PY_IMPORT_SCRIPT: &str = r#"
import ast
import json
import os
import sys

# Directories to skip during os.walk.
_SKIP_DIRS = {
    '.git', '.hg', '.svn', '__pycache__',
    'venv', '.venv', 'env', '.env',
    'node_modules', 'site-packages', 'dist-packages',
    '.egg', '.tox', '.mypy_cache', '.pytest_cache', '.ruff_cache',
    'target',
}

def module_for_file(filepath, pkg_root):
    """Convert a file path to a dotted module path."""
    rel = os.path.relpath(filepath, pkg_root)
    if rel.endswith('.py'):
        rel = rel[:-3]
    parts = rel.replace(os.sep, '/').split('/')
    if parts and parts[-1] == '__init__':
        parts.pop()
    if not parts:
        return None
    return '.'.join(parts)

def package_of(module, is_pkg):
    """Return the `__package__`-style package for a module."""
    if is_pkg:
        return module
    if '.' in module:
        return module.rsplit('.', 1)[0]
    return ''

def relative_anchor(src_mod, src_is_pkg, level):
    """Resolve the anchor module for a `level`-deep relative import."""
    pkg = package_of(src_mod, src_is_pkg)
    parts = pkg.split('.') if pkg else []
    if level > len(parts) + 1:
        return None  # beyond top-level
    if level > 1:
        parts = parts[:-(level - 1)]
    return '.'.join(parts) if parts else None

def join_target(anchor, target):
    if anchor:
        return anchor + '.' + target if target else anchor
    return target

def add_edge(edges, src, targets, known):
    """Add one edge per resolved target, plus the base module itself."""
    for base, names in targets:
        for name in names:
            target = join_target(base, name)
            if target in known:
                edges.add((src, target))
        # Always add edge to base when base is known — `from pkg import
        # thing` where `thing` is an attribute still depends on `pkg`.
        if base and base in known:
            edges.add((src, base))

def extract_edges(pkg_root, pkg_dir):
    """Walk pkg_dir and extract all import edges."""
    edges = set()
    known = {}
    for dirpath, dirnames, filenames in os.walk(pkg_dir):
        dirnames[:] = [d for d in dirnames if d not in _SKIP_DIRS and not d.startswith('.')]
        for fname in filenames:
            if fname.endswith('.py'):
                fpath = os.path.join(dirpath, fname)
                mod = module_for_file(fpath, pkg_root)
                if mod:
                    known[fpath] = (mod, fname == '__init__.py')
    known_mods = {mod for mod, _ in known.values()}

    for fpath, (src_mod, src_is_pkg) in known.items():
        try:
            with open(fpath, 'r', encoding='utf-8') as f:
                tree = ast.parse(f.read(), filename=fpath)
        except (SyntaxError, OSError, UnicodeError):
            continue

        for node in ast.walk(tree):
            if isinstance(node, ast.Import):
                names = [alias.name for alias in node.names]
                for name in names:
                    # The imported dotted path is the dependency if we know it.
                    if name in known_mods:
                        edges.add((src_mod, name))
                    # Also add parent packages (e.g. `a.b` for `import a.b.c`)
                    # to be symmetric with `from x import y` which adds the
                    # base module via `add_edge`.
                    while '.' in name:
                        name = name.rsplit('.', 1)[0]
                        if name in known_mods:
                            edges.add((src_mod, name))
            elif isinstance(node, ast.ImportFrom):
                names = [
                    alias.name
                    for alias in node.names
                    if alias.name != '*'
                ]
                if node.level == 0:
                    add_edge(edges, src_mod, [(node.module, names)], known_mods)
                else:
                    anchor = relative_anchor(src_mod, src_is_pkg, node.level)
                    if anchor is None:
                        continue
                    if node.module is not None:
                        base = join_target(anchor, node.module)
                    else:
                        base = anchor
                    add_edge(edges, src_mod, [(base, names)], known_mods)

    return sorted([list(e) for e in edges])

if __name__ == '__main__':
    pkg_root = sys.argv[1]
    pkg_dir = sys.argv[2]
    edges = extract_edges(pkg_root, pkg_dir)
    json.dump(edges, sys.stdout)
"#;

/// Shell out to python3 to extract import edges for a Python package.
pub(super) fn python_dependency_graph(
    pkg_root: &Path,
    pkg_dir: &Path,
) -> Result<Vec<(String, String)>, String> {
    let output = Command::new("python3")
        .arg("-c")
        .arg(PY_IMPORT_SCRIPT)
        .arg(pkg_root)
        .arg(pkg_dir)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "python3 not found. Install Python 3 to analyze Python packages.".into()
            } else {
                format!("failed to run python3: {e}")
            }
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("python3 import extraction failed: {stderr}"));
    }

    let stdout =
        String::from_utf8(output.stdout).map_err(|e| format!("invalid UTF-8 from python3: {e}"))?;

    let raw: Vec<[String; 2]> =
        serde_json::from_str(&stdout).map_err(|e| format!("python3 output parse: {e}"))?;

    Ok(raw.into_iter().map(|[src, dst]| (src, dst)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn py_pkg(name: &str, root: &Path, dir: &Path) -> crate::lang::PackageInfo {
        crate::lang::PackageInfo::Python(PythonPackage {
            name: name.to_string(),
            root_dir: root.to_path_buf(),
            dir: dir.to_path_buf(),
        })
    }

    #[test]
    fn rel_to_module_maps_package_files() {
        assert_eq!(
            rel_to_module(Path::new("pkg/core.py"), "pkg"),
            Some("pkg.core".to_string())
        );
        assert_eq!(
            rel_to_module(Path::new("pkg/utils/helpers.py"), "pkg"),
            Some("pkg.utils.helpers".to_string())
        );
    }

    #[test]
    fn rel_to_module_flat_layout_no_prefix() {
        // When root_dir == dir, rel does not include the package name.
        assert_eq!(
            rel_to_module(Path::new("core.py"), "mypkg"),
            Some("mypkg.core".to_string())
        );
        assert_eq!(
            rel_to_module(Path::new("utils/helpers.py"), "mypkg"),
            Some("mypkg.utils.helpers".to_string())
        );
    }

    #[test]
    fn rel_to_module_handles_dunder_files() {
        assert_eq!(
            rel_to_module(Path::new("pkg/__init__.py"), "pkg"),
            Some("pkg".to_string())
        );
        assert_eq!(
            rel_to_module(Path::new("pkg/__main__.py"), "pkg"),
            Some("pkg.__main__".to_string())
        );
        assert_eq!(
            rel_to_module(Path::new("pkg/utils/__init__.py"), "pkg"),
            Some("pkg.utils".to_string())
        );
    }

    #[test]
    fn file_to_python_module_selects_owning_package() {
        let root = Path::new("src");
        let packages = vec![
            py_pkg("pkg1", root, &root.join("pkg1")),
            py_pkg("pkg2", root, &root.join("pkg2")),
        ];

        assert_eq!(
            file_to_python_module(&root.join("pkg2").join("mod.py"), &packages),
            Some((
                "pkg2.mod".to_string(),
                format!("python:{}", root.join("pkg2").display())
            ))
        );
        assert_eq!(
            file_to_python_module(&root.join("pkg1").join("core.py"), &packages),
            Some((
                "pkg1.core".to_string(),
                format!("python:{}", root.join("pkg1").display())
            ))
        );
        assert_eq!(
            file_to_python_module(&root.join("pkg1").join("__init__.py"), &packages),
            Some((
                "pkg1".to_string(),
                format!("python:{}", root.join("pkg1").display())
            ))
        );
        assert_eq!(
            file_to_python_module(&root.join("pkg1").join("__main__.py"), &packages),
            Some((
                "pkg1.__main__".to_string(),
                format!("python:{}", root.join("pkg1").display())
            ))
        );
    }

    #[test]
    fn import_extractor_absolute_imports() {
        // Run the embedded Python script on a small fixture.
        let tmp = std::env::temp_dir().join("drag_py_test_abs");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("mypkg")).unwrap();
        std::fs::write(
            tmp.join("mypkg").join("__init__.py"),
            "from mypkg.core import run\n",
        )
        .unwrap();
        std::fs::write(tmp.join("mypkg").join("core.py"), "import mypkg.utils\n").unwrap();
        std::fs::write(tmp.join("mypkg").join("utils.py"), "# leaf module\n").unwrap();

        let edges = python_dependency_graph(&tmp, &tmp.join("mypkg")).unwrap();

        // mypkg → mypkg.core (from mypkg/__init__.py: `from mypkg.core import run`)
        assert!(edges.contains(&("mypkg".to_string(), "mypkg.core".to_string())));
        // mypkg.core → mypkg.utils (from mypkg/core.py: `import mypkg.utils`)
        assert!(edges.contains(&("mypkg.core".to_string(), "mypkg.utils".to_string())));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn import_extractor_relative_imports() {
        let tmp = std::env::temp_dir().join("drag_py_test_rel");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("pkg").join("sub")).unwrap();
        std::fs::write(tmp.join("pkg").join("__init__.py"), "").unwrap();
        std::fs::write(
            tmp.join("pkg").join("sub").join("__init__.py"),
            "from . import helper\n",
        )
        .unwrap();
        std::fs::write(
            tmp.join("pkg").join("sub").join("helper.py"),
            "from .. import core\n",
        )
        .unwrap();
        std::fs::write(tmp.join("pkg").join("core.py"), "# top-level\n").unwrap();

        let edges = python_dependency_graph(&tmp, &tmp.join("pkg")).unwrap();

        // pkg.sub → pkg.sub.helper (relative import: `from . import helper`)
        assert!(edges.contains(&("pkg.sub".to_string(), "pkg.sub.helper".to_string())));
        // pkg.sub.helper → pkg.core (relative import: `from .. import core`)
        assert!(edges.contains(&("pkg.sub.helper".to_string(), "pkg.core".to_string())));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
