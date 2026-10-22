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
    //
    // The discovered manifest may be the workspace root or a member package's
    // Cargo.toml; either is a valid anchor because `cargo metadata` walks up
    // to the workspace root from there.
    let manifest = match find_cargo_toml(base) {
        Some(m) => m,
        None => return Ok(Vec::new()),
    };

    let output = cargo_metadata_command(&manifest)
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

/// Build the `cargo metadata` command anchored at `manifest_path`.
fn cargo_metadata_command(manifest_path: &Path) -> Command {
    let mut cmd = Command::new("cargo");
    cmd.args([
        "metadata",
        "--format-version",
        "1",
        "--no-deps",
        "--manifest-path",
    ])
    .arg(manifest_path);
    cmd
}

// ---------------------------------------------------------------------------
// cargo-modules dependency graph
// ---------------------------------------------------------------------------

/// Build the `cargo modules dependencies` command anchored at `manifest_path`.
fn cargo_modules_command(pkg_name: &str, manifest_path: &Path) -> Command {
    let mut cmd = Command::new("cargo");
    cmd.args([
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
    .args(["-p", pkg_name]);
    cmd
}

fn run_cargo_modules_dependencies(pkg_name: &str, manifest_path: &Path) -> Result<String, String> {
    let output = cargo_modules_command(pkg_name, manifest_path)
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
    use std::ffi::OsStr;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, MutexGuard};

    // -----------------------------------------------------------------------
    // parse_dot
    // -----------------------------------------------------------------------

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

    // -----------------------------------------------------------------------
    // file_rel_to_module
    // -----------------------------------------------------------------------

    #[test]
    fn file_rel_to_module_normalizes_hyphens() {
        assert_eq!(
            file_rel_to_module(Path::new("main.rs"), "gremlins-drag", true),
            Some("gremlins_drag".to_string())
        );
        assert_eq!(
            file_rel_to_module(Path::new("foo/bar.rs"), "my-crate", false),
            Some("my_crate::foo::bar".to_string())
        );
    }

    #[test]
    fn file_rel_to_module_handles_mod_rs_and_roots() {
        assert_eq!(
            file_rel_to_module(Path::new("foo/mod.rs"), "crate", false),
            Some("crate::foo".to_string())
        );
        assert_eq!(
            file_rel_to_module(Path::new("lib.rs"), "crate", false),
            Some("crate".to_string())
        );
    }

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
    fn find_cargo_toml_finds_nearest_manifest_in_workspace() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        fs::write(
            tmp.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/*\"]\n",
        )
        .expect("write workspace Cargo.toml");

        // Create a member crate with its own Cargo.toml.
        let member_dir = tmp.path().join("crates/foo");
        fs::create_dir_all(member_dir.join("src")).expect("create member src");
        fs::write(member_dir.join("Cargo.toml"), "[package]\nname = \"foo\"\n")
            .expect("write member Cargo.toml");

        // From the member's src dir, find_cargo_toml should return the
        // nearest ancestor manifest — the member's own Cargo.toml, not the
        // workspace root above it.
        let file = member_dir.join("src/lib.rs");
        fs::write(&file, "// test").expect("write lib.rs");

        let found = find_cargo_toml(&file);
        assert_eq!(
            found.as_deref(),
            Some(member_dir.join("Cargo.toml").as_path())
        );
    }

    // -----------------------------------------------------------------------
    // cargo command construction
    // -----------------------------------------------------------------------

    #[test]
    fn cargo_metadata_command_passes_manifest_path() {
        let manifest = Path::new("/tmp/example/Cargo.toml");
        let cmd = cargo_metadata_command(manifest);
        let args: Vec<&OsStr> = cmd.get_args().collect();
        assert_eq!(
            args,
            vec![
                OsStr::new("metadata"),
                OsStr::new("--format-version"),
                OsStr::new("1"),
                OsStr::new("--no-deps"),
                OsStr::new("--manifest-path"),
                manifest.as_os_str(),
            ]
        );
    }

    #[test]
    fn cargo_modules_command_passes_manifest_path_and_package() {
        let manifest = Path::new("/tmp/example/Cargo.toml");
        let cmd = cargo_modules_command("mylib", manifest);
        let args: Vec<&OsStr> = cmd.get_args().collect();
        assert_eq!(
            args,
            vec![
                OsStr::new("modules"),
                OsStr::new("dependencies"),
                OsStr::new("--no-externs"),
                OsStr::new("--no-sysroot"),
                OsStr::new("--no-owns"),
                OsStr::new("--no-fns"),
                OsStr::new("--no-traits"),
                OsStr::new("--no-types"),
                OsStr::new("--manifest-path"),
                manifest.as_os_str(),
                OsStr::new("-p"),
                OsStr::new("mylib"),
            ]
        );
    }

    // -----------------------------------------------------------------------
    // discover_rust_packages with --manifest-path
    // -----------------------------------------------------------------------

    /// Process-wide CWD changes affect every test thread, so serialize them
    /// and always restore the previous directory on drop — even on panic.
    static CWD_LOCK: Mutex<()> = Mutex::new(());

    struct CwdGuard {
        prev: PathBuf,
        _lock: MutexGuard<'static, ()>,
    }

    impl CwdGuard {
        fn enter(dir: &Path) -> Self {
            let lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::current_dir().expect("current_dir");
            std::env::set_current_dir(dir).expect("set cwd");
            CwdGuard { prev, _lock: lock }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.prev);
        }
    }

    /// Regression: `discover_rust_packages` must pass `--manifest-path` to
    /// `cargo metadata` so discovery works even when CWD is outside the
    /// workspace. `cargo_metadata_command_passes_manifest_path` verifies the
    /// flag construction directly; this test exercises the full discovery
    /// path from a foreign CWD.
    #[test]
    fn discover_rust_packages_uses_manifest_path_from_outside_workspace() {
        let fixture = tempfile::TempDir::new().expect("tempdir");
        fs::write(
            fixture.path().join("Cargo.toml"),
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
        fs::write(lib_dir.join("src/lib.rs"), "pub fn hello() {}\n").expect("write lib.rs");

        let outside = tempfile::TempDir::new().expect("outside tempdir");
        let _cwd = CwdGuard::enter(outside.path());

        // discover_rust_packages receives a path inside the fixture. It must
        // find the manifest via find_cargo_toml and pass --manifest-path to
        // cargo metadata so the invocation does not depend on the process CWD.
        let packages = discover_rust_packages(&lib_dir.join("src/lib.rs"))
            .expect("discover_rust_packages should succeed");

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
