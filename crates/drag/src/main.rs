mod lang;
mod scoring;

use scoring::{compute_drag, compute_footprint};

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
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
    /// under it. Required unless --curves is used.
    path: Option<PathBuf>,

    /// Output JSON instead of a human-readable table.
    #[arg(long)]
    json: bool,

    /// Model spec for comprehensibility scoring (e.g. "openai/gpt-4o").
    /// Defaults to the gremlins default model.
    #[arg(long)]
    model: Option<String>,

    /// Tokenizer name for tiktoken-rs (e.g. "o200k_base", "cl100k_base").
    #[arg(long, default_value = "o200k_base")]
    tokenizer: String,

    /// Weight for the footprint term in the drag formula.
    #[arg(long, default_value_t = 0.5)]
    w2: f64,

    /// Exponent for the comprehensibility penalty
    /// (m=2.5 balances merges vs splits).
    #[arg(long, default_value_t = 2.5)]
    m: f64,

    /// Print the resolved model spec and diagnostic information.
    #[arg(short = 'v', long)]
    verbose: bool,

    /// Number of independent LLM estimates per module for comprehensibility
    /// scoring (median is used). Default 1.
    #[arg(long, default_value_t = 1)]
    comprehensibility_estimates: usize,

    /// Path to a JSONL cache file for comprehensibility scores.
    /// When set, this path overrides the default cache location.
    /// The default cache lives under the gremlins shared state directory
    /// (e.g. `drag/comprehensibility-cache.jsonl`).
    #[arg(long)]
    cache: Option<PathBuf>,

    /// Delete the comprehensibility cache file and exit.
    /// The cache path is resolved from `--cache` if given, otherwise
    /// the default location is used (same logic as the main run).
    #[arg(long)]
    clear_cache: bool,

    /// Print drag curves showing how footprint and comprehensibility
    /// contribute to drag at the current weights.
    /// Exits immediately after printing; no modules are scored.
    #[arg(long)]
    curves: bool,
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
    w2: f64,
    m: f64,
}

#[derive(Serialize)]
struct ModuleDrag {
    module: String,
    file: String,
    token_count: usize,
    /// Total token count of the module plus all modules transitively
    /// reachable from it (including the module itself).
    footprint: usize,
    comprehensibility: Option<f64>,
    /// The drag metric: `(1+footprint)^w2 · exp(m·(1−comp))`.
    drag: f64,
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

    // Initialize config early so cache path resolution (and --clear-cache)
    // respect any configured state-root overrides.
    gremlins::config::init_global().map_err(|e| e.to_string())?;

    // ── resolve cache path (needed by --curves? no, but --clear-cache needs it) ──
    let cache_path: PathBuf = match &cli.cache {
        Some(p) => p.clone(),
        None => {
            let state = gremlins::config::state_root();
            state.join("drag").join("comprehensibility-cache.jsonl")
        }
    };

    if cli.clear_cache {
        if cache_path.exists() {
            std::fs::remove_file(&cache_path).map_err(|e| {
                format!("failed to remove cache file {}: {e}", cache_path.display())
            })?;
            eprintln!("Cache cleared: {}", cache_path.display());
        } else {
            eprintln!("No cache file found at: {}", cache_path.display());
        }
        return Ok(());
    }

    if cli.curves {
        print_curves(cli.w2, cli.m);
        return Ok(());
    }

    let path = cli
        .path
        .ok_or_else(|| "<PATH> is required unless --curves or --clear-cache is used".to_string())?;

    // 1. Discover packages (Rust + Python, best-effort) anchored at the
    //    user-supplied path so that packages are found regardless of cwd.
    let packages = lang::discover_packages(&path);

    // 2. Resolve the input path to a list of recognized source files
    let target_files = resolve_path_files(&path)?;

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
            path.display()
        ));
    }

    // ── resolve model & cache path early (both needed for key computation) ──
    let model_spec = resolve_comprehensibility_model(cli.model.as_deref())?;

    let cache_map = load_cache(&cache_path)?;
    eprintln!(
        "Loaded {} cached entries from {}",
        cache_map.len(),
        cache_path.display()
    );

    // ── pre-compute cache keys and build module → CacheEntry lookup ──
    let mut module_cache: HashMap<String, CacheEntry> = HashMap::new();
    for (module, file, _pkg) in &target_modules {
        let source = std::fs::read_to_string(file).unwrap_or_default();
        if source.trim().is_empty() {
            continue;
        }
        let key = compute_cache_key(&source, &model_spec, cli.comprehensibility_estimates);
        if let Some(entry) = cache_map.get(&key) {
            // Only accept cached entries whose estimate count matches.
            if entry.estimates == cli.comprehensibility_estimates {
                module_cache.insert(module.clone(), entry.clone());
            }
        }
    }
    let cache_hit_count = module_cache.len();
    if cache_hit_count > 0 {
        eprintln!("  {cache_hit_count} modules have complete cache hits");
    }

    // 4. Build dependency graph (one graph per package, in parallel).
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
    for (i, handle) in handles.into_iter().enumerate() {
        match handle.await {
            Ok(Ok(pkg_edges)) => edges.extend(pkg_edges),
            Ok(Err(e)) => {
                let pkg_name = scoring_packages[i].name();
                eprintln!("gremlins-drag: dependency graph skipped for {pkg_name}: {e}");
            }
            Err(e) => {
                eprintln!("gremlins-drag: dependency graph join error: {e}");
            }
        }
    }

    // 5. Load the tokenizer once and count tokens for every in-package
    //    module.  `footprint(m)` sums token counts across a module and its
    //    transitive dependency closure, so the full package source tree is
    //    needed even when only a subset of modules is scored.
    let tokenizer = parse_tokenizer(&cli.tokenizer)?;
    let bpe = tiktoken_rs::bpe_for_tokenizer(tokenizer)
        .map_err(|e| format!("failed to load tokenizer {}: {e}", cli.tokenizer))?;

    let mut token_counts: HashMap<String, usize> = HashMap::new();
    for pkg in &scoring_packages {
        let files = lang::package_files(pkg)
            .map_err(|e| format!("failed to enumerate files for {}: {e}", pkg.name()))?;
        for file in files {
            if let Some((module, _)) = lang::file_to_module(&file, &packages) {
                let n = count_tokens(bpe, &file)?;
                token_counts.insert(module, n);
            }
        }
    }

    // 6. Compute transitive dependency footprints and assemble per-module
    //    metrics.  A module's footprint includes its own token count, so
    //    every scored module needs one — even when it falls outside the
    //    discovered packages.
    //    Comprehensibility stays scoped to `--target` modules only.
    for (module, file, _pkg_key) in &target_modules {
        if !token_counts.contains_key(module) {
            let n = count_tokens(bpe, file)?;
            token_counts.insert(module.clone(), n);
        }
    }

    let footprint_map = compute_footprint(&edges, &token_counts);

    let mut scores: Vec<ModuleDrag> = Vec::new();
    for (module, file, _pkg_key) in &target_modules {
        let token_count = token_counts.get(module).copied().unwrap_or(0);
        let footprint = footprint_map.get(module).copied().unwrap_or(0);

        scores.push(ModuleDrag {
            module: module.clone(),
            file: file.to_string_lossy().into_owned(),
            token_count,
            footprint,
            comprehensibility: None,
            drag: 0.0,
        });
    }

    // 7. Comprehensibility. Failures skip individual modules rather than
    //    aborting the whole run.
    if cli.comprehensibility_estimates < 1 {
        return Err("--comprehensibility-estimates must be >= 1".to_string());
    }

    let cache_state: CacheState = cache_path.clone();

    score_comprehensibility(
        &model_spec,
        cli.verbose,
        cli.comprehensibility_estimates,
        &target_modules,
        &mut scores,
        cache_state,
        module_cache,
    )
    .await?;

    // 8. Compute the final drag scores once, after any scoring.
    for s in &mut scores {
        s.drag = compute_drag(
            &DragConfig {
                w2: cli.w2,
                m: cli.m,
            },
            s.footprint,
            s.comprehensibility,
        );
    }

    // 9. Sort by drag descending
    scores.sort_by(|a, b| {
        b.drag
            .partial_cmp(&a.drag)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // 10. Output
    if cli.json {
        let report = DragReport {
            config: DragConfig {
                w2: cli.w2,
                m: cli.m,
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
// (see `crates/drag/src/scoring.rs` for compute_footprint and compute_drag)

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

// ---------------------------------------------------------------------------
// Drag formula
// ---------------------------------------------------------------------------
// `compute_drag` lives in `crates/drag/src/scoring.rs`.

// ---------------------------------------------------------------------------
// Comprehensibility scoring
// ---------------------------------------------------------------------------

const MAX_COMP_RETRIES: u32 = 3;
const COMP_BACKOFF: [f64; 2] = [1.0, 2.0];

/// Classification of an error from [`run_comp_agent`].
#[derive(Debug)]
enum CompAgentError {
    /// A transient failure worth retrying (model produced no/malformed JSON).
    Retryable(String),
    /// A non-retryable failure (bail, missing artifact after agent loop exhausted retries).
    Fatal(String),
}

impl std::fmt::Display for CompAgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompAgentError::Retryable(s) | CompAgentError::Fatal(s) => write!(f, "{s}"),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct ComprehensibilityScores {
    naming: u8,
    control_flow: u8,
    data_flow: u8,
    abstractions: u8,
}

/// One line in the JSONL cache file.
///
/// The cache key is model-dependent: `blake3(content || "\0" || model_uri)`.
///
/// **What the cache actually fast-paths:** only `token_count` and
/// `comprehensibility` are restored on a cache hit. The remaining fields
/// (`file`, `model`, `breakdown`) are stored for inspection/debugging but
/// are never read back — dependency footprints are position/context-dependent
/// and always recomputed fresh.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
struct CacheEntry {
    /// BLAKE3 hash of (content || "\0" || model_uri),
    /// hex-encoded, first 16 chars. This is the lookup key.
    key: String,
    /// Absolute path of the source file at scoring time.
    file: String,
    /// Fully-resolved model URI used for scoring.
    model: String,
    /// Token count from tiktoken BPE (model-independent; stored here for convenience).
    token_count: usize,
    /// Number of estimates used to produce the median.
    #[serde(default = "default_estimates")]
    estimates: usize,
    /// Median comprehensibility score (0–1).
    comprehensibility: f64,
    /// The four dimension scores from the LLM response.
    breakdown: ComprehensibilityScores,
}

fn default_estimates() -> usize {
    1
}

/// Compute a BLAKE3-based cache key from the file's full source text,
/// the resolved model URI, and the estimate count.
///
/// Key = first 16 hex chars of `blake3(content || "\0" || model_uri || "\0" || estimates)`.
/// The 64-bit truncation is fine at realistic cache sizes but collision
/// risk grows past ~10⁵–10⁶ entries for strict uniqueness guarantees.
fn compute_cache_key(content: &str, model_uri: &str, estimates: usize) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(content.as_bytes());
    hasher.update(b"\0");
    hasher.update(model_uri.as_bytes());
    hasher.update(b"\0");
    hasher.update(&estimates.to_le_bytes());
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

/// Shared comprehensibility cache: the JSONL file path it is persisted to.
type CacheState = PathBuf;

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
    cache_key: String,
    token_count: usize,
    breakdown: ComprehensibilityScores,
}

async fn score_comprehensibility(
    model_spec: &str,
    verbose: bool,
    estimates: usize,
    modules: &[(String, PathBuf, String)],
    scores: &mut [ModuleDrag],
    cache_state: CacheState,
    module_cache: HashMap<String, CacheEntry>,
) -> Result<(), String> {
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

    // Scores collected so far (cache hits go straight here).
    let mut by_module: HashMap<String, Vec<f64>> = HashMap::new();

    // Build a module → token count lookup from scores for cache-miss writeback.
    let module_tokens: HashMap<String, usize> = scores
        .iter()
        .map(|s| (s.module.clone(), s.token_count))
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

        let fence = lang::code_fence(file);
        let prompt = format!("SOURCE FILE ({module}):\n\n```{fence}\n{source}\n```");

        let token_count = module_tokens.get(module.as_str()).copied().unwrap_or(0);

        for _ in 0..estimates {
            let module = module.clone();
            let source = source.clone();
            let abs_path = abs_path.clone();
            let prompt = prompt.clone();
            let model_spec = model_spec.to_string();

            handles.push(tokio::spawn(async move {
                let cache_key = compute_cache_key(&source, &model_spec, estimates);

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
                                eprintln!("  [{module}] WARNING: perfect score (all 4s)");
                            }

                            return Ok(CompEstimate {
                                module,
                                score,
                                abs_path,
                                model_spec,
                                cache_key,
                                token_count,
                                breakdown: c,
                            });
                        }
                        Err(e) => {
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

    for (module, _file, _pkg) in modules.iter() {
        // Determine the scores: either from cache hits or fresh estimates.
        let scores_for_module: Vec<f64> = if let Some(estimates_vec) =
            estimates_map.get(module.as_str())
        {
            // ── cache miss: scores come from fresh estimates ──
            if estimates_vec.len() != estimates {
                eprintln!(
                        "  [{module}] WARNING: expected {estimates} estimates, got {} — skipping comprehensibility scoring",
                        estimates_vec.len()
                    );
                continue;
            }
            estimates_vec.iter().map(|e| e.score).collect()
        } else if let Some(scores_vec) = by_module.get(module.as_str()) {
            // ── cache hit: scores were pre-populated ──
            if scores_vec.len() != estimates {
                eprintln!(
                        "  [{module}] WARNING: cache-hit expected {estimates} scores, got {} — skipping comprehensibility scoring",
                        scores_vec.len()
                    );
                continue;
            }
            scores_vec.clone()
        } else {
            eprintln!(
                    "  [{module}] WARNING: no estimates collected — all tasks failed — skipping comprehensibility scoring"
                );
            continue;
        };

        let med = median(&scores_for_module);
        eprintln!("  {module}: {med:.3}");
        let s = match scores.iter_mut().find(|s| s.module == *module) {
            Some(s) => s,
            None => {
                eprintln!(
                    "  [{module}] WARNING: score computed but no matching ModuleDrag found — skipping"
                );
                continue;
            }
        };
        s.comprehensibility = Some(med);

        // ── Write a single cache entry with the median score (cache-miss only) ──
        if let Some(estimates_vec) = estimates_map.get(module.as_str()) {
            if let Some(first) = estimates_vec.first() {
                let cp = &cache_state;
                let entry = CacheEntry {
                    key: first.cache_key.clone(),
                    file: first.abs_path.clone(),
                    model: first.model_spec.clone(),
                    token_count: first.token_count,
                    estimates,
                    comprehensibility: med,
                    breakdown: first.breakdown.clone(),
                };
                if let Err(e) = append_cache_entry(cp, &entry) {
                    eprintln!("  [{module}] WARNING: failed to append cache entry: {e}");
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
    source: &str,
    prompt: &str,
    model_spec: &str,
    verbose: bool,
) -> Result<ComprehensibilityScores, CompAgentError> {
    let model_uri = ModelURI::parse(model_spec)
        .map_err(|e| CompAgentError::Fatal(format!("invalid model: {e}")))?;

    let stage = AgentBuilder::new("score")
        .option("src", source)
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
        .map_err(|e| CompAgentError::Fatal(format!("agent build: {e}")))?;

    let def = DefinitionBuilder::new("drag-comp", model_uri)
        .stage(stage)
        .build()
        .map_err(|e| CompAgentError::Fatal(format!("def build: {e}")))?;

    // Gremlin::init canonicalizes definition_path to derive the project root.
    // For ephemeral runs the worktree is a temp dir so the project root is
    // irrelevant — we just need an extant path.
    let cwd = std::env::current_dir().map_err(|e| CompAgentError::Fatal(format!("cwd: {e}")))?;

    let config = GremlinConfig { ephemeral: true };
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

    // Validate every dimension is in the requested 0–4 range.
    // Out-of-range values mean the model misbehaved — retryable.
    for (name, val) in [
        ("naming", scores.naming),
        ("control_flow", scores.control_flow),
        ("data_flow", scores.data_flow),
        ("abstractions", scores.abstractions),
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
// Drag curves
// ---------------------------------------------------------------------------

fn print_curves(w2: f64, m: f64) {
    let factor_footprint = |f: usize| -> f64 { (1.0 + f as f64).powf(w2) };
    let factor_comp = |c: f64| -> f64 { (m * (1.0 - c)).exp() };

    println!("Drag curves (w2={w2}, m={m}):\n");
    println!("  drag = (1+footprint)^w2 · exp(m·(1−comp))");

    println!("  Footprint factor ((1+footprint)^w2, w2={w2}):");
    for footprint in [0, 5, 10, 50, 100, 500, 1000, 5000] {
        println!(
            "    {footprint:>5} footprint  → {:>10.3}",
            factor_footprint(footprint)
        );
    }

    println!("\n  Comprehensibility factor (exp(m·(1−comp)), m={m}):");
    for comp in [0.10, 0.25, 0.50, 0.75, 0.90, 1.00] {
        println!("    {comp:.2}  → {:>10.3}", factor_comp(comp));
    }

    println!("\n  Both factors are multiplied together; larger values mean more drag.");
    println!("  Comprehensibility below 1.0 amplifies drag by up to exp(m).");
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
            "{:<55} {:>8} {:>10} {:>6} {:>14}",
            "module", "tokens", "footprint", "comp", "drag"
        );
        println!("{}", "-".repeat(97));
    } else {
        println!(
            "{:<55} {:>8} {:>10} {:>14}",
            "module", "tokens", "footprint", "drag"
        );
        println!("{}", "-".repeat(90));
    }

    for s in scores {
        if has_comp {
            let c = s.comprehensibility.expect("has_comp guarantees Some");
            println!(
                "{:<55} {:>8} {:>10} {:>6.2} {:>14.1}",
                truncate(&s.module, 54),
                s.token_count,
                s.footprint,
                c,
                s.drag,
            );
        } else {
            println!(
                "{:<55} {:>8} {:>10} {:>14.1}",
                truncate(&s.module, 54),
                s.token_count,
                s.footprint,
                s.drag,
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
    fn truncate_is_char_boundary_safe() {
        // "é" is two bytes; cutting at byte 1 would be a non-boundary.
        assert_eq!(truncate("éx", 1), "");
        assert_eq!(truncate("abc", 2), "ab");
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
        let k1 = compute_cache_key("fn main() {}", "openai/gpt-4o", 1);
        let k2 = compute_cache_key("fn main() {}", "openai/gpt-4o", 1);
        assert_eq!(k1, k2);
        assert_eq!(k1.len(), 16);
        // Different content → different key.
        let k3 = compute_cache_key("fn main() { /* changed */ }", "openai/gpt-4o", 1);
        assert_ne!(k1, k3);
        // Different model → different key.
        let k4 = compute_cache_key("fn main() {}", "anthropic/claude", 1);
        assert_ne!(k1, k4);
        // Different estimates → different key.
        let k5 = compute_cache_key("fn main() {}", "openai/gpt-4o", 5);
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
            "{\"key\":\"abc\",\"file\":\"/f.rs\",\"model\":\"m\",\"token_count\":10,\"comprehensibility\":0.5,\"breakdown\":{\"naming\":2,\"control_flow\":2,\"data_flow\":2,\"abstractions\":2}}\n\n",
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
        let line = "{\"key\":\"deadbeef12345678\",\"file\":\"/src/main.rs\",\"model\":\"openai/gpt-4o\",\"token_count\":42,\"comprehensibility\":0.75,\"breakdown\":{\"naming\":3,\"control_flow\":3,\"data_flow\":3,\"abstractions\":3}}\n";
        std::fs::write(&cache_path, line).unwrap();
        let map = load_cache(&cache_path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(map.len(), 1);
        let entry = map.get("deadbeef12345678").unwrap();
        assert_eq!(entry.comprehensibility, 0.75);
        assert_eq!(entry.breakdown.naming, 3);
    }

    #[test]
    fn load_cache_rejects_v1_entries_with_graph_fields() {
        let dir = std::env::temp_dir().join("drag-test-v1-reject");
        let _ = std::fs::create_dir_all(&dir);
        let cache_path = dir.join("cache.jsonl");
        let line = "{\"key\":\"deadbeef12345678\",\"file\":\"/src/main.rs\",\"model\":\"openai/gpt-4o\",\"token_count\":42,\"fan_in\":1,\"fan_out\":2,\"graph_complexity\":3,\"comprehensibility\":0.75,\"breakdown\":{\"naming\":3,\"control_flow\":3,\"data_flow\":3,\"abstractions\":3}}\n";
        std::fs::write(&cache_path, line).unwrap();
        let err = load_cache(&cache_path).unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            err.contains(&cache_path.display().to_string()),
            "error should contain cache file path, got: {err}"
        );
        assert!(
            err.contains("unknown field") || err.contains("fan_in"),
            "error should reject v1 entries because of unknown graph fields, got: {err}"
        );
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
            estimates: 1,
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
