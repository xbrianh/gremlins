// Package maintainability scoring for gremlins-drag.
//
// This module handles:
// - Building a landscape of a package directory.
// - Prompting one independent Agent stage per configured maintainability
//   dimension (from `MaintainabilityConfig`), run concurrently, each scoring
//   a single dimension (0–4) with tailored anchors so every dimension gets
//   focused attention.
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

use crate::config::{MaintainabilityConfig, MaintainabilityDimensionDef};
use crate::lang::PackageInfo;
use crate::util::{median, truncate, CacheState, CompAgentError};

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// Maintainability breakdown: dimension key → rounded score (0–4).
///
/// The dimension set is config-driven, so this is a plain map rather than a
/// fixed struct. Older caches that stored the former fixed dimension fields
/// still deserialize correctly.
pub(crate) type PackageMaintainability = HashMap<String, u8>;

/// Build a `PackageMaintainability` breakdown from a map of dimension
/// key → median score (f64), rounding each to u8 for the cacheable breakdown.
fn build_maint_breakdown(dims: &HashMap<&str, f64>) -> HashMap<String, u8> {
    dims.iter()
        .map(|(k, v)| (k.to_string(), v.round() as u8))
        .collect()
}

/// Compute the maintainability score (0–1) directly from unrounded f64
/// dimension medians, avoiding the upward bias of per-dimension rounding.
///
/// `dim_count` is the number of configured dimensions; the maximum possible
/// sum is `dim_count * 4`. An empty dimension set yields the neutral 0.5.
fn compute_maint_score_from_map(dims: &HashMap<&str, f64>, dim_count: usize) -> f64 {
    if dim_count == 0 {
        return 0.5;
    }
    let sum: f64 = dims.values().sum();
    let max = dim_count as f64 * 4.0;
    (sum / max).clamp(0.0, 1.0)
}

const MAX_MAINT_RETRIES: u32 = 3;
const MAINT_BACKOFF: [f64; 2] = [1.0, 2.0];

/// Handle for a single per-dimension maintainability scoring task.
/// Always returns the (pkg_key, dim_name) keys alongside the result
/// so the collection loop can inject a default on failure.
type DimTask = tokio::task::JoinHandle<(String, String, Result<u8, String>)>;
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
    maint_config: &MaintainabilityConfig,
    config_hash: &str,
) -> Result<HashMap<String, f64>, String> {
    if scoring_packages.is_empty() {
        return Ok(HashMap::new());
    }

    // The dimension set is fully config-driven. `config_hash` poisons the
    // cache key so a changed dimension set cannot reuse stale cache entries.

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

    let mut handles: Vec<DimTask> = Vec::new();
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
        let pub_api_source = read_public_api_module(pkg);

        // Cache key: pkg_key + hash of all source file contents + estimates.
        let cache_key = compute_pkg_cache_key(&pkg_key, pkg, estimates, config_hash)
            .map_err(|e| format!("[{pkg_name}] cache key computation failed: {e}"))?;

        if let Some(entry) = cache_map.get(&cache_key) {
            if entry.estimates == estimates && entry.model == model_spec {
                eprintln!("  [{pkg_name}] cache hit ({})", entry.key);
                cache_hits.insert(pkg_key.clone(), entry.maint_score);
                continue;
            }
        }

        // Cache miss: spawn `estimates × N` tasks — one per dimension per estimate.
        for dim in &maint_config.dimensions {
            let prompt = build_dim_prompt(
                &pkg_name,
                &landscape,
                &pub_api_source,
                &maint_config.preamble,
                dim,
            );

            for _ in 0..estimates {
                let pkg_key = pkg_key.clone();
                let pkg_name = pkg_name.clone();
                let prompt = prompt.clone();
                let model_spec = model_spec.to_string();
                let dim_name = dim.key.to_string();
                let dim_label = dim.label.to_string();

                handles.push(tokio::spawn(async move {
                    for attempt in 0..MAX_MAINT_RETRIES {
                        match run_dim_agent(
                            &pkg_name,
                            &dim_label,
                            &prompt,
                            &model_spec,
                            verbose,
                        )
                        .await
                        {
                            Ok(score) => {
                                return (pkg_key.clone(), dim_name.clone(), Ok(score));
                            }
                            Err(e) => {
                                if matches!(&e, CompAgentError::Retryable(_))
                                    && attempt + 1 < MAX_MAINT_RETRIES
                                {
                                    eprintln!(
                                        "  [{pkg_name}] {dim_label} attempt {}/{}: {e} — retrying in {:.0}s",
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
                                return (
                                    pkg_key.clone(),
                                    dim_name.clone(),
                                    Err(format!(
                                        "[{pkg_name}] {dim_label} failed after {} attempts: {}",
                                        attempt + 1,
                                        match &e {
                                            CompAgentError::Retryable(s)
                                            | CompAgentError::Fatal(s) => s.as_str(),
                                        }
                                    )),
                                );
                            }
                        }
                    }

                    (
                        pkg_key.clone(),
                        dim_name.clone(),
                        Err(format!(
                            "[{pkg_name}] {dim_label} exhausted {} retries",
                            MAX_MAINT_RETRIES
                        )),
                    )
                }));
            }
        }
    }

    // Collect task results, grouped by (pkg_key, dim_name).
    let mut estimates_map: HashMap<(String, String), Vec<u8>> = HashMap::new();
    for handle in handles {
        match handle.await {
            Ok((pkg_key, dim_name, Ok(score))) => {
                estimates_map
                    .entry((pkg_key, dim_name))
                    .or_default()
                    .push(score);
            }
            Ok((_pkg_key, _dim_name, Err(e))) => {
                eprintln!("  WARNING: {e}");
            }
            Err(e) => {
                eprintln!("  WARNING: join error: {e}");
            }
        }
    }
    // Build result map: merge cache hits with fresh estimates.
    let cache_mutex = Mutex::new(());
    let mut result: HashMap<String, f64> = HashMap::new();

    for pkg in scoring_packages {
        let pkg_key = pkg.key();
        let pkg_name = pkg.name().to_string();

        if let Some(score) = cache_hits.get(&pkg_key) {
            result.insert(pkg_key, *score);
            continue;
        }

        // Compute the median per dimension, keeping f64 precision
        // so maint_score is not biased by per-dimension rounding.
        let mut dim_medians: HashMap<&str, f64> = HashMap::new();

        for dim in &maint_config.dimensions {
            let key = (pkg_key.clone(), dim.key.to_string());
            if let Some(scores) = estimates_map.get(&key) {
                if scores.is_empty() {
                    eprintln!(
                        "  [{pkg_name}] {label}: no estimates — defaulting to 2.0",
                        label = dim.label
                    );
                    dim_medians.insert(dim.key.as_str(), 2.0);
                } else {
                    let f64_scores: Vec<f64> = scores.iter().map(|&s| s as f64).collect();
                    let med = median(&f64_scores);
                    if verbose {
                        eprintln!("  [{pkg_name}] {label}: {med:.1}", label = dim.label);
                    }
                    dim_medians.insert(dim.key.as_str(), med);
                }
            } else {
                eprintln!(
                    "  [{pkg_name}] {label}: no estimates collected — defaulting to 2.0",
                    label = dim.label
                );
                dim_medians.insert(dim.key.as_str(), 2.0);
            }
        }

        // Compute maint_score from the unrounded f64 medians.
        let maint_score = compute_maint_score_from_map(&dim_medians, maint_config.dimensions.len());

        eprintln!("  [{pkg_name}] maint: {maint_score:.3}");

        // Only cache and return when EVERY dimension got at least one real estimate.
        let all_dims_complete = maint_config.dimensions.iter().all(|dim| {
            let key = (pkg_key.clone(), dim.key.to_string());
            estimates_map
                .get(&key)
                .is_some_and(|v| v.len() >= estimates)
        });

        if !all_dims_complete {
            eprintln!(
                "  [{pkg_name}] WARNING: incomplete estimates — skipping cache write and result"
            );
            continue;
        }

        // Build breakdown with rounded values for the cache.
        let breakdown = build_maint_breakdown(&dim_medians);

        let cache_key = compute_pkg_cache_key(&pkg_key, pkg, estimates, config_hash)
            .map_err(|e| format!("[{pkg_name}] cache key computation failed: {e}"))?;

        let entry = MaintCacheEntry {
            key: cache_key,
            package: pkg_name.clone(),
            model: model_spec.to_string(),
            estimates,
            maint_score,
            breakdown,
        };
        let _guard = cache_mutex.lock().unwrap();
        if let Err(e) = append_maint_cache_entry(&maint_cache_path, &entry) {
            eprintln!("  [{pkg_name}] WARNING: failed to append maint cache: {e}");
        }

        result.insert(pkg_key, maint_score);
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

/// Build a per-dimension prompt containing the package context and tailored
/// anchors for a single dimension.
fn build_dim_prompt(
    pkg_name: &str,
    landscape: &str,
    pub_api_source: &str,
    preamble: &str,
    dim: &MaintainabilityDimensionDef,
) -> String {
    let api_section = if pub_api_source.is_empty() {
        "(no public API root module found)".to_string()
    } else {
        format!("Source of the package's public API root module:\n\n```\n{pub_api_source}\n```")
    };
    format!(
        "PACKAGE: {pkg_name}\n\n\
         ## Package structure (landscape)\n\n\
         ```\n{landscape}```\n\n\
         ## Public API\n\n\
         {api_section}\n\n\
         ## Dimension: {label}\n\n\
         {description}\n\n\
         {preamble}\n\n\
         {anchors}\n\n\
         Output only a single integer (0–4). Write it to {{maintainability}}.",
        label = dim.label,
        description = dim.description,
        anchors = dim.anchors,
    )
}

// ---------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------

/// Run a single-dimension maintainability estimate through an ephemeral
/// gremlin Agent stage.  Returns a u8 score (0–4).
async fn run_dim_agent(
    package: &str,
    dim_label: &str,
    prompt: &str,
    model_spec: &str,
    verbose: bool,
) -> Result<u8, CompAgentError> {
    let model_uri = ModelURI::parse(model_spec)
        .map_err(|e| CompAgentError::Fatal(format!("invalid model: {e}")))?;

    let stage = AgentBuilder::new("score-maint")
        .option("src", "")
        .prompt(prompt.to_string())
        .output("maintainability", output("artifact://maintainability"))
        .landscape(false)
        .model(model_uri.clone())
        .build()
        .map_err(|e| CompAgentError::Fatal(format!("agent build: {e}")))?;

    let def = DefinitionBuilder::new("drag-maint-dim", model_uri)
        .stage(stage)
        .build()
        .map_err(|e| CompAgentError::Fatal(format!("def build: {e}")))?;

    let cwd = std::env::current_dir().map_err(|e| CompAgentError::Fatal(format!("cwd: {e}")))?;

    let config = GremlinConfig { ephemeral: true };
    let mut gremlin = Gremlin::init(
        "drag-maint-dim",
        &cwd,
        &def,
        &HashMap::new(),
        Some(model_spec),
        &config,
    )
    .map_err(|e| CompAgentError::Fatal(format!("gremlin init: {e}")))?;

    if verbose {
        eprintln!("  [{package}] running dim agent ({dim_label}, model={model_spec})...");
    }

    let exit_code = gremlin
        .run(None)
        .await
        .map_err(|e| CompAgentError::Fatal(format!("gremlin run: {e}")))?;

    if exit_code != 0 {
        return Err(CompAgentError::Fatal(format!(
            "dim agent ({dim_label}) exited {exit_code} (bail/missing artifact)"
        )));
    }

    let content = gremlin
        .state
        .content("artifact://maintainability", None)
        .await
        .map_err(|e| CompAgentError::Fatal(format!("artifact read: {e}")))?;

    let score = extract_dim_score(&content).ok_or_else(|| {
        CompAgentError::Retryable(format!(
            "no integer 0–4 in artifact: {}",
            truncate(&content, 200)
        ))
    })?;

    if score > 4 {
        return Err(CompAgentError::Retryable(format!(
            "{dim_label}={score} out of 0–4 range"
        )));
    }

    Ok(score)
}

/// Extract a single 0–4 integer from agent output.
///
/// Only accepts a whitespace-delimited token that is exactly a single
/// digit 0–4 (after stripping trailing punctuation).  Multi-digit tokens
/// ("10", "30") and prose with embedded digits are rejected so the agent
/// is retried rather than silently locked to the wrong score.
fn extract_dim_score(content: &str) -> Option<u8> {
    for word in content.split_whitespace() {
        let trimmed = word.trim_matches(|c: char| {
            c == '.' || c == ',' || c == ';' || c == ':' || c == '"' || c == '\''
        });
        // Accept only if the trimmed token is exactly a single ASCII digit 0–4.
        if trimmed.len() == 1 {
            if let Some(ch) = trimmed.chars().next() {
                if let Some(n) = ch.to_digit(10) {
                    if n <= 4 {
                        return Some(n as u8);
                    }
                }
            }
        }
    }

    None
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
/// Returns an empty string when no recognized entry-point file exists.
fn read_public_api_module(pkg: &PackageInfo) -> String {
    match pkg {
        PackageInfo::Rust(r) => {
            let candidates = [
                r.dir.join("src").join("lib.rs"),
                r.dir.join("src").join("main.rs"),
                r.dir.join("src").join("mod.rs"),
            ];
            for candidate in &candidates {
                if candidate.exists() {
                    return std::fs::read_to_string(candidate).unwrap_or_else(|e| {
                        eprintln!("  WARNING: failed to read {}: {e}", candidate.display());
                        String::new()
                    });
                }
            }
            eprintln!(
                "  WARNING: no lib.rs, main.rs, or mod.rs in {}",
                r.dir.join("src").display()
            );
            String::new()
        }
        PackageInfo::Python(p) => {
            let init = p.dir.join("__init__.py");
            if init.exists() {
                std::fs::read_to_string(&init).unwrap_or_else(|e| {
                    eprintln!("  WARNING: failed to read {}: {e}", init.display());
                    String::new()
                })
            } else {
                eprintln!("  WARNING: {} not found", init.display());
                String::new()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

/// Compute a BLAKE3-based cache key using a Merkle-tree of all source
/// file contents plus the estimate count and the maintainability config
/// hash.  Any change to any source file, the number of estimates, or the
/// dimension configuration invalidates the entry.
fn compute_pkg_cache_key(
    pkg_key: &str,
    pkg: &PackageInfo,
    estimates: usize,
    config_hash: &str,
) -> Result<String, String> {
    let src_dir = match pkg {
        PackageInfo::Rust(r) => r.dir.join("src"),
        PackageInfo::Python(p) => p.dir.clone(),
    };

    let files = crate::lang::collect_source_files(&src_dir).map_err(|e| {
        format!(
            "failed to collect source files in {}: {e}",
            src_dir.display()
        )
    })?;

    if files.is_empty() {
        return Err(format!("no source files found in {}", src_dir.display()));
    }

    // Per-file hashes, sorted by relative path for determinism.
    let mut file_hashes: Vec<(String, String)> = Vec::with_capacity(files.len());
    for f in &files {
        let content =
            std::fs::read(f).map_err(|e| format!("failed to read {}: {e}", f.display()))?;
        let hash = blake3::hash(&content).to_hex();
        let rel = f
            .strip_prefix(&src_dir)
            .unwrap_or(f)
            .to_string_lossy()
            .to_string();
        file_hashes.push((rel, hash.to_string()));
    }
    file_hashes.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"v5\0");
    hasher.update(pkg_key.as_bytes());
    hasher.update(b"\0");
    for (path, hash) in &file_hashes {
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
        hasher.update(hash.as_bytes());
        hasher.update(b"\0");
    }
    hasher.update(&estimates.to_le_bytes());
    hasher.update(b"\0");
    hasher.update(config_hash.as_bytes());
    Ok(hasher.finalize().to_hex()[..16].to_string())
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
        match serde_json::from_str::<MaintCacheEntry>(line) {
            Ok(entry) => {
                map.insert(entry.key.clone(), entry);
            }
            Err(e) => {
                eprintln!(
                    "  WARNING: {}:{}: skipping invalid maint cache entry: {e}",
                    path.display(),
                    line_no + 1
                );
            }
        }
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── extract_dim_score ──

    #[test]
    fn extract_bare_digit() {
        assert_eq!(extract_dim_score("3"), Some(3));
        assert_eq!(extract_dim_score("0"), Some(0));
        assert_eq!(extract_dim_score("4"), Some(4));
    }

    #[test]
    fn extract_digit_in_prose() {
        // A standalone digit token among words.
        assert_eq!(
            extract_dim_score("The score is 3 for this dimension."),
            Some(3)
        );
    }

    #[test]
    fn extract_digit_trailing_punctuation() {
        assert_eq!(extract_dim_score("3."), Some(3));
        assert_eq!(extract_dim_score("2,"), Some(2));
        assert_eq!(extract_dim_score("4:"), Some(4));
        assert_eq!(extract_dim_score("1;"), Some(1));
        assert_eq!(extract_dim_score("\"3\""), Some(3));
        assert_eq!(extract_dim_score("'0'"), Some(0));
    }

    #[test]
    fn extract_digit_with_markdown_fence() {
        assert_eq!(extract_dim_score("```\n3\n```"), Some(3));
    }

    #[test]
    fn reject_multi_digit() {
        // "10" should not be parsed as 1.
        assert_eq!(extract_dim_score("10"), None);
        assert_eq!(extract_dim_score("30"), None);
        assert_eq!(extract_dim_score("100"), None);
    }

    #[test]
    fn reject_out_of_range_single_digit() {
        // 5–9 are single digits but out of range.
        assert_eq!(extract_dim_score("5"), None);
        assert_eq!(extract_dim_score("9"), None);
    }

    #[test]
    fn reject_ambiguous_prose() {
        // Embedded digits in words should not match.
        assert_eq!(extract_dim_score("version 2.0"), None);
        assert_eq!(extract_dim_score("0-4 scale"), None);
    }

    #[test]
    fn empty_or_no_digit() {
        assert_eq!(extract_dim_score(""), None);
        assert_eq!(extract_dim_score("no digits here"), None);
    }

    // ── build_maint_breakdown ──

    #[test]
    fn build_maint_breakdown_all_present() {
        let mut m: HashMap<&str, f64> = HashMap::new();
        m.insert("decomposition", 3.0);
        m.insert("interface_design", 2.0);
        m.insert("consistency", 4.0);

        let p = build_maint_breakdown(&m);
        assert_eq!(p.get("decomposition"), Some(&3));
        assert_eq!(p.get("interface_design"), Some(&2));
        assert_eq!(p.get("consistency"), Some(&4));
        assert_eq!(p.len(), 3);
    }

    #[test]
    fn build_maint_breakdown_rounds_f64() {
        let mut m: HashMap<&str, f64> = HashMap::new();
        // 2.7 rounds to 3, 2.2 rounds to 2.
        m.insert("decomposition", 2.7);
        m.insert("interface_design", 2.2);
        let p = build_maint_breakdown(&m);
        assert_eq!(p.get("decomposition"), Some(&3));
        assert_eq!(p.get("interface_design"), Some(&2));
    }

    // ── compute_maint_score_from_map ──

    /// The ten dimensions from the bundled config, used to exercise the
    /// dimension-count-based scoring math.
    const TEN_DIMS: [&str; 10] = [
        "decomposition",
        "interface_design",
        "consistency",
        "change_isolation",
        "duplication",
        "locality",
        "abstraction_clarity",
        "conceptual_surface",
        "predictability",
        "evasion",
    ];

    #[test]
    fn maint_score_from_f64_map() {
        let mut m: HashMap<&str, f64> = HashMap::new();
        // All ten dimensions at 2.0 → score = 20/40 = 0.5.
        for key in TEN_DIMS {
            m.insert(key, 2.0);
        }
        let score = compute_maint_score_from_map(&m, 10);
        assert!((score - 0.5).abs() < 1e-9);

        // One dimension at 2.5 → sum = 20.5, score = 0.5125.
        m.insert("decomposition", 2.5);
        let score = compute_maint_score_from_map(&m, 10);
        assert!((score - 0.5125).abs() < 1e-9);
    }

    #[test]
    fn maint_score_preserves_fraction() {
        let mut m: HashMap<&str, f64> = HashMap::new();
        for key in TEN_DIMS {
            m.insert(key, 2.0);
        }
        m.insert("decomposition", 1.0);
        // Sum = 1.0 + 9*2.0 = 19.0, score = 19/40 = 0.475.
        let score = compute_maint_score_from_map(&m, 10);
        assert!((score - 0.475).abs() < 1e-9);
    }

    #[test]
    fn maint_score_empty_dims_is_neutral() {
        let m: HashMap<&str, f64> = HashMap::new();
        assert!((compute_maint_score_from_map(&m, 0) - 0.5).abs() < 1e-9);
    }
}
