// Directory maintainability scoring for gremlins-drag.
//
// This module handles:
// - Prompting one Agent stage per directory per estimate, scoring all
//   configured maintainability dimensions in a single JSON-returning call.
// - Tools-based exploration: the agent uses Landscape, Read, Grep, and Glob
//   to explore the directory's source files rather than having source
//   embedded in the prompt.
// - Caching the mean score per directory (one entry per scored directory,
//   not per estimate).
// - Computing the directory-level drag formula.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use gremlins::builders::{output, AgentBuilder, DefinitionBuilder};
use gremlins::executor::gremlin::{Gremlin, GremlinConfig};
use gremlins::model_uri::ModelURI;

use crate::config::MaintainabilityConfig;
use crate::util::{extract_json, mean_with_ci, truncate, CacheState, CompAgentError};

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// Maintainability breakdown: dimension key → raw score (0–4).
///
/// The dimension set is config-driven, so this is a plain map rather than a
/// fixed struct.
pub(crate) type DirMaintainability = HashMap<String, u8>;

/// Compute the maintainability score (0–1) directly from unrounded f64
/// dimension means, avoiding the upward bias of per-dimension rounding.
///
/// The maximum possible sum is `dims.len() * 4`. An empty dimension set
/// yields the neutral 0.5.
fn compute_maint_score_from_map(dims: &HashMap<&str, f64>) -> f64 {
    if dims.is_empty() {
        return 0.5;
    }
    let sum: f64 = dims.values().sum();
    let max = dims.len() as f64 * 4.0;
    (sum / max).clamp(0.0, 1.0)
}

const MAX_MAINT_RETRIES: u32 = 3;
const MAINT_BACKOFF: [f64; 2] = [1.0, 2.0];

/// Handle for a single per-estimate multi-dimension maintainability task.
/// Returns (dir_path, dimension_scores) — all dimensions scored in one call.
type MaintEstimateTask = tokio::task::JoinHandle<(String, Result<HashMap<String, u8>, String>)>;

/// One line in the maintainability cache file.
///
/// Unlike module-level comprehensibility (which stores one entry per estimate),
/// this stores one entry per *scored directory* with all raw per-estimate
/// dimension scores. Aggregation to a 0–1 score is recomputed from these.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub(crate) struct MaintCacheEntry {
    /// `dir_path || "\0" || blake3(all source files)` — hex, first 16 chars.
    pub key: String,
    #[serde(alias = "package")]
    pub dir: String,
    pub model: String,
    pub estimates: usize,
    /// Per-estimate raw dimension scores (0–4 per dimension).
    /// Aggregation to a 0–1 score is pure math — recomputed from these.
    #[serde(default)]
    pub raw_estimates: Vec<DirMaintainability>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Score maintainability for each scoring directory.
///
/// Returns a map from absolute directory path to maintainability score
/// (0–1) plus the set of directories that were intentionally skipped
/// because they have no direct source files (only nested subdirectories).
/// Callers should treat skipped directories as containers with no
/// maintainability penalty rather than as scoring failures.
pub(crate) async fn score_directory_maintainability(
    model_spec: &str,
    verbose: bool,
    estimates: usize,
    scoring_dirs: &[PathBuf],
    cache_state: CacheState,
    maint_config: &MaintainabilityConfig,
    config_hash: &str,
) -> Result<(HashMap<String, f64>, HashSet<String>), String> {
    if scoring_dirs.is_empty() {
        return Ok((HashMap::new(), HashSet::new()));
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
        "Scoring maintainability for {} directories...",
        scoring_dirs.len()
    );

    let mut handles: Vec<MaintEstimateTask> = Vec::new();
    let mut estimates_map: HashMap<String, Vec<HashMap<String, u8>>> = HashMap::new();
    let mut cache_miss_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut skipped_dirs: HashSet<String> = HashSet::new();

    for dir in scoring_dirs {
        let dir_str = dir.to_string_lossy().into_owned();
        let dir_name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| dir_str.clone());

        // Build the directory source manifest for the agent to explore.
        // A directory whose source files are all in nested subdirectories
        // (i.e. no direct children) is legitimate — `enumerate_source_dirs`
        // includes every intermediate ancestor. Skip it gracefully rather
        // than erroring.
        let manifest = match build_dir_manifest(dir) {
            Ok(Some(m)) => m,
            Ok(None) => {
                eprintln!("  [{dir_name}] skipping: no direct source files (intermediate container directory)");
                skipped_dirs.insert(dir_str);
                continue;
            }
            Err(e) => return Err(format!("[{dir_name}] {e}")),
        };

        // Cache key: dir_path + hash of all source file contents + estimates.
        let cache_key = compute_dir_cache_key(dir, estimates, config_hash)
            .map_err(|e| format!("[{dir_name}] cache key computation failed: {e}"))?;

        if let Some(entry) = cache_map.get(&cache_key) {
            if entry.estimates == estimates && entry.model == model_spec {
                eprintln!("  [{dir_name}] cache hit ({})", entry.key);
                estimates_map.insert(dir_str.clone(), entry.raw_estimates.clone());
                continue;
            }
        }

        cache_miss_keys.insert(dir_str.clone());

        // Build the multi-dimension prompt (manifest-based, no source embedded).
        let prompt = build_multi_dim_prompt(&dir_name, &manifest, maint_config);

        // Cache miss: spawn `estimates` tasks.
        for _ in 0..estimates {
            let dir_str = dir_str.clone();
            let dir_name = dir_name.clone();
            let prompt = prompt.clone();
            let model_spec = model_spec.to_string();
            let maint_config = maint_config.clone();
            let dir_clone = dir.clone();

            handles.push(tokio::spawn(async move {
                for attempt in 0..MAX_MAINT_RETRIES {
                    match run_maint_agent(
                        &dir_name,
                        &prompt,
                        &model_spec,
                        verbose,
                        &maint_config,
                        &dir_clone,
                    )
                    .await
                    {
                        Ok(scores) => {
                            return (dir_str, Ok(scores));
                        }
                        Err(e) => {
                            if matches!(&e, CompAgentError::Retryable(_))
                                && attempt + 1 < MAX_MAINT_RETRIES
                            {
                                eprintln!(
                                    "  [{dir_name}] maintainability attempt {}/{}: {e} — retrying in {:.0}s",
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
                                dir_str,
                                Err(format!(
                                    "[{dir_name}] maintainability failed after {} attempts: {}",
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
                    dir_str,
                    Err(format!(
                        "[{dir_name}] maintainability exhausted {} retries",
                        MAX_MAINT_RETRIES
                    )),
                )
            }));
        }
    }

    // --- Collect fresh results: group per-dir across estimates ---
    let mut fresh_estimates: HashMap<String, Vec<HashMap<String, u8>>> = HashMap::new();
    for handle in handles {
        match handle.await {
            Ok((dir_str, Ok(scores))) => {
                fresh_estimates.entry(dir_str).or_default().push(scores);
            }
            Ok((_dir_str, Err(e))) => {
                eprintln!("  WARNING: {e}");
            }
            Err(e) => {
                eprintln!("  WARNING: join error: {e}");
            }
        }
    }

    // Merge fresh estimates into estimates_map.
    for (dir_str, scores) in fresh_estimates {
        estimates_map.entry(dir_str).or_default().extend(scores);
    }

    // --- Build result map: unified aggregation for cache hits and misses ---
    let cache_mutex = Mutex::new(());
    let mut result: HashMap<String, f64> = HashMap::new();

    for dir in scoring_dirs {
        let dir_str = dir.to_string_lossy().into_owned();
        let dir_name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| dir_str.clone());

        // Intentionally skipped: intermediate container directory with no
        // direct source files.  No score needed — it's a pure structural
        // node in the directory trie.
        if skipped_dirs.contains(&dir_str) {
            continue;
        }

        let estimate_batch = match estimates_map.get(&dir_str) {
            Some(b) if !b.is_empty() => b,
            _ => {
                eprintln!(
                    "  [{dir_name}] no estimates collected for '{dir_str}' — \
                     aborting (scoring cannot proceed without estimates)",
                );
                return Err(format!(
                    "[{dir_name}] no estimates collected for '{dir_str}' — \
                     all retries exhausted or agent failures prevented scoring"
                ));
            }
        };

        // Aggregate: per-dimension, compute mean across estimates (f64).
        let mut dim_means: HashMap<&str, f64> = HashMap::new();

        for dim in &maint_config.dimensions {
            let f64_scores: Vec<f64> = estimate_batch
                .iter()
                .filter_map(|est| est.get(&dim.key).map(|&s| s as f64))
                .collect();

            if f64_scores.is_empty() {
                eprintln!(
                    "  [{dir_name}] {label}: no estimates — defaulting to 2.0",
                    label = dim.label
                );
                dim_means.insert(dim.key.as_str(), 2.0);
            } else {
                let (m, lo, hi) = mean_with_ci(&f64_scores);
                if verbose {
                    eprintln!(
                        "  [{dir_name}] {label}: {m:.1} (95% CI: {lo:.1}–{hi:.1})",
                        label = dim.label
                    );
                }
                dim_means.insert(dim.key.as_str(), m);
            }
        }

        let maint_score = compute_maint_score_from_map(&dim_means);

        eprintln!("  [{dir_name}] maint: {maint_score:.3}");

        let all_dims_complete = maint_config.dimensions.iter().all(|dim| {
            let count = estimate_batch
                .iter()
                .filter(|est| est.contains_key(&dim.key))
                .count();
            count >= estimates
        });

        if !all_dims_complete {
            eprintln!(
                "  [{dir_name}] WARNING: incomplete dimension coverage for '{dir_str}' — \
                 aborting (not all dimensions reached the required estimate count)",
            );
            return Err(format!(
                "[{dir_name}] incomplete dimension coverage for '{dir_str}' — \
                 not all dimensions reached the required {estimates} estimate(s)"
            ));
        }

        if estimate_batch.len() != estimates {
            eprintln!(
                "  [{dir_name}] WARNING: {}/{} estimates collected for '{dir_str}' — \
                 aborting (estimate count does not match expected)",
                estimate_batch.len(),
                estimates,
            );
            return Err(format!(
                "[{dir_name}] estimate count mismatch for '{dir_str}': \
                 got {} estimates, expected {estimates}",
                estimate_batch.len(),
            ));
        }

        let was_cache_miss = cache_miss_keys.contains(&dir_str);
        if was_cache_miss {
            let cache_key = compute_dir_cache_key(dir, estimates, config_hash)
                .map_err(|e| format!("[{dir_name}] cache key computation failed: {e}"))?;

            let entry = MaintCacheEntry {
                key: cache_key,
                dir: dir_str.clone(),
                model: model_spec.to_string(),
                estimates,
                raw_estimates: estimate_batch.clone(),
            };
            let _guard = cache_mutex.lock().unwrap();
            if let Err(e) = append_maint_cache_entry(&maint_cache_path, &entry) {
                eprintln!("  [{dir_name}] WARNING: failed to append maint cache: {e}");
            }
        }

        result.insert(dir_str, maint_score);
    }

    Ok((result, skipped_dirs))
}

// ---------------------------------------------------------------------------
// Directory manifest (for tools-based exploration)
// ---------------------------------------------------------------------------

/// Build a manifest string listing the directory path and sorted relative
/// file paths for every recognized source file directly under it.
///
/// The agent is expected to use its tools (Landscape, Read, Grep, Glob) to
/// explore these files — full source is not embedded in the prompt.
///
/// Returns `Ok(None)` when the directory has no direct source files (they
/// may all live in nested subdirectories, which are scored independently).
/// Collection failures are returned as `Err` so callers can propagate them.
fn build_dir_manifest(dir: &Path) -> Result<Option<String>, String> {
    // `collect_source_files` recurses into subdirectories, but each nested
    // directory is scored independently. Keep only files whose immediate
    // parent is `dir` so the manifest doesn't leak subdirectory files into
    // this directory's score.
    let files: Vec<_> = crate::lang::collect_source_files(dir)
        .map_err(|e| format!("failed to collect source files in {}: {e}", dir.display()))?
        .into_iter()
        .filter(|f| f.parent() == Some(dir))
        .collect();

    if files.is_empty() {
        return Ok(None);
    }

    // Sort for deterministic output.
    let mut files = files;
    files.sort();

    let mut manifest = format!("DIRECTORY: {}\n\nSOURCE FILES:\n", dir.display());
    for f in &files {
        let rel = f.strip_prefix(dir).unwrap_or(f);
        manifest.push_str(&format!("  {}\n", rel.display()));
    }
    manifest.push_str(&format!("\n({} source files total)\n", files.len()));

    Ok(Some(manifest))
}

// ---------------------------------------------------------------------------
// Prompt
// ---------------------------------------------------------------------------

/// Build a multi-dimension prompt listing all dimensions and asking for a
/// JSON object output. The agent explores the directory source via tools.
fn build_multi_dim_prompt(
    dir_name: &str,
    manifest: &str,
    config: &MaintainabilityConfig,
) -> String {
    let dim_list: String = config
        .dimensions
        .iter()
        .map(|d| {
            format!(
                "### {key}: {label}\n\n{description}\n\nAnchors:\n{anchors}",
                key = d.key,
                label = d.label,
                description = d.description,
                anchors = d.anchors,
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    let dim_keys = config
        .dimensions
        .iter()
        .map(|d| d.key.as_str())
        .collect::<Vec<_>>()
        .join(", ");

    format!(
        "DIRECTORY: {dir_name}\n\n\
         {manifest}\n\n\
         {preamble}\n\n\
         ## Dimensions to score (0–4 scale)\n\n\
         {dim_list}\n\n\
         ## Instructions\n\n\
         **Scope**: Limit your assessment to the source files under this\n\
         directory listed above.  Nested directories are scored independently.

\
         Explore the directory source files using Landscape, Read, Grep, and Glob tools.\n\
         Score each of the {n} dimensions above on a 0–4 integer scale.\n\
         Produce a single JSON object with these fields: {dim_keys}.\n\
         Write the JSON to {{maintainability}}.",
        preamble = config.preamble,
        n = config.dimensions.len(),
    )
}

// ---------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------

/// Run a single multi-dimension maintainability estimate through an ephemeral
/// gremlin Agent stage.  Returns a HashMap<String, u8> of dimension scores.
/// `dir` is used as the agent's working directory so it can access
/// source files via its tools.
async fn run_maint_agent(
    dir_name: &str,
    prompt: &str,
    model_spec: &str,
    verbose: bool,
    maint_config: &MaintainabilityConfig,
    dir: &Path,
) -> Result<HashMap<String, u8>, CompAgentError> {
    if maint_config.dimensions.is_empty() {
        return Err(CompAgentError::Fatal(
            "maintainability config has no dimensions".to_string(),
        ));
    }

    let model_uri = ModelURI::parse(model_spec)
        .map_err(|e| CompAgentError::Fatal(format!("invalid model: {e}")))?;

    let stage = AgentBuilder::new("score-maint")
        .option("src", "")
        .prompt(prompt.to_string())
        .output("maintainability", output("artifact://maintainability"))
        .landscape(true)
        .model(model_uri.clone())
        .build()
        .map_err(|e| CompAgentError::Fatal(format!("agent build: {e}")))?;

    let def = DefinitionBuilder::new("drag-maint", model_uri)
        .stage(stage)
        .build()
        .map_err(|e| CompAgentError::Fatal(format!("def build: {e}")))?;

    let cwd = dir.to_path_buf();

    let config = GremlinConfig {
        ephemeral: true,
        extra_sandbox_roots: vec![cwd.clone()],
    };
    let mut gremlin = Gremlin::init(
        "drag-maint",
        &cwd,
        &def,
        &HashMap::new(),
        Some(model_spec),
        &config,
    )
    .map_err(|e| CompAgentError::Fatal(format!("gremlin init: {e}")))?;

    if verbose {
        eprintln!("  [{dir_name}] running maint agent (model={model_spec})...");
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

    let json_str = extract_json(&content).ok_or_else(|| {
        CompAgentError::Retryable(format!("no JSON in artifact: {}", truncate(&content, 200)))
    })?;

    let scores: HashMap<String, u8> = serde_json::from_str(&json_str)
        .map_err(|e| CompAgentError::Retryable(format!("parse: {e}")))?;

    validate_maint_scores(&scores, maint_config)?;

    Ok(scores)
}

/// Validate that every configured dimension is present and in the 0–4
/// range.  Extracted as a pure helper for test coverage.
fn validate_maint_scores(
    scores: &HashMap<String, u8>,
    maint_config: &MaintainabilityConfig,
) -> Result<(), CompAgentError> {
    for dim in &maint_config.dimensions {
        match scores.get(&dim.key) {
            None => {
                return Err(CompAgentError::Retryable(format!(
                    "missing dimension {}",
                    dim.key
                )));
            }
            Some(&val) if val > 4 => {
                return Err(CompAgentError::Retryable(format!(
                    "{}={val} out of 0–4 range",
                    dim.key
                )));
            }
            _ => {}
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

/// Compute a BLAKE3-based cache key using a Merkle-tree of all source
/// file contents plus the estimate count and the maintainability config
/// hash.  Any change to any source file, the number of estimates, or the
/// dimension configuration invalidates the entry.
fn compute_dir_cache_key(
    dir: &Path,
    estimates: usize,
    config_hash: &str,
) -> Result<String, String> {
    let files = crate::lang::collect_source_files(dir)
        .map_err(|e| format!("failed to collect source files in {}: {e}", dir.display()))?;

    if files.is_empty() {
        return Err(format!("no source files found in {}", dir.display()));
    }

    // Per-file hashes, sorted by relative path for determinism.
    let mut file_hashes: Vec<(String, String)> = Vec::with_capacity(files.len());
    for f in &files {
        let content =
            std::fs::read(f).map_err(|e| format!("failed to read {}: {e}", f.display()))?;
        let hash = blake3::hash(&content).to_hex();
        let rel = f
            .strip_prefix(dir)
            .unwrap_or(f)
            .to_string_lossy()
            .to_string();
        file_hashes.push((rel, hash.to_string()));
    }
    file_hashes.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"v8\0");
    hasher.update(dir.to_string_lossy().as_bytes());
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

    // ── compute_maint_score_from_map ──

    /// The ten dimensions from the bundled config, used to exercise the
    /// dimension-count-based scoring math.
    const TEN_DIMS: [&str; 10] = [
        "decomposition",
        "interface_design",
        "consistency",
        "dependency_hygiene",
        "tool_awareness",
        "naming",
        "error_handling",
        "state_management",
        "logging",
        "testability",
    ];

    #[test]
    fn compute_maint_score_empty_is_neutral() {
        let dims: HashMap<&str, f64> = HashMap::new();
        assert_eq!(compute_maint_score_from_map(&dims), 0.5);
    }

    #[test]
    fn compute_maint_score_uniform() {
        let dims: HashMap<&str, f64> = TEN_DIMS.iter().map(|&d| (d, 2.0)).collect();
        assert!((compute_maint_score_from_map(&dims) - 0.5).abs() < 1e-10);
    }

    #[test]
    fn compute_maint_score_all_zeros() {
        let dims: HashMap<&str, f64> = TEN_DIMS.iter().map(|&d| (d, 0.0)).collect();
        assert!((compute_maint_score_from_map(&dims) - 0.0).abs() < 1e-10);
    }

    #[test]
    fn compute_maint_score_all_fours() {
        let dims: HashMap<&str, f64> = TEN_DIMS.iter().map(|&d| (d, 4.0)).collect();
        assert!((compute_maint_score_from_map(&dims) - 1.0).abs() < 1e-10);
    }

    #[test]
    fn compute_maint_score_mixed() {
        let mut dims: HashMap<&str, f64> = HashMap::new();
        dims.insert("decomposition", 3.0);
        dims.insert("interface_design", 2.0);
        let score = compute_maint_score_from_map(&dims);
        // sum=5, max=8 → 0.625
        assert!((score - 0.625).abs() < 1e-10);
    }

    // ── build_dir_manifest ──

    #[test]
    fn build_dir_manifest_excludes_nested_files() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        std::fs::create_dir(tmp.path().join("sub")).expect("create sub");
        std::fs::write(tmp.path().join("a.rs"), "fn a() {}").expect("write a.rs");
        std::fs::write(tmp.path().join("sub/b.rs"), "fn b() {}").expect("write b.rs");

        let manifest = build_dir_manifest(tmp.path())
            .expect("manifest")
            .expect("direct files");

        assert!(
            manifest.contains("a.rs"),
            "manifest should list a.rs: {manifest}"
        );
        assert!(
            !manifest.contains("b.rs"),
            "manifest should not list nested b.rs: {manifest}"
        );
    }

    #[test]
    fn build_dir_manifest_only_nested_files_is_empty() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        std::fs::create_dir(tmp.path().join("sub")).expect("create sub");
        std::fs::write(tmp.path().join("sub/b.rs"), "fn b() {}").expect("write b.rs");

        let result = build_dir_manifest(tmp.path());
        assert_eq!(
            result.expect("collection should succeed"),
            None,
            "directories with only nested source files should produce an empty manifest"
        );
    }

    // ── validate_maint_scores ──

    use crate::config::{MaintainabilityConfig, MaintainabilityDimensionDef};

    fn make_config(dim_keys: &[&str]) -> MaintainabilityConfig {
        MaintainabilityConfig {
            preamble: String::new(),
            dimensions: dim_keys
                .iter()
                .map(|&k| MaintainabilityDimensionDef {
                    key: k.to_string(),
                    label: k.to_string(),
                    description: String::new(),
                    anchors: String::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn validate_all_dim_present() {
        let cfg = make_config(&["naming", "cohesion"]);
        let mut scores: HashMap<String, u8> = HashMap::new();
        scores.insert("naming".into(), 3);
        scores.insert("cohesion".into(), 2);
        assert!(validate_maint_scores(&scores, &cfg).is_ok());
    }

    #[test]
    fn validate_missing_dimension() {
        let cfg = make_config(&["naming", "cohesion"]);
        let mut scores: HashMap<String, u8> = HashMap::new();
        scores.insert("naming".into(), 3);
        let err = validate_maint_scores(&scores, &cfg).unwrap_err();
        assert!(matches!(err, CompAgentError::Retryable(_)));
        let msg = err.to_string();
        assert!(msg.contains("missing dimension") && msg.contains("cohesion"));
    }

    #[test]
    fn validate_out_of_range() {
        let cfg = make_config(&["naming"]);
        let mut scores: HashMap<String, u8> = HashMap::new();
        scores.insert("naming".into(), 5);
        let err = validate_maint_scores(&scores, &cfg).unwrap_err();
        assert!(matches!(err, CompAgentError::Retryable(_)));
        assert!(err.to_string().contains("out of 0–4 range"));
    }

    #[test]
    fn validate_extra_keys_allowed() {
        // Extra keys in the JSON are fine — the agent may add commentary keys.
        let cfg = make_config(&["naming"]);
        let mut scores: HashMap<String, u8> = HashMap::new();
        scores.insert("naming".into(), 3);
        scores.insert("extra".into(), 2);
        assert!(validate_maint_scores(&scores, &cfg).is_ok());
    }

    #[test]
    fn validate_empty_config() {
        let cfg = make_config(&[]);
        let scores: HashMap<String, u8> = HashMap::new();
        assert!(validate_maint_scores(&scores, &cfg).is_ok());
    }

    // ── MaintCacheEntry round-trip ──

    #[test]
    fn maint_cache_entry_round_trip() {
        let dir = std::env::temp_dir().join("drag-test-maint-roundtrip");
        let _ = std::fs::create_dir_all(&dir);
        let cache_path = dir.join("maint-cache.jsonl");
        let raw_estimates: Vec<DirMaintainability> = vec![[
            ("modularity", 3),
            ("interface_cohesion", 2),
            ("error_propagation", 1),
            ("dependency_harmony", 2),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()];
        let entry = MaintCacheEntry {
            key: "b33fdeadb33fdead".to_string(),
            dir: "test-dir".to_string(),
            model: "openai/gpt-4o".to_string(),
            estimates: 1,
            raw_estimates,
        };
        append_maint_cache_entry(&cache_path, &entry).unwrap();
        let map = load_maint_cache(&cache_path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(map.len(), 1);
        let loaded = map.get("b33fdeadb33fdead").unwrap();
        assert_eq!(loaded.raw_estimates.len(), 1);
        assert_eq!(loaded.raw_estimates[0].get("modularity"), Some(&3));
        assert_eq!(loaded.dir, "test-dir");
        assert_eq!(loaded.model, "openai/gpt-4o");
    }

    #[test]
    fn maint_cache_entry_deser_old_format_falls_back() {
        // Old cache entries with maint_score/breakdown (instead of raw_estimates)
        // should deserialize with an empty raw_estimates vec, triggering re-score.
        let dir = std::env::temp_dir().join("drag-test-maint-oldformat");
        let _ = std::fs::create_dir_all(&dir);
        let cache_path = dir.join("maint-cache.jsonl");
        std::fs::write(
            &cache_path,
            "{\"key\":\"oldkey0000000000\",\"dir\":\"dir\",\"model\":\"m\",\"estimates\":3,\"maint_score\":0.6,\"breakdown\":{\"a\":3}}\n",
        )
        .unwrap();
        let map = load_maint_cache(&cache_path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(map.len(), 1);
        let entry = map.get("oldkey0000000000").unwrap();
        assert!(entry.raw_estimates.is_empty());
    }

    /// Legacy `package` field is accepted via #[serde(alias = "package")].
    #[test]
    fn maint_cache_entry_deser_legacy_package_field() {
        let dir = std::env::temp_dir().join("drag-test-maint-legacypkg");
        let _ = std::fs::create_dir_all(&dir);
        let cache_path = dir.join("maint-cache.jsonl");
        std::fs::write(
            &cache_path,
            "{\"key\":\"legacy0000000000\",\"package\":\"legacy-pkg\",\"model\":\"m\",\"estimates\":1,\"raw_estimates\":[{\"a\":3}]}\n",
        )
        .unwrap();
        let map = load_maint_cache(&cache_path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(map.len(), 1);
        let entry = map.get("legacy0000000000").unwrap();
        assert_eq!(entry.dir, "legacy-pkg");
        assert_eq!(entry.raw_estimates.len(), 1);
    }
}
