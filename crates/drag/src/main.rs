mod lang;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use serde::{Deserialize, Serialize};

use gremlins::builders::{output, AgentBuilder, DefinitionBuilder};
use gremlins::executor::gremlin::{Gremlin, GremlinConfig};
use gremlins::model_uri::ModelURI;

use lang::PackageInfo;

/// Compute drag scores for source modules.
#[derive(Parser)]
#[command(name = "gremlins-drag")]
struct Cli {
    /// Source file or directory to score. Recognized extensions: .rs, .py.
    /// If a file, scores that module. If a directory, scores all modules
    /// under it.
    path: PathBuf,

    /// Output JSON instead of a human-readable table.
    #[arg(long)]
    json: bool,

    /// Also score comprehensibility via LLM (requires a configured model).
    #[arg(long)]
    comprehensibility: bool,

    /// Model spec for comprehensibility scoring (e.g. "openai/gpt-4o").
    /// Defaults to the gremlins default model.
    #[arg(long)]
    model: Option<String>,

    /// Tokenizer name for tiktoken-rs (e.g. "o200k_base", "cl100k_base").
    #[arg(long, default_value = "o200k_base")]
    tokenizer: String,

    /// Weight for token_count in the ln_drag formula.
    #[arg(long, default_value_t = 1.0)]
    w1: f64,

    /// Weight for graph_complexity in the ln_drag formula.
    #[arg(long, default_value_t = 0.4)]
    w2: f64,

    /// Weight for comprehensibility in the ln_drag formula (subtracted).
    /// Only applied when `--comprehensibility` is set.
    #[arg(long, default_value_t = 0.4)]
    w3: f64,

    /// Added to comprehensibility before taking ln() to avoid ln(0).
    #[arg(long, default_value_t = 0.01)]
    epsilon: f64,

    /// Print the resolved model spec and the full request body sent to the API.
    #[arg(short = 'v', long)]
    verbose: bool,

    /// Number of independent LLM estimates per module for comprehensibility
    /// scoring (median is used). Default 1. Only meaningful with
    /// `--comprehensibility`.
    #[arg(long, default_value_t = 1)]
    comprehensibility_estimates: usize,

    /// Path to a JSONL cache file for comprehensibility scores.
    /// When set, scores are stored keyed by BLAKE3 hash of
    /// (file path, file content, model URI). On re-runs, cached
    /// scores are reused, eliminating redundant LLM calls.
    /// Without `--comprehensibility` this flag is ignored.
    #[arg(long)]
    cache: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Output types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct DragReport {
    config: DragConfig,
    modules: Vec<ModuleDrag>,
}

#[derive(Serialize)]
struct DragConfig {
    w1: f64,
    w2: f64,
    w3: f64,
    epsilon: f64,
}

#[derive(Serialize)]
struct ModuleDrag {
    module: String,
    file: String,
    token_count: usize,
    fan_in: usize,
    fan_out: usize,
    graph_complexity: usize,
    comprehensibility: Option<f64>,
    /// `exp(ln_drag)`. A monotonic transform of `ln_drag`; it adds no
    /// additional ranking information.
    drag: f64,
    /// The drag metric: `w1*ln(tokens) + w2*ln(complexity) - w3*ln(comp)`.
    ln_drag: f64,
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("gremlins-drag: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let cli = Cli::parse();

    // ── guard ──
    if cli.cache.is_some() && !cli.comprehensibility {
        return Err("--cache requires --comprehensibility".to_string());
    }

    // 1. Discover packages (Rust + Python, best-effort)
    let packages = lang::discover_packages();

    // 2. Resolve the input path to a list of recognized source files
    let target_files = resolve_path_files(&cli.path)?;

    // 3. Map files to module paths, tracking which package each belongs to.
    let mut target_modules: Vec<(String, PathBuf, String)> = Vec::new(); // (module, file, pkg_key)
    for file in &target_files {
        if let Some((module, pkg_key)) = lang::file_to_module(file, &packages) {
            target_modules.push((module, file.clone(), pkg_key));
        } else {
            eprintln!("warning: could not map to module: {}", file.display());
        }
    }
    if target_modules.is_empty() {
        let exts = lang::extensions()
            .iter()
            .map(|e| format!(".{e}"))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "no recognized modules ({exts}) found at: {}",
            cli.path.display()
        ));
    }

    // ── resolve model & load cache early (both needed for key computation) ──
    let model_spec: Option<String> = if cli.comprehensibility {
        Some(resolve_comprehensibility_model(cli.model.as_deref())?)
    } else {
        None
    };

    let cache_map: Option<HashMap<String, CacheEntry>> = match &cli.cache {
        Some(cache_path) => {
            let map = load_cache(cache_path)?;
            eprintln!(
                "Loaded {} cached entries from {}",
                map.len(),
                cache_path.display()
            );
            Some(map)
        }
        None => None,
    };

    // ── pre-compute cache keys and build module → CacheEntry lookup ──
    let mut module_cache: HashMap<String, CacheEntry> = HashMap::new();
    if let (Some(ref cache_map), Some(ref ms)) = (&cache_map, &model_spec) {
        for (module, file, _pkg) in &target_modules {
            let source = std::fs::read_to_string(file).unwrap_or_default();
            if source.trim().is_empty() {
                continue;
            }
            let abs_path = std::fs::canonicalize(file)
                .unwrap_or_else(|_| file.clone())
                .to_string_lossy()
                .into_owned();
            let key = compute_cache_key(&abs_path, &source, ms);
            if let Some(entry) = cache_map.get(&key) {
                module_cache.insert(module.clone(), entry.clone());
            }
        }
    }
    let cache_hit_count = module_cache.len();
    if cache_hit_count > 0 {
        eprintln!("  {cache_hit_count} modules have complete cache hits");
    }

    // 4. Build dependency graph (fan-out per package in parallel).
    let mut edges: Vec<(String, String)> = Vec::new();

    let relevant_packages: HashSet<String> = target_modules
        .iter()
        .map(|(_, _, pkg_key)| pkg_key.clone())
        .collect();

    let scoring_packages: Vec<&PackageInfo> = packages
        .iter()
        .filter(|p| relevant_packages.contains(&p.key()))
        .collect();

    // Run dependency-graph extraction for each package in parallel.
    let mut handles = Vec::new();
    for pkg in &scoring_packages {
        let pkg = (*pkg).clone();
        handles.push(tokio::task::spawn_blocking(move || {
            lang::dependency_graph(&pkg)
        }));
    }
    for handle in handles {
        let pkg_edges = handle.await.map_err(|e| format!("join error: {e}"))??;
        edges.extend(pkg_edges);
    }

    // 5. Compute fan-in / fan-out
    let (fan_out_map, fan_in_map) = compute_degrees(&edges);

    // 6. Count tokens and assemble per-module metrics.
    //    Use cached values when available; compute fresh otherwise.
    let tokenizer = parse_tokenizer(&cli.tokenizer)?;
    let bpe = tiktoken_rs::bpe_for_tokenizer(tokenizer)
        .map_err(|e| format!("failed to load tokenizer {}: {e}", cli.tokenizer))?;

    let mut scores: Vec<ModuleDrag> = Vec::new();
    for (module, file, _pkg_key) in &target_modules {
        let (token_count, fan_out, fan_in, graph_complexity) =
            if let Some(entry) = module_cache.get(module.as_str()) {
                (
                    entry.token_count,
                    entry.fan_out,
                    entry.fan_in,
                    entry.graph_complexity,
                )
            } else {
                let tc = count_tokens(bpe, file)?;
                let fo = fan_out_map.get(module).copied().unwrap_or(0);
                let fi = fan_in_map.get(module).copied().unwrap_or(0);
                (tc, fo, fi, fo + fi)
            };

        scores.push(ModuleDrag {
            module: module.clone(),
            file: file.to_string_lossy().into_owned(),
            token_count,
            fan_in,
            fan_out,
            graph_complexity,
            comprehensibility: None,
            drag: 0.0,
            ln_drag: 0.0,
        });
    }

    // 7. Comprehensibility (optional). Failures skip individual modules
    //    rather than aborting the whole run.
    if cli.comprehensibility {
        if cli.comprehensibility_estimates < 1 {
            return Err("--comprehensibility-estimates must be >= 1".to_string());
        }
        let model_spec = model_spec.as_ref().unwrap();

        // Use the already-loaded cache.
        let cache_state: Option<CacheState> = match &cli.cache {
            Some(cache_path) => {
                let map = cache_map.as_ref().unwrap().clone();
                Some((Arc::new(Mutex::new(map)), cache_path.clone()))
            }
            None => None,
        };

        score_comprehensibility(
            model_spec,
            cli.verbose,
            cli.comprehensibility_estimates,
            &target_modules,
            &mut scores,
            cache_state,
            module_cache,
        )
        .await?;
    }

    // 8. Compute the final drag scores once, after any scoring.
    for s in &mut scores {
        s.ln_drag = compute_ln_drag(
            cli.w1,
            cli.w2,
            cli.w3,
            cli.epsilon,
            s.token_count,
            s.graph_complexity,
            s.comprehensibility,
        );
        s.drag = s.ln_drag.exp();
    }

    // 9. Sort by ln_drag descending
    scores.sort_by(|a, b| {
        b.ln_drag
            .partial_cmp(&a.ln_drag)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // 10. Output
    if cli.json {
        let report = DragReport {
            config: DragConfig {
                w1: cli.w1,
                w2: cli.w2,
                w3: cli.w3,
                epsilon: cli.epsilon,
            },
            modules: scores,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
        );
    } else {
        print_human_table(&scores);
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// Resolve a path to a list of recognized source files.
fn resolve_path_files(path: &Path) -> Result<Vec<PathBuf>, String> {
    let canonical = path
        .canonicalize()
        .map_err(|e| format!("{}: {e}", path.display()))?;

    if canonical.is_file() {
        let ext = canonical.extension().and_then(|e| e.to_str()).unwrap_or("");
        if lang::extensions().contains(&ext) {
            return Ok(vec![canonical]);
        }
        let exts = lang::extensions()
            .iter()
            .map(|e| format!(".{e}"))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "not a recognized source file ({exts}): {}",
            canonical.display()
        ));
    }

    if canonical.is_dir() {
        let files = lang::collect_source_files(&canonical)
            .map_err(|e| format!("{}: {e}", canonical.display()))?;
        if files.is_empty() {
            let exts = lang::extensions()
                .iter()
                .map(|e| format!(".{e}"))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "no source files ({exts}) found in: {}",
                canonical.display()
            ));
        }
        return Ok(files);
    }

    Err(format!("not found: {}", path.display()))
}

// ---------------------------------------------------------------------------
// Tokenizer parsing
// ---------------------------------------------------------------------------

fn parse_tokenizer(name: &str) -> Result<tiktoken_rs::tokenizer::Tokenizer, String> {
    match name {
        "o200k_base" => Ok(tiktoken_rs::tokenizer::Tokenizer::O200kBase),
        "o200k_harmony" => Ok(tiktoken_rs::tokenizer::Tokenizer::O200kHarmony),
        "cl100k_base" => Ok(tiktoken_rs::tokenizer::Tokenizer::Cl100kBase),
        "p50k_base" => Ok(tiktoken_rs::tokenizer::Tokenizer::P50kBase),
        "r50k_base" => Ok(tiktoken_rs::tokenizer::Tokenizer::R50kBase),
        "p50k_edit" => Ok(tiktoken_rs::tokenizer::Tokenizer::P50kEdit),
        "gpt2" => Ok(tiktoken_rs::tokenizer::Tokenizer::Gpt2),
        _ => Err(format!(
            "unknown tokenizer {name:?}. Available: o200k_base, o200k_harmony, cl100k_base, p50k_base, r50k_base, p50k_edit, gpt2"
        )),
    }
}

// ---------------------------------------------------------------------------
// Graph computations
// ---------------------------------------------------------------------------

/// Compute fan-out and fan-in maps from edge list.
fn compute_degrees(edges: &[(String, String)]) -> (HashMap<String, usize>, HashMap<String, usize>) {
    let mut fan_out: HashMap<String, usize> = HashMap::new();
    let mut fan_in: HashMap<String, usize> = HashMap::new();

    for (src, dst) in edges {
        *fan_out.entry(src.clone()).or_insert(0) += 1;
        *fan_in.entry(dst.clone()).or_insert(0) += 1;
    }

    (fan_out, fan_in)
}

// ---------------------------------------------------------------------------
// Token counting
// ---------------------------------------------------------------------------

fn count_tokens(tokenizer: &tiktoken_rs::CoreBPE, file: &Path) -> Result<usize, String> {
    let source = std::fs::read_to_string(file)
        .map_err(|e| format!("failed to read {}: {e}", file.display()))?;
    if source.is_empty() {
        return Ok(0);
    }
    Ok(tokenizer.encode_with_special_tokens(&source).len())
}

/// Compute the log-drag score for a module.
///
/// When comprehensibility has not been scored, the comprehensibility term is
/// omitted entirely rather than assuming a perfect score.
fn compute_ln_drag(
    w1: f64,
    w2: f64,
    w3: f64,
    epsilon: f64,
    token_count: usize,
    graph_complexity: usize,
    comprehensibility: Option<f64>,
) -> f64 {
    let ln = w1 * (token_count.max(1) as f64).ln() + w2 * (graph_complexity.max(1) as f64).ln();
    match comprehensibility {
        Some(c) => ln - w3 * (c + epsilon).ln(),
        None => ln,
    }
}

// ---------------------------------------------------------------------------
// Comprehensibility scoring
// ---------------------------------------------------------------------------

const MAX_COMP_RETRIES: u32 = 3;
const COMP_BACKOFF: [f64; 2] = [1.0, 2.0];

#[derive(Serialize, Deserialize, Debug, Clone)]
struct ComprehensibilityScores {
    naming: u8,
    control_flow: u8,
    data_flow: u8,
    abstractions: u8,
}

/// One line in the JSONL cache file.
///
/// The cache key is model-dependent: `blake3(abs_path || "\0" || content || "\0" || model_uri)`.
/// Token counts and graph metrics are model-independent but ride along on the same key so
/// the cache stays self-consistent — every entry is a full snapshot of all drag dimensions
/// for a given (file, content, model) tuple.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct CacheEntry {
    /// BLAKE3 hash of (abs_path || "\0" || content || "\0" || model_uri),
    /// hex-encoded, first 16 chars. This is the lookup key.
    key: String,
    /// Absolute path of the source file at scoring time.
    file: String,
    /// Fully-resolved model URI used for scoring.
    model: String,
    /// Token count from tiktoken BPE (model-independent; stored here for convenience).
    token_count: usize,
    /// Fan-in count from the dependency graph (model-independent).
    fan_in: usize,
    /// Fan-out count from the dependency graph (model-independent).
    fan_out: usize,
    /// Graph complexity = fan_in + fan_out.
    graph_complexity: usize,
    /// Median comprehensibility score (0–1).
    comprehensibility: f64,
    /// The four dimension scores from the LLM response.
    breakdown: ComprehensibilityScores,
}

/// Compute a BLAKE3-based cache key from an absolute file path, the file's
/// full source text, and the resolved model URI.
///
/// Key = first 16 hex chars of `blake3(abs_path || "\0" || content || "\0" || model_uri)`.
fn compute_cache_key(abs_path: &str, content: &str, model_uri: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(abs_path.as_bytes());
    hasher.update(b"\0");
    hasher.update(content.as_bytes());
    hasher.update(b"\0");
    hasher.update(model_uri.as_bytes());
    hasher.finalize().to_hex()[..16].to_string()
}

/// Load a JSONL cache file, returning a map keyed by `CacheEntry::key`.
///
/// An empty or missing file is treated as a cold cache (empty map).
/// Any line that is not valid JSON or fails to deserialize as a [`CacheEntry`]
/// produces an error that includes the file path and 1-based line number.
fn load_cache(path: &Path) -> Result<HashMap<String, CacheEntry>, String> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read cache file {}: {e}", path.display()))?;
    let mut map = HashMap::new();
    for (line_no, line) in content.lines().enumerate() {
        let line = line.trim();
        let entry: CacheEntry = serde_json::from_str(line).map_err(|e| {
            format!(
                "{}:{}: invalid cache entry: {e}",
                path.display(),
                line_no + 1
            )
        })?;
        map.insert(entry.key.clone(), entry);
    }
    Ok(map)
}

/// Append one [`CacheEntry`] as a JSON line to the cache file.
///
/// Creates the file (and parent directories) if they do not exist.
fn append_cache_entry(path: &Path, entry: &CacheEntry) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create cache dir {}: {e}", parent.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("failed to open cache file {}: {e}", path.display()))?;
    let mut line = serde_json::to_string(entry).map_err(|e| format!("serialization error: {e}"))?;
    line.push('\n');
    std::io::Write::write_all(&mut file, line.as_bytes())
        .map_err(|e| format!("failed to write cache entry: {e}"))?;
    Ok(())
}

/// Whether a string error from [`run_comp_agent`] is worth retrying.
///
/// Bail / missing-artifact errors happen when the model returned empty
/// (the agent loop breaks, the artifact is never written, and the stage
/// bails).  Run errors wrap underlying network/provider failures that
/// the agent loop exhausted its own retries on.
fn is_retryable_comp_error_str(msg: &str) -> bool {
    msg.contains("bail")
        || msg.contains("missing artifact")
        || msg.contains("gremlin run")
}

/// Resolve the model to use for comprehensibility scoring.
/// If `--model` was given, resolve it through profiles.
/// Otherwise, use the gremlins default model.
fn resolve_comprehensibility_model(cli_model: Option<&str>) -> Result<String, String> {
    gremlins::config::init_global().map_err(|e| e.to_string())?;
    let spec = match cli_model {
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

/// Shared comprehensibility cache: a lock-protected map plus the JSONL file it
/// is persisted to.
type CacheState = (Arc<Mutex<HashMap<String, CacheEntry>>>, PathBuf);

/// Handle for a single comprehensibility scoring task.
/// On success returns all the info needed to compute the median and write the cache.
/// On failure returns a human-readable reason.
type ComprehensionTask = tokio::task::JoinHandle<Result<CompEstimate, String>>;

/// Result of a single comprehensibility estimate — returned by each spawned task.
struct CompEstimate {
    module: String,
    score: f64,
    /// Fields for deferred cache writing (after median is computed).
    abs_path: String,
    model_spec: String,
    cache_key: Option<String>,
    cached_metrics: Option<(usize, usize, usize, usize)>,
    breakdown: ComprehensibilityScores,
}

async fn score_comprehensibility(
    model_spec: &str,
    verbose: bool,
    estimates: usize,
    modules: &[(String, PathBuf, String)],
    scores: &mut [ModuleDrag],
    cache_state: Option<CacheState>,
    module_cache: HashMap<String, CacheEntry>,
) -> Result<(), String> {
    // `model_spec` is already fully resolved by `resolve_comprehensibility_model`.
    use gremlins::clients::client::Client;
    let client =
        Client::parse(model_spec).map_err(|e| format!("invalid model spec {model_spec:?}: {e}"))?;
    let backend = client
        .get_or_build_backend()
        .map_err(|e| format!("failed to build backend for {model_spec}: {e}"))?;

    let model = backend
        .make_model(model_spec)
        .ok_or_else(|| format!("backend cannot build model for {model_spec}"))?;

    // ── verbose: dump model info ──
    if verbose {
        eprintln!("╔══ model diagnostics ══╗");
        eprintln!("║ model spec:      {model_spec}");
        eprintln!("║ client:          {client}");
        eprintln!("║ model name:      {}", model.name());
        if let Some(id) = model.id() {
            eprintln!("║ model id:        {id}");
        }
        if let Some(ref rp) = backend.resolved_params() {
            eprintln!("║ backend temp:    {:?}", rp.temperature);
            eprintln!("║ backend max_tok: {:?}", rp.max_tokens);
        }
        eprintln!("╚═══════════════════╝");
    }

    eprintln!("Scoring comprehensibility for {} modules...", modules.len());

    // Single pass: for each module, check the pre-computed module_cache.
    // Cache hits produce `estimates` copies of the cached score directly.
    // Cache misses spawn `estimates` LLM tasks.
    let mut handles: Vec<ComprehensionTask> = Vec::new();

    // Scores collected so far (cache hits go straight here).
    let mut by_module: HashMap<String, Vec<f64>> = HashMap::new();

    // Build a module → metrics lookup from scores for cache-miss writeback.
    let metrics: HashMap<String, (usize, usize, usize, usize)> = scores
        .iter()
        .map(|s| {
            (
                s.module.clone(),
                (s.token_count, s.fan_in, s.fan_out, s.graph_complexity),
            )
        })
        .collect();

    for (module, file, _pkg) in modules.iter() {
        // ── cache hit (pre-computed in run()) ──
        if let Some(entry) = module_cache.get(module.as_str()) {
            eprintln!("  [{module}] cache hit ({})", entry.key);
            by_module
                .entry(module.clone())
                .or_default()
                .resize(estimates, entry.comprehensibility);
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
            by_module
                .entry(module.clone())
                .or_default()
                .resize(estimates, 1.0);
            continue;
        }

        let abs_path = std::fs::canonicalize(file)
            .unwrap_or_else(|_| file.clone())
            .to_string_lossy()
            .into_owned();

        let fence = lang::code_fence(&file);
        let prompt =
            format!("SOURCE FILE ({module}):\n\n```{fence}\n{source}\n```");

        let cached_metrics = metrics.get(module.as_str()).copied();

        for _ in 0..estimates {
            let module = module.clone();
            let source = source.clone();
            let abs_path = abs_path.clone();
            let prompt = prompt.clone();
            let model_spec = model_spec.to_string();
            let cache_enabled = cache_state.is_some();
            let cached_metrics = cached_metrics;

            handles.push(tokio::spawn(async move {
                let cache_key = if cache_enabled {
                    Some(compute_cache_key(&abs_path, &source, &model_spec))
                } else {
                    None
                };

                for attempt in 0..MAX_COMP_RETRIES {
                    match run_comp_agent(&module, &source, &prompt, &model_spec, verbose).await {
                        Ok(c) => {
                            let raw: f64 = (c.naming as f64
                                + c.control_flow as f64
                                + c.data_flow as f64
                                + c.abstractions as f64)
                                / 16.0;
                            let score = raw.clamp(0.0, 1.0);
                            if score >= 1.0 {
                                eprintln!(
                                    "  [{module}] WARNING: perfect score (all 4s)"
                                );
                            }

                            return Ok(CompEstimate {
                                module,
                                score,
                                abs_path,
                                model_spec,
                                cache_key,
                                cached_metrics,
                                breakdown: c,
                            });
                        }
                        Err(e) => {
                            if is_retryable_comp_error_str(&e) && attempt + 1 < MAX_COMP_RETRIES {
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
                                "[{module}] failed after {} attempts: {e}",
                                attempt + 1
                            ));
                        }
                    }
                }

                Err(format!(
                    "[{module}] exhausted {} retries",
                    MAX_COMP_RETRIES
                ))
            }));
        }
    }

    // Collect task results into estimates_map (cache-miss modules only).
    let mut estimates_map: HashMap<String, Vec<CompEstimate>> = HashMap::new();
    for handle in handles {
        let est = handle.await.map_err(|e| format!("join error: {e}"))??;
        estimates_map.entry(est.module.clone()).or_default().push(est);
    }

    for (module, _file, _pkg) in modules.iter() {
        // Determine the scores: either from cache hits or fresh estimates.
        let scores_for_module: Vec<f64> =
            if let Some(estimates_vec) = estimates_map.get(module.as_str()) {
                // ── cache miss: scores come from fresh estimates ──
                if estimates_vec.len() != estimates {
                    return Err(format!(
                        "[{module}] expected {estimates} estimates, got {}",
                        estimates_vec.len()
                    ));
                }
                estimates_vec.iter().map(|e| e.score).collect()
            } else if let Some(scores_vec) = by_module.get(module.as_str()) {
                // ── cache hit: scores were pre-populated ──
                if scores_vec.len() != estimates {
                    return Err(format!(
                        "[{module}] cache-hit expected {estimates} scores, got {}",
                        scores_vec.len()
                    ));
                }
                scores_vec.clone()
            } else {
                return Err(format!(
                    "[{module}] no estimates collected — all tasks failed"
                ));
            };

        let med = median(&scores_for_module);
        eprintln!("  {module}: {med:.3}");
        let s = scores
            .iter_mut()
            .find(|s| s.module == *module)
            .ok_or_else(|| format!("[{module}] score computed but no matching ModuleDrag found"))?;
        s.comprehensibility = Some(med);

        // ── Write a single cache entry with the median score (cache-miss only) ──
        if let Some(estimates_vec) = estimates_map.get(module.as_str()) {
            if let Some(first) = estimates_vec.first() {
                if let (Some(key), Some(metrics)) = (&first.cache_key, first.cached_metrics) {
                    if let Some((ref arc, ref cp)) = cache_state {
                        let (tc, fi, fo, gc) = metrics;
                        let entry = CacheEntry {
                            key: key.clone(),
                            file: first.abs_path.clone(),
                            model: first.model_spec.clone(),
                            token_count: tc,
                            fan_in: fi,
                            fan_out: fo,
                            graph_complexity: gc,
                            comprehensibility: med,
                            breakdown: first.breakdown.clone(),
                        };
                        if let Err(e) = append_cache_entry(cp, &entry) {
                            eprintln!(
                                "  [{module}] WARNING: failed to append cache entry: {e}"
                            );
                        }
                        arc.lock().unwrap().insert(key.clone(), entry);
                    }
                }
            }
        }
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
    _source: &str,
    prompt: &str,
    model_spec: &str,
    verbose: bool,
) -> Result<ComprehensibilityScores, String> {
    let model_uri =
        ModelURI::parse(model_spec).map_err(|e| format!("invalid model: {e}"))?;

    let stage = AgentBuilder::new("score")
        .prompt(format!(
            "{prompt}\n\n\
             Score the above source file on comprehensibility.\n\n\
             Target reader: a competent developer familiar with the language \
             but unfamiliar with this file.  Do not assume shorter code is \
             better or longer code is clearer.  Do not judge correctness.\n\n\
             Score four dimensions on a 0-4 scale:\n\
             4: Clear on a first careful reading.\n\
             3: Clear with minor rereading or navigation.\n\
             2: Requires substantial tracing or inference.\n\
             1: Important behavior is obscured.\n\
             0: Cannot reliably understand from supplied context.\n\n\
             Produce a JSON object with four fields: naming, control_flow,\
             data_flow, abstractions.  Write the JSON to {{comprehensibility}}."
        ))
        .output("comprehensibility", output("artifact://comprehensibility"))
        .landscape(false)
        .model(model_uri.clone())
        .build()
        .map_err(|e| format!("agent build: {e}"))?;

    let def = DefinitionBuilder::new("drag-comp", model_uri)
        .stage(stage)
        .build()
        .map_err(|e| format!("def build: {e}"))?;

    // Gremlin::init canonicalizes definition_path to derive the project root.
    // For ephemeral runs the worktree is a temp dir so the project root is
    // irrelevant — we just need an extant path.
    let cwd = std::env::current_dir().map_err(|e| format!("cwd: {e}"))?;

    let config = GremlinConfig { ephemeral: true };
    let mut gremlin = Gremlin::init(
        "drag-comp",
        &cwd,
        &def,
        &HashMap::new(),
        Some(model_spec),
        &config,
    )
    .map_err(|e| format!("gremlin init: {e}"))?;

    if verbose {
        eprintln!("  [{module}] running agent stage (model={model_spec})...");
    }

    let exit_code = gremlin
        .run(None)
        .await
        .map_err(|e| format!("gremlin run: {e}"))?;

    if exit_code != 0 {
        return Err(format!(
            "agent stage exited {exit_code} (bail/missing artifact)"
        ));
    }

    let content = gremlin
        .state
        .content("artifact://comprehensibility", None)
        .await
        .map_err(|e| format!("artifact read: {e}"))?;

    let json_str = extract_json(&content)
        .ok_or_else(|| format!("no JSON in artifact: {}", truncate(&content, 200)))?;

    serde_json::from_str(&json_str).map_err(|e| format!("parse: {e}"))
}

/// Whether a string error from [`run_comp_agent`] is worth retrying.
// ---------------------------------------------------------------------------
// JSON extraction
// ---------------------------------------------------------------------------

/// Extract a JSON object from an LLM response, handling common failure modes.
///
/// Tries these strategies in order:
/// 1. Strip markdown code fences (` ```json ` / ` ``` `).
/// 2. Find the outermost `{...}` pair (handles text before/after JSON).
/// 3. Clean common JSON formatting errors (trailing commas, comments).
fn extract_json(text: &str) -> Option<String> {
    let text = text.trim();

    // ── 1. Strip markdown code fences ──
    let mut inner = text;

    // Strip opening fence: ```json or ``` (possibly with no newline after).
    inner = inner
        .strip_prefix("```json")
        .or_else(|| inner.strip_prefix("```"))
        .unwrap_or(inner)
        .trim();

    // Strip closing fence: last ``` in the text.
    if let Some(pos) = inner.rfind("```") {
        // Only strip if it appears to be a closing fence (at end or on its own line).
        let tail = inner[pos..].trim();
        if tail == "```" || tail.starts_with("```\n") || tail.starts_with("```\r") {
            inner = inner[..pos].trim();
        }
    }

    // ── 2. Find outermost JSON object ──
    let json_str = find_json_object(inner)?;

    // ── 3. Clean and return ──
    Some(clean_json(&json_str))
}

/// Find the outermost `{...}` pair in text, respecting string boundaries.
fn find_json_object(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut start = None;
    let mut depth = 0u32;
    let mut in_string = false;
    let mut escape = false;

    for (i, &ch) in chars.iter().enumerate() {
        if escape {
            escape = false;
            continue;
        }
        if ch == '\\' && in_string {
            escape = true;
            continue;
        }
        if ch == '"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }
        if ch == '{' {
            if depth == 0 {
                start = Some(i);
            }
            depth += 1;
        } else if ch == '}' && depth > 0 {
            depth -= 1;
            if depth == 0 {
                let s = start?;
                return Some(chars[s..=i].iter().collect());
            }
        }
    }
    None
}

/// Clean common JSON formatting errors in LLM output:
/// - `//` line comments
/// - `/* */` block comments
/// - Trailing commas before `}` or `]`
fn clean_json(json: &str) -> String {
    let mut result = String::with_capacity(json.len());
    let chars: Vec<char> = json.chars().collect();
    let mut in_string = false;
    let mut escape = false;
    let mut in_block_comment = false;
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];

        // ── block comment ──
        if in_block_comment {
            if ch == '*' && i + 1 < chars.len() && chars[i + 1] == '/' {
                in_block_comment = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }

        // ── string escape ──
        if escape {
            escape = false;
            result.push(ch);
            i += 1;
            continue;
        }
        if ch == '\\' && in_string {
            escape = true;
            result.push(ch);
            i += 1;
            continue;
        }

        // ── string boundary ──
        if ch == '"' {
            in_string = !in_string;
            result.push(ch);
            i += 1;
            continue;
        }

        // Inside a string: copy verbatim.
        if in_string {
            result.push(ch);
            i += 1;
            continue;
        }

        // ── line comment ──
        if ch == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
            // Skip to end of line.
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }

        // ── block comment start ──
        if ch == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
            in_block_comment = true;
            i += 2;
            continue;
        }

        result.push(ch);
        i += 1;
    }

    // ── trailing commas ──
    // Replace `,}` and `,]` with `}` / `]`, handling whitespace between.
    remove_trailing_commas(&result)
}

/// Remove commas immediately before `}` or `]`, ignoring whitespace.
fn remove_trailing_commas(json: &str) -> String {
    let chars: Vec<char> = json.chars().collect();
    let mut out = String::with_capacity(json.len());
    let len = chars.len();
    let mut i = 0;

    while i < len {
        let ch = chars[i];

        // Look ahead from a comma: if the next non-whitespace char is } or ],
        // skip this comma.
        if ch == ',' {
            let mut j = i + 1;
            while j < len
                && (chars[j] == ' ' || chars[j] == '\t' || chars[j] == '\n' || chars[j] == '\r')
            {
                j += 1;
            }
            if j < len && (chars[j] == '}' || chars[j] == ']') {
                // Skip the comma, then copy the whitespace between comma and closer.
                i += 1; // skip the comma itself
                while i < j {
                    out.push(chars[i]);
                    i += 1;
                }
                continue;
            }
        }

        out.push(ch);
        i += 1;
    }

    out
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

fn print_human_table(scores: &[ModuleDrag]) {
    // Show a "comp" column only when every module was scored (i.e. after a
    // successful comprehensibility run). When unscored, all are None.
    let has_comp = scores.iter().all(|s| s.comprehensibility.is_some());

    if has_comp {
        println!(
            "{:<55} {:>8} {:>8} {:>8} {:>12} {:>6} {:>10}",
            "module", "tokens", "fan-in", "fan-out", "complexity", "comp", "drag (ln)"
        );
        println!("{}", "-".repeat(116));
    } else {
        println!(
            "{:<55} {:>8} {:>8} {:>8} {:>12} {:>10}",
            "module", "tokens", "fan-in", "fan-out", "complexity", "drag (ln)"
        );
        println!("{}", "-".repeat(110));
    }

    for s in scores {
        if has_comp {
            let c = s.comprehensibility.expect("has_comp guarantees Some");
            println!(
                "{:<55} {:>8} {:>8} {:>8} {:>12} {:>6.2} {:>10.2}",
                truncate(&s.module, 54),
                s.token_count,
                s.fan_in,
                s.fan_out,
                s.graph_complexity,
                c,
                s.ln_drag,
            );
        } else {
            println!(
                "{:<55} {:>8} {:>8} {:>8} {:>12} {:>10.2}",
                truncate(&s.module, 54),
                s.token_count,
                s.fan_in,
                s.fan_out,
                s.graph_complexity,
                s.ln_drag,
            );
        }
    }
}

fn truncate(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        return s;
    }
    let mut end = max_len;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Return the median of a non-empty slice of f64 values.
/// Odd-length: middle element after sort. Even-length: average of the two
/// middle elements.
fn median(values: &[f64]) -> f64 {
    debug_assert!(!values.is_empty(), "median of empty slice");
    let mut sorted: Vec<f64> = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = sorted.len();
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizer_names_parse() {
        assert!(parse_tokenizer("o200k_base").is_ok());
        assert!(parse_tokenizer("bogus").is_err());
    }

    #[test]
    fn computes_degrees() {
        let edges = vec![
            ("a".to_string(), "b".to_string()),
            ("a".to_string(), "c".to_string()),
            ("b".to_string(), "c".to_string()),
        ];
        let (fan_out, fan_in) = compute_degrees(&edges);
        assert_eq!(fan_out.get("a"), Some(&2));
        assert_eq!(fan_out.get("b"), Some(&1));
        assert_eq!(fan_in.get("c"), Some(&2));
        assert_eq!(fan_in.get("b"), Some(&1));
    }

    #[test]
    fn truncate_is_char_boundary_safe() {
        // "é" is two bytes; cutting at byte 1 would be a non-boundary.
        assert_eq!(truncate("éx", 1), "");
        assert_eq!(truncate("abc", 2), "ab");
    }

    #[test]
    fn ln_drag_omits_comprehensibility_term_when_unscored() {
        let no_comp = compute_ln_drag(1.0, 0.4, 0.4, 0.01, 100, 2, None);
        let with_comp = compute_ln_drag(1.0, 0.4, 0.4, 0.01, 100, 2, Some(1.0));
        assert!((no_comp - with_comp - 0.4 * 1.01f64.ln()).abs() < 1e-12);
    }

    // ── JSON extraction tests ──

    #[test]
    fn extract_plain_json() {
        let raw = r#"{"naming": 3, "control_flow": 2, "data_flow": 2, "abstractions": 3}"#;
        let extracted = extract_json(raw).unwrap();
        let scores: ComprehensibilityScores = serde_json::from_str(&extracted).unwrap();
        assert_eq!(scores.naming, 3);
        assert_eq!(scores.control_flow, 2);
    }

    #[test]
    fn extract_json_fenced() {
        let raw = "```json\n{\"naming\": 3, \"control_flow\": 2, \"data_flow\": 2, \"abstractions\": 3}\n```";
        let extracted = extract_json(raw).unwrap();
        let scores: ComprehensibilityScores = serde_json::from_str(&extracted).unwrap();
        assert_eq!(scores.naming, 3);
    }

    #[test]
    fn extract_json_fenced_no_lang() {
        let raw =
            "```\n{\"naming\": 3, \"control_flow\": 2, \"data_flow\": 2, \"abstractions\": 3}\n```";
        let extracted = extract_json(raw).unwrap();
        let scores: ComprehensibilityScores = serde_json::from_str(&extracted).unwrap();
        assert_eq!(scores.naming, 3);
    }

    #[test]
    fn extract_json_with_text_before() {
        let raw = "Here is my assessment of the file:\n\n{\"naming\": 3, \"control_flow\": 2, \"data_flow\": 2, \"abstractions\": 3}\n\nHope that helps.";
        let extracted = extract_json(raw).unwrap();
        let scores: ComprehensibilityScores = serde_json::from_str(&extracted).unwrap();
        assert_eq!(scores.naming, 3);
    }

    #[test]
    fn extract_json_with_trailing_comma() {
        let raw = "{\"naming\": 3, \"control_flow\": 2, \"data_flow\": 2, \"abstractions\": 3,}";
        let extracted = extract_json(raw).unwrap();
        let scores: ComprehensibilityScores = serde_json::from_str(&extracted).unwrap();
        assert_eq!(scores.naming, 3);
    }

    #[test]
    fn extract_json_with_line_comment() {
        let raw = "{\n// scoring notes\n\"naming\": 3,\n\"control_flow\": 2,\n\"data_flow\": 2,\n\"abstractions\": 3\n}";
        let extracted = extract_json(raw).unwrap();
        let scores: ComprehensibilityScores = serde_json::from_str(&extracted).unwrap();
        assert_eq!(scores.naming, 3);
    }

    #[test]
    fn extract_json_with_block_comment() {
        let raw = "{\n/* scoring notes */\n\"naming\": 3,\n\"control_flow\": 2,\n\"data_flow\": 2,\n\"abstractions\": 3\n}";
        let extracted = extract_json(raw).unwrap();
        let scores: ComprehensibilityScores = serde_json::from_str(&extracted).unwrap();
        assert_eq!(scores.naming, 3);
    }

    #[test]
    fn extract_json_fenced_with_text_and_trailing_comma() {
        // The most common failure mode: markdown fence, trailing comma, extra text.
        let raw = "Here's the score:\n\n```json\n{\n  \"naming\": 3,\n  \"control_flow\": 2,\n  \"data_flow\": 2,\n  \"abstractions\": 3,\n}\n```\n\nLet me know if you need anything else.";
        let extracted = extract_json(raw).unwrap();
        let scores: ComprehensibilityScores = serde_json::from_str(&extracted).unwrap();
        assert_eq!(scores.naming, 3);
    }

    #[test]
    fn extract_json_missing_closing_fence() {
        let raw =
            "```json\n{\"naming\": 3, \"control_flow\": 2, \"data_flow\": 2, \"abstractions\": 3}";
        let extracted = extract_json(raw).unwrap();
        let scores: ComprehensibilityScores = serde_json::from_str(&extracted).unwrap();
        assert_eq!(scores.naming, 3);
    }

    #[test]
    fn extract_json_nested_braces_in_string() {
        // Braces inside a JSON string should not confuse the matcher.
        let raw = r#"{"naming": 3, "note": "use {braces} here", "control_flow": 2, "data_flow": 2, "abstractions": 3}"#;
        let extracted = extract_json(raw).unwrap();
        // This test just needs valid JSON out — extra field is fine for serde.
        let _: serde_json::Value = serde_json::from_str(&extracted).unwrap();
    }

    #[test]
    fn extract_json_no_braces_returns_none() {
        assert!(extract_json("just some text, no json here").is_none());
    }

    #[test]
    fn remove_trailing_comma_before_brace() {
        assert_eq!(remove_trailing_commas("{\"a\": 1,}"), "{\"a\": 1}");
    }

    #[test]
    fn remove_trailing_comma_with_newline() {
        assert_eq!(remove_trailing_commas("{\"a\": 1,\n}"), "{\"a\": 1\n}");
    }

    // ── median tests ──

    #[test]
    fn median_odd_length() {
        let vals = [3.0, 1.0, 2.0];
        assert!((median(&vals) - 2.0).abs() < 1e-12);
    }

    #[test]
    fn median_even_length() {
        let vals = [1.0, 4.0, 2.0, 3.0];
        assert!((median(&vals) - 2.5).abs() < 1e-12);
    }

    #[test]
    fn median_single_value() {
        let vals = [7.0];
        assert!((median(&vals) - 7.0).abs() < 1e-12);
    }

    #[test]
    fn median_identical_values() {
        let vals = [0.5, 0.5, 0.5];
        assert!((median(&vals) - 0.5).abs() < 1e-12);
    }

    // ── cache tests ──

    #[test]
    fn cache_key_is_deterministic() {
        let k1 = compute_cache_key("/abs/path/foo.rs", "fn main() {}", "openai/gpt-4o");
        let k2 = compute_cache_key("/abs/path/foo.rs", "fn main() {}", "openai/gpt-4o");
        assert_eq!(k1, k2);
        assert_eq!(k1.len(), 16);
        // Different content → different key.
        let k3 = compute_cache_key(
            "/abs/path/foo.rs",
            "fn main() { /* changed */ }",
            "openai/gpt-4o",
        );
        assert_ne!(k1, k3);
        // Different model → different key.
        let k4 = compute_cache_key("/abs/path/foo.rs", "fn main() {}", "anthropic/claude");
        assert_ne!(k1, k4);
        // Different path → different key.
        let k5 = compute_cache_key("/abs/path/bar.rs", "fn main() {}", "openai/gpt-4o");
        assert_ne!(k1, k5);
    }

    #[test]
    fn load_cache_missing_file_is_cold() {
        let map = load_cache(Path::new("/nonexistent/cache/file.jsonl")).unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn load_cache_blank_line_errors() {
        let dir = std::env::temp_dir().join("drag-test-blank-line");
        let _ = std::fs::create_dir_all(&dir);
        let cache_path = dir.join("cache.jsonl");
        // Write a file where line 2 is blank.
        std::fs::write(
            &cache_path,
            "{\"key\":\"abc\",\"file\":\"/f.rs\",\"model\":\"m\",\"token_count\":10,\"fan_in\":0,\"fan_out\":0,\"graph_complexity\":0,\"comprehensibility\":0.5,\"breakdown\":{\"naming\":2,\"control_flow\":2,\"data_flow\":2,\"abstractions\":2}}\n\n",
        )
        .unwrap();
        let err = load_cache(&cache_path).unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            err.contains(&cache_path.display().to_string()),
            "error should contain cache file path, got: {err}"
        );
        assert!(
            err.contains(":2:"),
            "error should mention line 2, got: {err}"
        );
    }

    #[test]
    fn load_cache_malformed_line_errors_with_line_number() {
        let dir = std::env::temp_dir().join("drag-test-malformed");
        let _ = std::fs::create_dir_all(&dir);
        let cache_path = dir.join("cache.jsonl");
        std::fs::write(&cache_path, "not valid json\n").unwrap();
        let err = load_cache(&cache_path).unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            err.contains(&cache_path.display().to_string()),
            "error should contain cache file path, got: {err}"
        );
        assert!(
            err.contains(":1:"),
            "error should mention line 1, got: {err}"
        );
    }

    #[test]
    fn load_cache_valid_entries() {
        let dir = std::env::temp_dir().join("drag-test-valid");
        let _ = std::fs::create_dir_all(&dir);
        let cache_path = dir.join("cache.jsonl");
        let line = "{\"key\":\"deadbeef12345678\",\"file\":\"/src/main.rs\",\"model\":\"openai/gpt-4o\",\"token_count\":42,\"fan_in\":1,\"fan_out\":2,\"graph_complexity\":3,\"comprehensibility\":0.75,\"breakdown\":{\"naming\":3,\"control_flow\":3,\"data_flow\":3,\"abstractions\":3}}\n";
        std::fs::write(&cache_path, line).unwrap();
        let map = load_cache(&cache_path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(map.len(), 1);
        let entry = map.get("deadbeef12345678").unwrap();
        assert_eq!(entry.comprehensibility, 0.75);
        assert_eq!(entry.breakdown.naming, 3);
    }

    #[test]
    fn append_and_load_roundtrip() {
        let dir = std::env::temp_dir().join("drag-test-roundtrip");
        let _ = std::fs::create_dir_all(&dir);
        let cache_path = dir.join("cache.jsonl");
        let entry = CacheEntry {
            key: "abcd1234abcd1234".to_string(),
            file: "/abs/path/module.rs".to_string(),
            model: "openai/gpt-4o".to_string(),
            token_count: 100,
            fan_in: 2,
            fan_out: 3,
            graph_complexity: 5,
            comprehensibility: 0.5,
            breakdown: ComprehensibilityScores {
                naming: 2,
                control_flow: 2,
                data_flow: 2,
                abstractions: 2,
            },
        };
        append_cache_entry(&cache_path, &entry).unwrap();
        let map = load_cache(&cache_path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(map.len(), 1);
        let loaded = map.get("abcd1234abcd1234").unwrap();
        assert_eq!(loaded.comprehensibility, 0.5);
        assert_eq!(loaded.file, "/abs/path/module.rs");
        assert_eq!(loaded.model, "openai/gpt-4o");
    }
}
