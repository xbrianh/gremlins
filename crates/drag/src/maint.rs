// Package maintainability scoring for gremlins-drag.
//
// This module handles:
// - Building a landscape of a package directory.
// - Prompting an ephemeral Agent stage to score five dimensions:
//   decomposition, interface_design, consistency, change_isolation, duplication.
// - Caching the median score per package (one entry per scored package,
//   not per estimate).
// - Computing the package-level drag formula.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use gremlins::builders::{output, AgentBuilder, DefinitionBuilder};
use gremlins::executor::gremlin::{Gremlin, GremlinConfig};
use gremlins::model_uri::ModelURI;

use crate::lang::PackageInfo;
use crate::{median, truncate, CacheState, CompAgentError};

/// Maintainability scores (0-4 each) for five dimensions.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub(crate) struct PackageMaintainability {
    pub decomposition: u8,
    pub interface_design: u8,
    pub consistency: u8,
    pub change_isolation: u8,
    #[serde(default)]
    pub duplication: u8,
}

impl PackageMaintainability {
    pub(crate) fn maint_score(&self) -> f64 {
        let sum = self.decomposition as f64
            + self.interface_design as f64
            + self.consistency as f64
            + self.change_isolation as f64
            + self.duplication as f64;
        (sum / 20.0).clamp(0.0, 1.0)
    }
}

const MAX_MAINT_RETRIES: u32 = 3;
const MAINT_BACKOFF: [f64; 2] = [1.0, 2.0];

/// Handle for a single package maintainability scoring task.
/// Returns (pkg_key, score, MaintCacheEntry) on success.
type MaintTask = tokio::task::JoinHandle<Result<(String, f64, MaintCacheEntry), String>>;

/// One line in the maintainability cache file.
///
/// Unlike module-level comprehensibility (which stores one entry per estimate),
/// this stores one entry per *scored package* with the median score.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub(crate) struct MaintCacheEntry {
    /// `pkg_key || "\0" || blake3(landscape)` — hex, first 16 chars.
    pub key: String,
    pub package: String,
    pub model: String,
    pub estimates: usize,
    pub maint_score: f64,
    pub breakdown: PackageMaintainability,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Score package maintainability for each scoring package.
/// Returns a map from package key to maintainability score (0–1).
pub(crate) async fn score_package_maintainability(
    model_spec: &str,
    verbose: bool,
    estimates: usize,
    scoring_packages: &[&PackageInfo],
    cache_state: CacheState,
) -> Result<HashMap<String, f64>, String> {
    if scoring_packages.is_empty() {
        return Ok(HashMap::new());
    }

    // Resolve maintainability cache path.
    let maint_cache_path = if let Some(parent) = cache_state.parent() {
        parent.join("maint-cache.jsonl")
    } else {
        PathBuf::from("maint-cache.jsonl")
    };

    let cache_map = load_maint_cache(&maint_cache_path)?;
    eprintln!(
        "Loaded {} cached maintainability entries from {}",
        cache_map.len(),
        maint_cache_path.display()
    );

    eprintln!(
        "Scoring package maintainability for {} packages...",
        scoring_packages.len()
    );

    let explorer = gremlins::landscape::LandscapeExplorer;

    let mut handles: Vec<MaintTask> = Vec::new();
    let mut cache_hits: HashMap<String, f64> = HashMap::new();

    for pkg in scoring_packages {
        let pkg_key = pkg.key();
        let pkg_name = pkg.name().to_string();
        let pkg_dir = package_root_dir(pkg);

        // Build landscape of package directory.
        let outlines = explorer
            .build(&pkg_dir, 10)
            .map_err(|e| format!("[{pkg_name}] landscape build failed: {e}"))?;
        let landscape = gremlins::landscape::format_landscape(&outlines, &pkg_dir);

        // Read public API root module.
        let pub_api_source = read_public_api_module(pkg)
            .map_err(|e| format!("[{pkg_name}] failed to read public API module: {e}"))?;

        // Cache key: pkg_key + landscape hash.
        let cache_key = maint_cache_key(&pkg_key, &landscape);

        if let Some(entry) = cache_map.get(&cache_key) {
            if entry.estimates == estimates {
                eprintln!("  [{pkg_name}] cache hit ({})", entry.key);
                cache_hits.insert(pkg_key.clone(), entry.maint_score);
                continue;
            }
        }

        // Cache miss: spawn estimates tasks.
        let prompt = build_maint_prompt(&pkg_name, &landscape, &pub_api_source);

        for _ in 0..estimates {
            let pkg_key = pkg_key.clone();
            let pkg_name = pkg_name.clone();
            let prompt = prompt.clone();
            let model_spec = model_spec.to_string();

            handles.push(tokio::spawn(async move {
                for attempt in 0..MAX_MAINT_RETRIES {
                    match run_maint_agent(&pkg_name, &prompt, &model_spec, verbose).await {
                        Ok(m) => {
                            let score = m.maint_score();
                            return Ok((
                                pkg_key.clone(),
                                score,
                                MaintCacheEntry {
                                    key: String::new(), // filled after median
                                    package: pkg_name.clone(),
                                    model: model_spec.clone(),
                                    estimates,
                                    maint_score: score,
                                    breakdown: m,
                                },
                            ));
                        }
                        Err(e) => {
                            if matches!(&e, CompAgentError::Retryable(_))
                                && attempt + 1 < MAX_MAINT_RETRIES
                            {
                                eprintln!(
                                    "  [{pkg_name}] attempt {}/{}: {e} — retrying in {:.0}s",
                                    attempt + 1,
                                    MAX_MAINT_RETRIES,
                                    MAINT_BACKOFF[attempt as usize]
                                );
                                tokio::time::sleep(Duration::from_secs_f64(
                                    MAINT_BACKOFF[attempt as usize],
                                ))
                                .await;
                                continue;
                            }
                            return Err(format!(
                                "[{pkg_name}] failed after {} attempts: {}",
                                attempt + 1,
                                match &e {
                                    CompAgentError::Retryable(s) | CompAgentError::Fatal(s) =>
                                        s.as_str(),
                                }
                            ));
                        }
                    }
                }

                Err(format!(
                    "[{pkg_name}] exhausted {} retries",
                    MAX_MAINT_RETRIES
                ))
            }));
        }
    }

    // Collect task results, grouped by pkg_key.
    let mut estimates_map: HashMap<String, Vec<f64>> = HashMap::new();
    for handle in handles {
        match handle.await {
            Ok(Ok((pkg_key, score, _entry))) => {
                estimates_map.entry(pkg_key).or_default().push(score);
            }
            Ok(Err(e)) => {
                eprintln!("  WARNING: {e}");
            }
            Err(e) => {
                eprintln!("  WARNING: join error: {e}");
            }
        }
    }

    // Build result map: merge cache hits with fresh estimates.
    // Write cache entries AFTER computing medians (not per-estimate).
    let cache_mutex = Mutex::new(());
    let mut result: HashMap<String, f64> = HashMap::new();

    for pkg in scoring_packages {
        let pkg_key = pkg.key();
        let pkg_name = pkg.name().to_string();

        if let Some(score) = cache_hits.get(&pkg_key) {
            result.insert(pkg_key, *score);
            continue;
        }

        if let Some(scores) = estimates_map.get(&pkg_key) {
            // Compute median from whatever estimates succeeded.
            let med = median(scores);
            eprintln!(
                "  [{pkg_name}] maint: {med:.3} (from {} estimates)",
                scores.len()
            );

            // Write a single cache entry with the median score.
            // Recompute the landscape for the cache key.
            let pkg_dir = package_root_dir(pkg);
            let outlines = explorer
                .build(&pkg_dir, 10)
                .map_err(|e| format!("[{pkg_name}] landscape build failed for cache write: {e}"))?;
            let landscape = gremlins::landscape::format_landscape(&outlines, &pkg_dir);
            let cache_key = maint_cache_key(&pkg_key, &landscape);

            // Take the breakdown from any succeeded estimate (they're all for
            // the same package, so the breakdown structure is representative).
            // We use a placeholder breakdown if none is available.
            let entry = MaintCacheEntry {
                key: cache_key,
                package: pkg_name.clone(),
                model: model_spec.to_string(),
                estimates,
                maint_score: med,
                breakdown: PackageMaintainability {
                    decomposition: 0,
                    interface_design: 0,
                    consistency: 0,
                    change_isolation: 0,
                    duplication: 0,
                },
            };
            let _guard = cache_mutex.lock().unwrap();
            if let Err(e) = append_maint_cache_entry(&maint_cache_path, &entry) {
                eprintln!("  [{pkg_name}] WARNING: failed to append maint cache: {e}");
            }

            result.insert(pkg_key, med);
        } else {
            eprintln!("  [{pkg_name}] WARNING: no estimates collected — defaulting maint=1.0");
            result.insert(pkg_key, 1.0);
        }
    }

    Ok(result)
}

/// Compute package-level drag: `exp(p·(1−maint)) · Σ module_drags`.
pub(crate) fn compute_package_drag(p: f64, maint_score: f64, module_drag_sum: f64) -> f64 {
    (p * (1.0 - maint_score)).exp() * module_drag_sum
}

// ---------------------------------------------------------------------------
// Prompt
// ---------------------------------------------------------------------------

/// Build the maintainability agent prompt — context only, no scoring rubric.
/// The task instructions are appended by [`run_maint_agent`].
fn build_maint_prompt(pkg_name: &str, landscape: &str, pub_api_source: &str) -> String {
    format!(
        "PACKAGE: {pkg_name}\n\n\
         ## Package structure (landscape)\n\n\
         ```\n{landscape}```\n\n\
         ## Public API\n\n\
         Source of the package's public API root module:\n\n\
         ```\n{pub_api_source}\n```"
    )
}

// ---------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------

/// Run a single package maintainability estimate through an ephemeral gremlin Agent stage.
async fn run_maint_agent(
    package: &str,
    prompt: &str,
    model_spec: &str,
    verbose: bool,
) -> Result<PackageMaintainability, CompAgentError> {
    let model_uri = ModelURI::parse(model_spec)
        .map_err(|e| CompAgentError::Fatal(format!("invalid model: {e}")))?;

    let stage = AgentBuilder::new("score-maint")
        .option("src", "")
        .prompt(format!(
            "{prompt}\n\n\
             Score the above package on maintainability.\n\n\
             Target audience: an experienced developer evaluating whether the \
             package is well-organized for long-term maintenance. Judge \
             the package's structure — not correctness, not performance.\n\n\
             Score five dimensions on a 0-4 scale:\n\
             4: Exemplary — would serve as a model for other packages.\n\
             3: Good — minor improvements possible.\n\
             2: Adequate — room for meaningful improvement.\n\
             1: Problematic — structural issues impede maintenance.\n\
             0: Poor — the structure actively resists maintenance.\n\n\
             - **Decomposition**: Are modules appropriately sized and focused? \
             Does each module have a clear, singular responsibility?\n\
             - **Interface design**: Are cross-module contracts minimal and \
             clear? Are public APIs narrow and well-defined?\n\
             - **Consistency**: Do naming, error handling, and abstraction \
             levels stay uniform across the package?\n\
             - **Change isolation**: Would a typical feature change touch one \
             module or scatter across many?\n\
             - **Code duplication**: Are repeated blocks of logic extracted \
             into shared helpers? Is copy-paste minimal across the package?\n\n\
             Produce a JSON object with five fields: decomposition, \
             interface_design, consistency, change_isolation, duplication. \
             Write the JSON to {{maintainability}}."
        ))
        .output("maintainability", output("artifact://maintainability"))
        .landscape(false)
        .model(model_uri.clone())
        .build()
        .map_err(|e| CompAgentError::Fatal(format!("agent build: {e}")))?;

    let def = DefinitionBuilder::new("drag-maint-comp", model_uri)
        .stage(stage)
        .build()
        .map_err(|e| CompAgentError::Fatal(format!("def build: {e}")))?;

    let cwd = std::env::current_dir().map_err(|e| CompAgentError::Fatal(format!("cwd: {e}")))?;

    let config = GremlinConfig { ephemeral: true };
    let mut gremlin = Gremlin::init(
        "drag-maint-comp",
        &cwd,
        &def,
        &HashMap::new(),
        Some(model_spec),
        &config,
    )
    .map_err(|e| CompAgentError::Fatal(format!("gremlin init: {e}")))?;

    if verbose {
        eprintln!("  [{package}] running maint agent (model={model_spec})...");
    }

    let exit_code = gremlin
        .run(None)
        .await
        .map_err(|e| CompAgentError::Fatal(format!("gremlin run: {e}")))?;

    if exit_code != 0 {
        return Err(CompAgentError::Fatal(format!(
            "maint agent exited {exit_code} (bail/missing artifact)"
        )));
    }

    let content = gremlin
        .state
        .content("artifact://maintainability", None)
        .await
        .map_err(|e| CompAgentError::Fatal(format!("artifact read: {e}")))?;

    let json_str = crate::extract_json(&content).ok_or_else(|| {
        CompAgentError::Retryable(format!("no JSON in artifact: {}", truncate(&content, 200)))
    })?;

    let scores: PackageMaintainability = serde_json::from_str(&json_str)
        .map_err(|e| CompAgentError::Retryable(format!("parse: {e}")))?;

    for (name, val) in [
        ("decomposition", scores.decomposition),
        ("interface_design", scores.interface_design),
        ("consistency", scores.consistency),
        ("change_isolation", scores.change_isolation),
        ("duplication", scores.duplication),
    ] {
        if val > 4 {
            return Err(CompAgentError::Retryable(format!(
                "{name}={val} out of 0–4 range"
            )));
        }
    }
    Ok(scores)
}

// ---------------------------------------------------------------------------
// Package utilities
// ---------------------------------------------------------------------------

/// Get the root source directory for a package.
fn package_root_dir(pkg: &PackageInfo) -> PathBuf {
    match pkg {
        PackageInfo::Rust(r) => r.dir.clone(),
        PackageInfo::Python(p) => p.dir.clone(),
    }
}

/// Read the public API root module source for a package.
/// Returns an error when no recognized entry-point file exists.
fn read_public_api_module(pkg: &PackageInfo) -> Result<String, String> {
    match pkg {
        PackageInfo::Rust(r) => {
            // Try lib.rs first, then main.rs, then mod.rs.
            let candidates = [
                r.dir.join("src").join("lib.rs"),
                r.dir.join("src").join("main.rs"),
                r.dir.join("src").join("mod.rs"),
            ];
            for candidate in &candidates {
                if candidate.exists() {
                    return std::fs::read_to_string(candidate)
                        .map_err(|e| format!("{}: {e}", candidate.display()));
                }
            }
            Err(format!(
                "no lib.rs, main.rs, or mod.rs in {}",
                r.dir.join("src").display()
            ))
        }
        PackageInfo::Python(p) => {
            let init = p.dir.join("__init__.py");
            if init.exists() {
                std::fs::read_to_string(&init).map_err(|e| format!("{}: {e}", init.display()))
            } else {
                Err(format!("{} not found", init.display()))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

/// Compute a BLAKE3-based cache key for a package maintainability entry.
fn maint_cache_key(pkg_key: &str, landscape: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"v2\0");
    hasher.update(pkg_key.as_bytes());
    hasher.update(b"\0");
    hasher.update(landscape.as_bytes());
    hasher.finalize().to_hex()[..16].to_string()
}

/// Load the maintainability JSONL cache file.
fn load_maint_cache(path: &Path) -> Result<HashMap<String, MaintCacheEntry>, String> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read maint cache {}: {e}", path.display()))?;
    let mut map = HashMap::new();
    for (line_no, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let entry: MaintCacheEntry = serde_json::from_str(line).map_err(|e| {
            format!(
                "{}:{}: invalid maint cache entry: {e}",
                path.display(),
                line_no + 1
            )
        })?;
        map.insert(entry.key.clone(), entry);
    }
    Ok(map)
}

/// Append one MaintCacheEntry as a JSON line to the maintainability cache.
///
/// Callers must serialize appends; this function does not guard against
/// concurrent writes.
fn append_maint_cache_entry(path: &Path, entry: &MaintCacheEntry) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create maint cache dir {}: {e}", parent.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("failed to open maint cache {}: {e}", path.display()))?;
    let mut line = serde_json::to_string(entry).map_err(|e| format!("serialization error: {e}"))?;
    line.push('\n');
    std::io::Write::write_all(&mut file, line.as_bytes())
        .map_err(|e| format!("failed to write maint cache entry: {e}"))?;
    Ok(())
}
