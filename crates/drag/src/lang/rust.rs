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

pub(super) fn discover_rust_packages(base: &Path) -> Result<Vec<RustPackage>, String> {
    // If there is no `Cargo.toml` anywhere up the tree, this isn't a Rust
    // project. Running `cargo metadata` would fail with a scary "could not
    // find Cargo.toml" message, so short-circuit to an empty package list
    // instead. This keeps pure-Python (and other non-Rust) projects quiet.
    let workspace_manifest = match find_cargo_toml(base) {
        Some(m) => m,
        None => return Ok(Vec::new()),
    };

    let output = Command::new("cargo")
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--manifest-path",
        ])
        .arg(&workspace_manifest)
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

/// Locate the nearest `Cargo.toml`, walking up from `from`.
/// Mirrors `cargo metadata`'s default manifest discovery.
fn find_cargo_toml(from: &Path) -> Option<PathBuf> {
    let mut dir = if from.is_dir() {
        from.to_path_buf()
    } else {
        from.parent()?.to_path_buf()
    };
    loop {
        let candidate = dir.join("Cargo.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

// ---------------------------------------------------------------------------
// cargo-modules dependency graph
// ---------------------------------------------------------------------------

fn run_cargo_modules_dependencies(pkg_name: &str, manifest_path: &Path) -> Result<String, String> {
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
            "--manifest-path",
        ])
        .arg(manifest_path)
        .args(["-p", pkg_name])
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
pub(super) fn rust_dependency_graph(
    pkg_name: &str,
    manifest_path: &Path,
) -> Result<Vec<(String, String)>, String> {
    let dot = run_cargo_modules_dependencies(pkg_name, manifest_path)?;
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
    use std::fs;

    // -----------------------------------------------------------------------
    // find_cargo_toml
    // -----------------------------------------------------------------------

    #[test]
    fn find_cargo_toml_walks_up_from_deep_file() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let deep = tmp.path().join("a/b/c/d");
        fs::create_dir_all(&deep).expect("create deep dirs");
        let manifest = tmp.path().join("Cargo.toml");
        fs::write(&manifest, "[workspace]\n").expect("write Cargo.toml");

        let file = deep.join("e.rs");
        fs::write(&file, "// test").expect("write test file");

        let found = find_cargo_toml(&file);
        assert_eq!(found.as_deref(), Some(manifest.as_path()));
    }

    #[test]
    fn find_cargo_toml_returns_none_when_no_manifest() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let deep = tmp.path().join("x/y");
        fs::create_dir_all(&deep).expect("create deep dirs");

        let file = deep.join("z.rs");
        fs::write(&file, "// test").expect("write test file");

        // The temp dir has no Cargo.toml anywhere in the hierarchy
        let found = find_cargo_toml(&file);
        assert!(found.is_none());
    }

    #[test]
    fn find_cargo_toml_finds_workspace_manifest_from_subdir() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let workspace_manifest = tmp.path().join("Cargo.toml");
        fs::write(&workspace_manifest, "[workspace]\nmembers = [\"crates/*\"]\n")
            .expect("write workspace Cargo.toml");

        // Create a member crate with its own Cargo.toml
        let member_dir = tmp.path().join("crates/foo");
        fs::create_dir_all(&member_dir).expect("create member dir");
        fs::write(member_dir.join("Cargo.toml"), "[package]\nname = \"foo\"\n")
            .expect("write member Cargo.toml");

        // From the member's src dir, find_cargo_toml should find the
        // member's own Cargo.toml (the nearest one)
        let src_dir = member_dir.join("src");
        fs::create_dir_all(&src_dir).expect("create src dir");
        let file = src_dir.join("lib.rs");
        fs::write(&file, "// test").expect("write lib.rs");

        let found = find_cargo_toml(&file);
        assert_eq!(found.as_deref(), Some(member_dir.join("Cargo.toml").as_path()));
    }

    // -----------------------------------------------------------------------
    // discover_rust_packages with --manifest-path
    // -----------------------------------------------------------------------

    /// Regression: `discover_rust_packages` must pass `--manifest-path` to
    /// `cargo metadata` so discovery works even when CWD is outside the
    /// workspace. This test creates a minimal Cargo workspace fixture, sets
    /// CWD to a temp dir outside it, and verifies that `discover_rust_packages`
    /// succeeds by using the manifest path found by `find_cargo_toml`.
    #[test]
    fn discover_rust_packages_uses_manifest_path_from_outside_workspace() {
        let fixture = tempfile::TempDir::new().expect("tempdir");
        let manifest = fixture.path().join("Cargo.toml");
        fs::write(
            &manifest,
            "[workspace]\nmembers = [\"mylib\"]\nresolver = \"2\"\n",
        )
        .expect("write workspace Cargo.toml");

        let lib_dir = fixture.path().join("mylib");
        fs::create_dir_all(lib_dir.join("src")).expect("create lib src");
        fs::write(
            lib_dir.join("Cargo.toml"),
            "[package]\nname = \"mylib\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("write lib Cargo.toml");
        fs::write(lib_dir.join("src/lib.rs"), "pub fn hello() {}\n")
            .expect("write lib.rs");

        // Save current dir and move to a temp dir outside the fixture
        let prev_cwd = std::env::current_dir().expect("current_dir");
        let outside = tempfile::TempDir::new().expect("outside tempdir");
        std::env::set_current_dir(outside.path()).expect("set cwd outside");

        // discover_rust_packages receives a path inside the fixture.
        // It must find the manifest via find_cargo_toml and pass
        // --manifest-path to cargo metadata.
        let result = discover_rust_packages(&lib_dir.join("src/lib.rs"));

        // Restore CWD before asserting (so test failures don't leave
        // the process in a stale directory).
        std::env::set_current_dir(&prev_cwd).expect("restore cwd");

        let packages = result.expect("discover_rust_packages should succeed");
        assert!(
            !packages.is_empty(),
            "should find at least one package; got empty list"
        );
        assert!(
            packages.iter().any(|p| p.pkg_name == "mylib"),
            "should contain the 'mylib' package"
        );
    }
}
