use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;

use gremlins::builders::{output, AgentBuilder, DefinitionBuilder};
use gremlins::executor::gremlin::{Gremlin, GremlinConfig};
use gremlins::model_uri::ModelURI;

use crate::cache::{append_cache_entry, compute_cache_key, CacheEntry};
use crate::config::ComprehensibilityConfig;
use crate::scoring::ModuleDrag;
use crate::util::{
    extract_json, mean_with_ci, truncate, CacheState, CompAgentError, ComprehensibilityScores,
};

const MAX_COMP_RETRIES: u32 = 3;
const COMP_BACKOFF: [f64; 2] = [1.0, 2.0];

/// Handle for a single comprehensibility scoring task.
/// On success returns all the info needed to compute the mean and write the cache.
/// On failure returns a human-readable reason.
type ComprehensionTask = tokio::task::JoinHandle<Result<CompEstimate, String>>;

/// Result of a single comprehensibility estimate — returned by each spawned task.
pub(crate) struct CompEstimate {
    module: String,
    /// Per-dimension 0–4 scores from the LLM. Aggregation is deferred.
    dim_scores: ComprehensibilityScores,
}

/// Resolve the model to use for comprehensibility scoring.
/// If `--model` was given, resolve it through profiles.
/// Otherwise, use the gremlins default model.
pub(crate) fn resolve_comprehensibility_model(model_opt: Option<&str>) -> Result<String, String> {
    gremlins::config::init_global().map_err(|e| e.to_string())?;
    let spec = match model_opt {
        Some(spec) => spec.to_string(),
        None => {
            let config = gremlins::config::global_config().map_err(|e| e.to_string())?;
            config.default_model().map(String::from).ok_or_else(|| {
                "no default model configured — set one in settings.yaml or pass --model".to_string()
            })?
        }
    };
    gremlins::config::resolve_model_with_global(&spec)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn score_comprehensibility(
    model_spec: &str,
    verbose: bool,
    estimates: usize,
    modules: &[(String, PathBuf, String)],
    scores: &mut [ModuleDrag],
    cache_state: CacheState,
    module_cache: HashMap<String, CacheEntry>,
    comprehensibility_config: &ComprehensibilityConfig,
    config_hash: &str,
    scoring_sem: Arc<Semaphore>,
) -> Result<(), String> {
    // Validate dimensions before spawning any LLM tasks.
    if comprehensibility_config.dimensions.is_empty() {
        return Err("comprehensibility config has no dimensions".to_string());
    }

    // `model_spec` is already fully resolved by `resolve_comprehensibility_model`.
    // Diagnostics are best-effort — model construction may fail for cmd/… backends.
    if verbose {
        use gremlins::clients::client::Client;
        if let Ok(client) = Client::parse(model_spec) {
            eprintln!("╔══ model diagnostics ══╗");
            eprintln!("║ model spec:      {model_spec}");
            eprintln!("║ client:          {client}");
            if let Ok(backend) = client.get_or_build_backend() {
                if let Some(model) = backend.make_model(model_spec) {
                    eprintln!("║ model name:      {}", model.name());
                    if let Some(id) = model.id() {
                        eprintln!("║ model id:        {id}");
                    }
                }
                if let Some(ref rp) = backend.resolved_params() {
                    eprintln!("║ backend temp:    {:?}", rp.temperature);
                    eprintln!("║ backend max_tok: {:?}", rp.max_tokens);
                }
            }
            eprintln!("╚═══════════════════╝");
        }
    }

    eprintln!("Scoring comprehensibility for {} modules...", modules.len());

    // Single pass: for each module, check the pre-computed module_cache.
    // Cache hits produce `estimates` copies of the cached score directly.
    // Cache misses spawn `estimates` LLM tasks.
    let mut handles: Vec<ComprehensionTask> = Vec::new();

    // Cache hits store raw_estimates here (Vec of per-estimate dim scores).
    // Cache misses are collected into estimates_map below, then both
    // paths feed through the same inline aggregation loop at the end of
    // this function.
    let mut by_module: HashMap<String, Vec<ComprehensibilityScores>> = HashMap::new();

    // Build a module → token count lookup from scores for cache-miss writeback.
    let module_tokens: HashMap<String, usize> = scores
        .iter()
        .map(|s| (s.module.clone(), s.token_count))
        .collect();

    for (module, file, _pkg) in modules.iter() {
        // ── cache hit (pre-computed in run()) ──
        if let Some(entry) = module_cache.get(module.as_str()) {
            eprintln!("  [{module}] cache hit ({})", entry.key);
            by_module.insert(module.clone(), entry.raw_estimates.clone());
            continue;
        }

        // ── cache miss or no cache: spawn LLM tasks ──
        let source = match std::fs::read_to_string(file) {
            Ok(s) => s,
            Err(e) => {
                return Err(format!("[{module}] failed to read source: {e}"));
            }
        };
        if source.trim().is_empty() {
            // Empty files: all dimensions at 4 (perfect score).
            let dim_keys: HashMap<String, u8> = comprehensibility_config
                .dimensions
                .iter()
                .map(|d| (d.key.clone(), 4u8))
                .collect();
            by_module.insert(module.clone(), vec![dim_keys; estimates]);
            continue;
        }

        let abs_path = std::fs::canonicalize(file)
            .unwrap_or_else(|_| file.clone())
            .to_string_lossy()
            .into_owned();

        let fence = crate::lang::code_fence(file);
        let prompt = format!("SOURCE FILE ({module}):\n\n```{fence}\n{source}\n```");

        let token_count = module_tokens.get(module.as_str()).copied().unwrap_or(0);

        let cache_key = compute_cache_key(&source, model_spec, estimates, config_hash);

        for _ in 0..estimates {
            let module = module.clone();
            let source = source.clone();
            let abs_path = abs_path.clone();
            let prompt = prompt.clone();
            let model_spec = model_spec.to_string();
            let comp_config = comprehensibility_config.clone();
            let sem = Arc::clone(&scoring_sem);
            let cache_path = cache_state.clone();
            let cache_key = cache_key.clone();

            handles.push(tokio::spawn(async move {
                for attempt in 0..MAX_COMP_RETRIES {
                    // Acquire a semaphore permit before creating the
                    // ephemeral gremlin so total open file descriptors
                    // stay bounded.  The permit is dropped at the end of
                    // each attempt so a sleeping/backoff task does not
                    // hold a scoring slot.
                    let _permit = sem
                        .acquire()
                        .await
                        .expect("scoring semaphore closed — cannot proceed without a permit");
                    match run_comp_agent(
                        &module,
                        &source,
                        &prompt,
                        &model_spec,
                        verbose,
                        &comp_config,
                    )
                    .await
                    {
                        Ok(dim_scores) => {
                            // Persist this estimate immediately so a partial
                            // run still records whatever succeeded.  A cache
                            // write failure must not fail the estimate.
                            let entry = CacheEntry {
                                key: cache_key.clone(),
                                file: abs_path.clone(),
                                model: model_spec.clone(),
                                token_count,
                                estimates,
                                raw_estimates: vec![dim_scores.clone()],
                            };
                            if let Err(e) = append_cache_entry(&cache_path, &entry) {
                                eprintln!(
                                    "  [{module}] WARNING: failed to append cache entry: {e}"
                                );
                            }
                            return Ok(CompEstimate { module, dim_scores });
                        }
                        Err(e) => {
                            // Drop the permit before sleeping.
                            drop(_permit);
                            if matches!(&e, CompAgentError::Retryable(_))
                                && attempt + 1 < MAX_COMP_RETRIES
                            {
                                eprintln!(
                                    "  [{module}] attempt {}/{}: {e} — retrying in {:.0}s",
                                    attempt + 1,
                                    MAX_COMP_RETRIES,
                                    COMP_BACKOFF[attempt as usize]
                                );
                                tokio::time::sleep(Duration::from_secs_f64(
                                    COMP_BACKOFF[attempt as usize],
                                ))
                                .await;
                                continue;
                            }
                            return Err(format!(
                                "[{module}] failed after {} attempts: {}",
                                attempt + 1,
                                match &e {
                                    CompAgentError::Retryable(s) | CompAgentError::Fatal(s) =>
                                        s.as_str(),
                                }
                            ));
                        }
                    }
                }

                Err(format!("[{module}] exhausted {} retries", MAX_COMP_RETRIES))
            }));
        }
    }

    // Collect task results into estimates_map (cache-miss modules only).
    let mut estimates_map: HashMap<String, Vec<CompEstimate>> = HashMap::new();
    for handle in handles {
        let est = handle.await.map_err(|e| format!("join error: {e}"))?;
        match est {
            Ok(est) => {
                estimates_map
                    .entry(est.module.clone())
                    .or_default()
                    .push(est);
            }
            Err(e) => {
                eprintln!("  WARNING: {e}");
                continue;
            }
        }
    }

    let dim_count = comprehensibility_config.dimensions.len();

    for (module, _file, _pkg) in modules.iter() {
        // Gather raw estimates: cache-miss from estimates_map, cache-hit from by_module.
        let raw_estimates: Vec<&ComprehensibilityScores> =
            if let Some(estimates_vec) = estimates_map.get(module.as_str()) {
                if estimates_vec.len() != estimates {
                    eprintln!(
                        "  [{module}] WARNING: expected {estimates} estimates, got {} — skipping",
                        estimates_vec.len()
                    );
                    continue;
                }
                // Borrow dim_scores from estimates_vec for aggregation.
                estimates_vec.iter().map(|e| &e.dim_scores).collect()
            } else if let Some(raw_vec) = by_module.get(module.as_str()) {
                if raw_vec.len() != estimates {
                    eprintln!(
                    "  [{module}] WARNING: expected {estimates} raw estimates, got {} — skipping",
                    raw_vec.len()
                );
                    continue;
                }
                raw_vec.iter().collect()
            } else {
                eprintln!("  [{module}] WARNING: no estimates collected — skipping");
                continue;
            };

        // Aggregate: per-estimate 0-1 scores, then mean with CI.
        let dim_keys: Vec<&str> = comprehensibility_config
            .dimensions
            .iter()
            .map(|d| d.key.as_str())
            .collect();
        let per_estimate: Vec<f64> = raw_estimates
            .iter()
            .map(|dims| {
                let sum: f64 = dim_keys
                    .iter()
                    .map(|k| *dims.get(*k).unwrap_or(&0) as f64)
                    .sum();
                (sum / (dim_count * 4) as f64).clamp(0.0, 1.0)
            })
            .collect();
        let (com, lo, hi) = mean_with_ci(&per_estimate);
        if com >= 1.0 {
            eprintln!("  [{module}] WARNING: perfect score (all 4s)");
        }
        eprintln!("  {module}: {com:.3} (95% CI: {lo:.3}–{hi:.3})");
        let s = match scores.iter_mut().find(|s| s.module == *module) {
            Some(s) => s,
            None => {
                eprintln!(
                    "  [{module}] WARNING: score computed but no matching ModuleDrag found — skipping"
                );
                continue;
            }
        };
        s.comprehensibility = Some(com);
        s.ci_lower = Some(lo);
        s.ci_upper = Some(hi);
    }

    Ok(())
}

/// Run a single comprehensibility estimate through an ephemeral gremlin Agent stage.
///
/// The Agent stage's built-in loop handles retries, stream errors, timeouts,
/// and nudges the model when it produces text but doesn't write the expected
/// artifact.  On a completely empty response the loop breaks, the missing
/// artifact causes a bail, and our outer retry loop re-runs the whole gremlin.
async fn run_comp_agent(
    module: &str,
    source: &str,
    prompt: &str,
    model_spec: &str,
    verbose: bool,
    comprehensibility_config: &ComprehensibilityConfig,
) -> Result<ComprehensibilityScores, CompAgentError> {
    // Validate we have dimensions to score — an empty config would produce
    // NaN scores and nonsensical prompts.
    if comprehensibility_config.dimensions.is_empty() {
        return Err(CompAgentError::Fatal(
            "comprehensibility config has no dimensions".to_string(),
        ));
    }

    let model_uri = ModelURI::parse(model_spec)
        .map_err(|e| CompAgentError::Fatal(format!("invalid model: {e}")))?;

    // Assemble the scoring prompt dynamically from the comprehensibility config.
    let dim_list = comprehensibility_config
        .dimensions
        .iter()
        .map(|d| format!("— {}: {}", d.key, d.description))
        .collect::<Vec<_>>()
        .join("\n");

    let structural = comprehensibility_config
        .dimensions
        .iter()
        .filter_map(|d| {
            comprehensibility_config
                .structural_anchors
                .get(&d.key)
                .map(|a| format!("{}: {}", d.key, a))
        })
        .collect::<Vec<_>>()
        .join("\n");

    let structural_section = if structural.is_empty() {
        String::new()
    } else {
        format!("\n\nAnchors for the structural dimensions:\n{structural}")
    };

    let dim_keys = comprehensibility_config
        .dimensions
        .iter()
        .map(|d| d.key.clone())
        .collect::<Vec<_>>()
        .join(", ");

    let stage = AgentBuilder::new("score")
        .option("src", source)
        .prompt(format!(
            "{prompt}\n\n{preamble}\n\n\
             Score {n} dimensions on a 0-4 scale:\n{dim_list}\n\n\
             {generic_anchors}{structural_section}\n\n\
             Produce a JSON object with these fields: {dim_keys}.\n\
             Write the JSON to {{comprehensibility}}.",
            preamble = comprehensibility_config.preamble,
            n = comprehensibility_config.dimensions.len(),
            generic_anchors = comprehensibility_config.generic_anchors,
        ))
        .output("comprehensibility", output("artifact://comprehensibility"))
        .landscape(false)
        .model(model_uri.clone())
        .build()
        .map_err(|e| CompAgentError::Fatal(format!("agent build: {e}")))?;

    let def = DefinitionBuilder::new("drag-comp", model_uri)
        .stage(stage)
        .build()
        .map_err(|e| CompAgentError::Fatal(format!("def build: {e}")))?;

    // Gremlin::init canonicalizes definition_path to derive the project root.
    // For ephemeral runs the worktree is a temp dir so the project root is
    // irrelevant — we just need an extant path.
    let cwd = std::env::current_dir().map_err(|e| CompAgentError::Fatal(format!("cwd: {e}")))?;

    let config = GremlinConfig {
        ephemeral: true,
        ..Default::default()
    };
    let mut gremlin = Gremlin::init(
        "drag-comp",
        &cwd,
        &def,
        &HashMap::new(),
        Some(model_spec),
        &config,
    )
    .map_err(|e| CompAgentError::Fatal(format!("gremlin init: {e}")))?;

    if verbose {
        eprintln!("  [{module}] running agent stage (model={model_spec})...");
    }

    let exit_code = gremlin
        .run(None)
        .await
        .map_err(|e| CompAgentError::Fatal(format!("gremlin run: {e}")))?;

    if exit_code != 0 {
        return Err(CompAgentError::Fatal(format!(
            "agent stage exited {exit_code} (bail/missing artifact)"
        )));
    }

    let content = gremlin
        .state
        .content("artifact://comprehensibility", None)
        .await
        .map_err(|e| CompAgentError::Fatal(format!("artifact read: {e}")))?;

    let json_str = extract_json(&content).ok_or_else(|| {
        CompAgentError::Retryable(format!("no JSON in artifact: {}", truncate(&content, 200)))
    })?;

    let scores: ComprehensibilityScores = serde_json::from_str(&json_str)
        .map_err(|e| CompAgentError::Retryable(format!("parse: {e}")))?;

    // Validate every configured dimension is present and in the 0–4 range.
    // A missing or out-of-range value means the model misbehaved — retryable.
    for dim in &comprehensibility_config.dimensions {
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

    Ok(scores)
}
