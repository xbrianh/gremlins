use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

/// A workspace package we can analyze: its directory, Cargo.toml name, and the
/// Rust crate name that `cargo modules` emits.
#[derive(Clone, Debug)]
pub struct RustPackage {
    /// Directory containing Cargo.toml.
    pub dir: PathBuf,
    /// The `[package].name` in Cargo.toml.
    pub pkg_name: String,
    /// The Rust crate name (with `-` normalized to `_`) that cargo-modules emits.
    pub target_name: String,
    /// Whether this is a bin target (affects root file: main.rs vs lib.rs).
    pub is_bin: bool,
}

// ---------------------------------------------------------------------------
// Cargo metadata types (partial — only what we need)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CargoMetadata {
    packages: Vec<CargoPackage>,
}

#[derive(Deserialize)]
struct CargoPackage {
    name: String,
    manifest_path: String,
    targets: Vec<CargoTarget>,
}

#[derive(Deserialize)]
struct CargoTarget {
    name: String,
    kind: Vec<String>,
}

// ---------------------------------------------------------------------------
// Package discovery
// ---------------------------------------------------------------------------

pub(super) fn discover_rust_packages() -> Result<Vec<RustPackage>, String> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()
        .map_err(|e| format!("failed to run cargo metadata: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("cargo metadata failed: {stderr}"));
    }

    let meta: CargoMetadata =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("cargo metadata parse: {e}"))?;

    let mut packages = Vec::new();
    for pkg in &meta.packages {
        let lib_target = pkg
            .targets
            .iter()
            .find(|t| t.kind.iter().any(|k| k == "lib" || k == "rlib"));
        let bin_target = pkg
            .targets
            .iter()
            .find(|t| t.kind.iter().any(|k| k == "bin"));

        let (target_name, is_bin) = if let Some(t) = lib_target {
            (t.name.clone(), false)
        } else if let Some(t) = bin_target {
            (t.name.clone(), true)
        } else {
            continue;
        };

        let dir = Path::new(&pkg.manifest_path)
            .parent()
            .ok_or_else(|| format!("no parent for manifest: {}", pkg.manifest_path))?
            .canonicalize()
            .map_err(|e| format!("failed to canonicalize {}: {e}", pkg.manifest_path))?;

        packages.push(RustPackage {
            dir,
            pkg_name: pkg.name.clone(),
            // cargo-modules emits Rust crate names, where `-` becomes `_`.
            target_name: target_name.replace('-', "_"),
            is_bin,
        });
    }

    Ok(packages)
}

// ---------------------------------------------------------------------------
// cargo-modules dependency graph
// ---------------------------------------------------------------------------

fn run_cargo_modules_dependencies(pkg_name: &str) -> Result<String, String> {
    let output = Command::new("cargo")
        .args([
            "modules",
            "dependencies",
            "--no-externs",
            "--no-sysroot",
            "--no-owns",
            "--no-fns",
            "--no-traits",
            "--no-types",
            "-p",
            pkg_name,
        ])
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "cargo-modules not found. Install with: cargo install cargo-modules".into()
            } else {
                format!("failed to run cargo modules: {e}")
            }
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("cargo modules failed for {pkg_name}: {stderr}"));
    }

    String::from_utf8(output.stdout).map_err(|e| format!("invalid UTF-8 from cargo modules: {e}"))
}

/// Produce the dependency-graph edges for a Rust package.
pub(super) fn rust_dependency_graph(pkg_name: &str) -> Result<Vec<(String, String)>, String> {
    let dot = run_cargo_modules_dependencies(pkg_name)?;
    Ok(parse_dot(&dot))
}

/// Parse DOT output, returning the list of dependency edges.
fn parse_dot(dot: &str) -> Vec<(String, String)> {
    let mut edges = Vec::new();

    for line in dot.lines() {
        let line = line.trim();

        // Edge: "src" -> "dst" [label="uses", ...];
        if let Some(rest) = line.strip_prefix('"') {
            if let Some(src_end) = rest.find('"') {
                let src = &rest[..src_end];
                let after_src = &rest[src_end + 1..];
                if let Some(after_arrow) = after_src.trim_start().strip_prefix("->") {
                    if let Some(dst_quoted) = after_arrow.trim_start().strip_prefix('"') {
                        if let Some(dst_end) = dst_quoted.find('"') {
                            let dst = &dst_quoted[..dst_end];
                            edges.push((src.to_string(), dst.to_string()));
                        }
                    }
                }
            }
        }
    }

    edges
}

// ---------------------------------------------------------------------------
// File → module mapping
// ---------------------------------------------------------------------------

/// Map a source file path back to its cargo-modules node name and the
/// owning package key. Returns `(module_name, pkg_key)`.
pub(super) fn file_to_rust_module(
    file: &Path,
    packages: &[super::PackageInfo],
) -> Option<(String, String)> {
    for pkg in packages {
        if let super::PackageInfo::Rust(rpkg) = pkg {
            let src_dir = rpkg.dir.join("src");
            if let Ok(rel) = file.strip_prefix(&src_dir) {
                let key = format!("rust:{}", rpkg.pkg_name);
                return Some((
                    file_rel_to_module(rel, &rpkg.target_name, rpkg.is_bin)?,
                    key,
                ));
            }
        }
    }
    None
}

fn file_rel_to_module(rel: &Path, target_name: &str, is_bin: bool) -> Option<String> {
    // cargo-modules emits Rust crate names, where `-` is replaced by `_`.
    let crate_name = target_name.replace('-', "_");
    let rel_str = rel.to_string_lossy();

    // Handle crate root files.
    if rel_str == "lib.rs" && !is_bin {
        return Some(crate_name);
    }
    if rel_str == "main.rs" && is_bin {
        return Some(crate_name);
    }

    // Strip .rs suffix.
    let stripped = rel_str.strip_suffix(".rs")?;

    // Handle mod.rs: <segments>/mod.rs → <segments>
    if let Some(parent) = stripped.strip_suffix("/mod") {
        let module_path = parent.replace('/', "::");
        return Some(format!("{crate_name}::{module_path}"));
    }

    // Regular file: <segments>.rs → <segments>
    let module_path = stripped.replace('/', "::");
    Some(format!("{crate_name}::{module_path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dependency_edges() {
        let dot = "\"gremlins_drag\" [label=\"...\"];\n\"gremlins_drag::foo\" -> \"gremlins_drag::bar\" [label=\"uses\"];\n\"gremlins_drag::bar\" -> \"gremlins_drag::baz\" [label=\"uses\"];";
        let edges = parse_dot(dot);
        assert_eq!(
            edges,
            vec![
                (
                    "gremlins_drag::foo".to_string(),
                    "gremlins_drag::bar".to_string()
                ),
                (
                    "gremlins_drag::bar".to_string(),
                    "gremlins_drag::baz".to_string()
                ),
            ]
        );
    }

    #[test]
    fn file_rel_to_module_normalizes_hyphens() {
        assert_eq!(
            file_rel_to_module(std::path::Path::new("main.rs"), "gremlins-drag", true),
            Some("gremlins_drag".to_string())
        );
        assert_eq!(
            file_rel_to_module(std::path::Path::new("foo/bar.rs"), "my-crate", false),
            Some("my_crate::foo::bar".to_string())
        );
    }

    #[test]
    fn file_rel_to_module_handles_mod_rs_and_roots() {
        assert_eq!(
            file_rel_to_module(std::path::Path::new("foo/mod.rs"), "crate", false),
            Some("crate::foo".to_string())
        );
        assert_eq!(
            file_rel_to_module(std::path::Path::new("lib.rs"), "crate", false),
            Some("crate".to_string())
        );
    }
}
