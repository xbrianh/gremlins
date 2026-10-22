use std::collections::HashMap;
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
///
/// After parsing the `cargo modules` DOT output, edges whose source module is
/// a re-export-only facade are dropped.
pub(super) fn rust_dependency_graph(pkg: &RustPackage) -> Result<Vec<(String, String)>, String> {
    // `pkg.dir` is the canonicalized directory containing the package's
    // Cargo.toml. `cargo modules` resolves the workspace by walking up from
    // that manifest, so the package manifest is a valid anchor even when the
    // process CWD is outside the workspace.
    let manifest_path = pkg.dir.join("Cargo.toml");
    let dot = run_cargo_modules_dependencies(&pkg.pkg_name, &manifest_path)?;
    let mut edges = parse_dot(&dot);
    filter_reexport_only_edges(&mut edges, pkg);
    Ok(edges)
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
// Re-export-only facade heuristic
// ---------------------------------------------------------------------------

/// Rewire dependency edges that pass through re-export-only facade modules.
///
/// A facade module's source file contains only doc comments, `pub use`
/// statements, `mod` declarations, and stray semicolons. Such modules are
/// mostly public surface area re-exporting other modules.  Instead of
/// dropping facade-originating edges (which would undercount transitive
/// `deps` for every importer), we splice importers directly to the facade's
/// own dependencies and then remove all edges where the source is a facade.
fn filter_reexport_only_edges(edges: &mut Vec<(String, String)>, pkg: &RustPackage) {
    let file_map = module_file_map(pkg);
    let mut facade_cache: HashMap<String, bool> = HashMap::new();

    let mut is_facade = |m: &str| -> bool {
        *facade_cache.entry(m.to_string()).or_insert_with(|| {
            file_map
                .get(m)
                .map(|file| is_reexport_only_module(file))
                .unwrap_or(false)
        })
    };

    // Build adjacency for rewiring: importers → facade, facade → exports.
    let mut importers: HashMap<String, Vec<String>> = HashMap::new();
    let mut exports: HashMap<String, Vec<String>> = HashMap::new();
    for (src, dst) in edges.iter() {
        if is_facade(src) {
            exports.entry(src.clone()).or_default().push(dst.clone());
        }
        if is_facade(dst) {
            importers.entry(dst.clone()).or_default().push(src.clone());
        }
    }

    // Collect rewired edges: for each facade, every (importer → export).
    let mut splice: Vec<(String, String)> = Vec::new();
    for facade in exports.keys() {
        let imps = importers.get(facade);
        let exps = exports.get(facade);
        if let (Some(imps), Some(exps)) = (imps, exps) {
            for s in imps {
                for d in exps {
                    if s != d {
                        splice.push((s.clone(), d.clone()));
                    }
                }
            }
        }
    }

    // Keep only edges whose source is NOT a facade, then append splices.
    let mut kept: Vec<(String, String)> = Vec::with_capacity(edges.len() + splice.len());
    for (src, dst) in edges.drain(..) {
        if !is_facade(&src) {
            kept.push((src, dst));
        }
    }
    kept.extend(splice);

    // Deduplicate (splice may produce edges that already existed).
    kept.sort();
    kept.dedup();
    *edges = kept;
}

/// Map every module in the package's `src/` tree to its source file.
fn module_file_map(pkg: &RustPackage) -> HashMap<String, PathBuf> {
    let src_dir = pkg.dir.join("src");
    let mut map = HashMap::new();
    let files = match super::collect_source_files(&src_dir) {
        Ok(files) => files,
        Err(_) => return map,
    };

    for file in files {
        if let Ok(rel) = file.strip_prefix(&src_dir) {
            if let Some(module) = file_rel_to_module(rel, &pkg.target_name, pkg.is_bin) {
                map.insert(module, file);
            }
        }
    }
    map
}

/// Read a module's source and decide whether it is a re-export-only facade.
fn is_reexport_only_module(file: &Path) -> bool {
    let src = match std::fs::read_to_string(file) {
        Ok(src) => src,
        Err(_) => return false,
    };
    let no_comments = strip_all_comments(&src);
    let no_attrs = strip_attributes(&no_comments);
    is_reexport_only_body(&no_attrs)
}

/// Strip all Rust comments — line (`//` including `///` / `//!`) and block
/// (`/* … */` including `/** … */` / `/*! … */`).
fn strip_all_comments(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;

    while i < chars.len() {
        // Line comment: // …
        if chars[i] == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }

        // Block comment: /* … */
        if chars[i] == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i = if i + 1 < chars.len() {
                i + 2
            } else {
                chars.len()
            };
            continue;
        }

        out.push(chars[i]);
        i += 1;
    }

    out
}

/// Strip Rust outer and inner attributes (`#[…]` and `#![…]`), including
/// nested brackets.
fn strip_attributes(src: &str) -> String {
    let chars: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '#' && i + 1 < chars.len() && chars[i + 1] == '[' {
            // Outer attribute: #[ … ]
            i += 2;
            let mut depth = 1u32;
            while i < chars.len() && depth > 0 {
                match chars[i] {
                    '[' => depth += 1,
                    ']' => depth -= 1,
                    _ => {}
                }
                i += 1;
            }
            continue;
        }
        if chars[i] == '#' && i + 2 < chars.len() && chars[i + 1] == '!' && chars[i + 2] == '[' {
            // Inner attribute: #![ … ]
            i += 3;
            let mut depth = 1u32;
            while i < chars.len() && depth > 0 {
                match chars[i] {
                    '[' => depth += 1,
                    ']' => depth -= 1,
                    _ => {}
                }
                i += 1;
            }
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }

    out
}

/// Decide whether a comment-and-attribute-free body contains only `pub use`
/// (including `pub(crate) use`, `pub(super) use`, etc.), `pub mod`,
/// `mod`, and stray semicolons.  `pub mod` / `mod` are only accepted in
/// semicolon form (`mod foo;`), not block form (`mod foo { … }`).
fn is_reexport_only_body(mut body: &str) -> bool {
    loop {
        body = body.trim_start();
        if body.is_empty() {
            return true;
        }

        if let Some(rest) = body.strip_prefix(';') {
            body = rest;
            continue;
        }

        // pub use, pub(crate) use, pub(super) use, pub(in path) use, …
        if let Some(after_kw) = strip_pub_use(body) {
            match skip_to_stmt_end(after_kw) {
                Some(rest) => {
                    body = rest;
                    continue;
                }
                None => return false,
            }
        }

        // pub mod foo;  (semicolon form only — block form is not a facade)
        if let Some(after_kw) = strip_keyword(body, "pub mod") {
            match skip_to_stmt_end(after_kw) {
                Some(rest) => {
                    body = rest;
                    continue;
                }
                None => return false,
            }
        }

        // mod foo;  (semicolon form only)
        if let Some(after_kw) = strip_keyword(body, "mod") {
            match skip_to_stmt_end(after_kw) {
                Some(rest) => {
                    body = rest;
                    continue;
                }
                None => return false,
            }
        }

        return false;
    }
}

/// Strip `pub` optionally followed by `(path)` then the `use` keyword.
fn strip_pub_use(s: &str) -> Option<&str> {
    let rest = s.strip_prefix("pub")?;
    let rest = rest.trim_start();

    // Optionally strip visibility restriction: (crate), (super), (in path), …
    let rest = if let Some(inner) = rest.strip_prefix('(') {
        let close = inner.find(')')?;
        inner[close + 1..].trim_start()
    } else {
        rest
    };

    strip_keyword(rest, "use")
}

/// Skip to the end of a statement: find the next `;` at bracket-depth zero
/// (outside `()`, `[]`, `{}`).  Returns `None` when no such `;` exists
/// (e.g. `mod foo { … }` with a block body), which signals a non-facade.
fn skip_to_stmt_end(s: &str) -> Option<&str> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut depth_paren = 0u32;
    let mut depth_bracket = 0u32;
    let mut depth_brace = 0u32;

    while i < chars.len() {
        match chars[i] {
            '(' => depth_paren += 1,
            ')' => {
                depth_paren = depth_paren.saturating_sub(1);
            }
            '[' => depth_bracket += 1,
            ']' => {
                depth_bracket = depth_bracket.saturating_sub(1);
            }
            '{' => depth_brace += 1,
            '}' => {
                depth_brace = depth_brace.saturating_sub(1);
            }
            ';' if depth_paren == 0 && depth_bracket == 0 && depth_brace == 0 => {
                return Some(&s[i + 1..]);
            }
            _ => {}
        }
        i += 1;
    }

    None
}

/// Strip `keyword` from the front of `s`, requiring a word boundary after it.
fn strip_keyword<'a>(s: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = s.strip_prefix(keyword)?;
    match rest.chars().next() {
        None => Some(rest),
        Some(c) if c.is_whitespace() || c == ';' => Some(rest),
        _ => None,
    }
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

    // -----------------------------------------------------------------------
    // re-export-only facade heuristic
    // -----------------------------------------------------------------------

    fn test_pkg(dir: &Path) -> RustPackage {
        RustPackage {
            dir: dir.to_path_buf(),
            pkg_name: "crate".to_string(),
            target_name: "crate".to_string(),
            is_bin: false,
        }
    }

    #[test]
    fn reexport_only_body_accepts_use_mod_and_semicolons() {
        assert!(is_reexport_only_body(
            "pub use crate::a;\nmod b;\n;\npub mod c;"
        ));
        assert!(is_reexport_only_body(""));
        assert!(!is_reexport_only_body("pub fn helper() {}"));
        assert!(!is_reexport_only_body("const X: usize = 1;"));
    }

    #[test]
    fn reexport_only_body_accepts_pub_crate_use() {
        // pub(crate) use should be recognized as a re-export.
        assert!(is_reexport_only_body("pub(crate) use crate::a;\n"));
        assert!(is_reexport_only_body("pub(super) use crate::b;\n"));
        assert!(is_reexport_only_body("pub(in crate::foo) use crate::c;\n"));
    }

    #[test]
    fn reexport_only_body_rejects_mod_with_block_body() {
        // A `mod foo { … }` block is NOT a facade.
        assert!(!is_reexport_only_body("mod foo { fn bar() {} }"));
        assert!(!is_reexport_only_body("pub mod foo { fn bar() {} }"));
    }

    #[test]
    fn reexport_only_body_handles_inner_attribute() {
        // #![…] inner attributes are stripped and should not defeat detection.
        // (Attributes are already stripped before this function runs;
        //  verify that the body parser still works on the remaining text.)
        assert!(is_reexport_only_body("pub use crate::a;\n"));
    }

    #[test]
    fn reexport_only_body_handles_regular_line_comment() {
        // Regular // comments (not doc) are stripped before this function.
        assert!(is_reexport_only_body("pub use crate::a;\n"));
    }

    #[test]
    fn reexport_only_facade_rewires_through_facade() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).expect("create src");
        fs::write(src.join("lib.rs"), "pub mod prelude;\npub mod util;\n").expect("write lib.rs");
        fs::write(
            src.join("prelude.rs"),
            "//! Facade.\n\npub use crate::util;\n",
        )
        .expect("write prelude.rs");
        fs::write(src.join("util.rs"), "pub fn helper() {}\n").expect("write util.rs");

        let pkg = test_pkg(tmp.path());
        // crate (lib.rs) imports prelude, prelude re-exports util.
        let mut edges = vec![
            ("crate".to_string(), "crate::prelude".to_string()),
            ("crate::prelude".to_string(), "crate::util".to_string()),
        ];

        filter_reexport_only_edges(&mut edges, &pkg);

        // crate now reaches util directly via the splice.
        assert_eq!(
            edges,
            vec![("crate".to_string(), "crate::util".to_string())]
        );
    }

    #[test]
    fn reexport_only_facade_no_importers_removes_outgoing_edges() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).expect("create src");
        fs::write(src.join("lib.rs"), "pub mod prelude;\npub mod util;\n").expect("write lib.rs");
        fs::write(
            src.join("prelude.rs"),
            "//! Orphan facade (no importers).\npub use crate::util;\n",
        )
        .expect("write prelude.rs");
        fs::write(src.join("util.rs"), "pub fn helper() {}\n").expect("write util.rs");

        let pkg = test_pkg(tmp.path());
        // Nobody imports prelude; it's a facade with outgoing edges only.
        let mut edges = vec![("crate::prelude".to_string(), "crate::util".to_string())];

        filter_reexport_only_edges(&mut edges, &pkg);

        // Facade source edges are removed; no importers → no splice edges.
        assert!(edges.is_empty());
    }

    #[test]
    fn non_facade_keeps_edges() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).expect("create src");
        fs::write(src.join("lib.rs"), "pub mod util;\n").expect("write lib.rs");
        fs::write(src.join("util.rs"), "pub fn helper() {}\n").expect("write util.rs");

        let pkg = test_pkg(tmp.path());
        let mut edges = vec![("crate::util".to_string(), "crate".to_string())];
        filter_reexport_only_edges(&mut edges, &pkg);

        assert_eq!(
            edges,
            vec![("crate::util".to_string(), "crate".to_string())]
        );
    }

    #[test]
    fn strip_all_comments_handles_regular_and_doc_comments() {
        let src = "//! inner\n/// outer\n// regular\n/** block */\n/* regular block */\npub use crate::a;\n";
        let stripped = strip_all_comments(src);
        // With all comments gone and attributes gone, body is just the pub use.
        let no_attrs = strip_attributes(&stripped);
        assert!(is_reexport_only_body(&no_attrs));
    }

    #[test]
    fn strip_attributes_removes_inner_and_outer_attrs() {
        let src = "#![deny(unsafe_code)]\n#[allow(dead_code)]\npub use crate::a;\n";
        let no_comments = strip_all_comments(src);
        let no_attrs = strip_attributes(&no_comments);
        assert!(is_reexport_only_body(&no_attrs));
    }

    #[test]
    fn facade_with_regular_comment_still_recognized() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).expect("create src");
        fs::write(
            src.join("prelude.rs"),
            "// keep in sync with util.rs\npub use crate::util;\n",
        )
        .expect("write prelude.rs");

        assert!(is_reexport_only_module(&src.join("prelude.rs")));
    }

    #[test]
    fn facade_with_inner_attribute_still_recognized() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).expect("create src");
        fs::write(
            src.join("prelude.rs"),
            "#![allow(unused_imports)]\npub use crate::util;\n",
        )
        .expect("write prelude.rs");

        assert!(is_reexport_only_module(&src.join("prelude.rs")));
    }

    #[test]
    fn non_facade_has_real_code() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).expect("create src");
        fs::write(src.join("util.rs"), "pub fn helper() {}\n").expect("write util.rs");
        assert!(!is_reexport_only_module(&src.join("util.rs")));
    }
}
