mod config;
mod lang;
mod maint;
mod scoring;
mod util;

use config::{ComprehensibilityConfig, ComprehensibilityDimensionDef, DragConfig, OutputFormat};
use scoring::{compute_drag, compute_footprint};
use util::{extract_json, median, truncate, CacheState, CompAgentError};

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
    /// under it. Required unless --explain or --clear-cache is used.
    path: Option<PathBuf>,

    /// Path to a JSON5 config file. Overrides the bundled config.
    /// Mutually exclusive with GREMLINS_DRAG_CLI_CONFIG.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Model spec for comprehensibility scoring (e.g. "openai/gpt-4o").
    /// Overrides the model field from the resolved config.
    #[arg(long)]
    model: Option<String>,

    /// Print the resolved model spec and diagnostic information.
    #[arg(short = 'v', long)]
    verbose: bool,

    /// Delete the comprehensibility cache file and exit.
    /// The cache path is resolved from the config if given, otherwise
    /// the default location is used (same logic as the main run).
    #[arg(long)]
    clear_cache: bool,

    /// Print a full explanation of the drag calculation — formula, survey
    /// dimensions, and drag curves at the current weights. Exits immediately
    /// after printing; no modules are scored.
    #[arg(long)]
    explain: bool,
}

// ---------------------------------------------------------------------------
// Output types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct DragReport {
    config: ReportConfig,
    modules: Vec<ModuleDrag>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    packages: Vec<PackageDrag>,
}

#[derive(Serialize)]
struct ReportConfig {
    w2: f64,
    m: f64,
    p: f64,
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

/// Per-package drag summary emitted in JSON output.
#[derive(Serialize)]
struct PackageDrag {
    package: String,
    maint_score: f64,
    module_drag_sum: f64,
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
    let config = DragConfig::load(cli.config.clone(), cli.model.clone())?;
    let comp_config_hash = config.comprehensibility_hash();
    let maint_config_hash = config.maintainability_hash();

    // Initialize config early so cache path resolution (and --clear-cache)
    // respect any configured state-root overrides.
    gremlins::config::init_global().map_err(|e| e.to_string())?;

    // ── resolve cache path (--clear-cache needs it) ──
    let cache_path: PathBuf = match &config.cache {
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

    if cli.explain {
        print_explain(&config);
        return Ok(());
    }

    let path = cli.path.ok_or_else(|| {
        "<PATH> is required unless --explain or --clear-cache is used".to_string()
    })?;

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
    let model_spec = resolve_comprehensibility_model(config.model.as_deref())?;

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
        let key = compute_cache_key(
            &source,
            &model_spec,
            config.estimates_per_survey,
            &comp_config_hash,
        );
        if let Some(entry) = cache_map.get(&key) {
            // Only accept cached entries whose estimate count matches.
            if entry.estimates == config.estimates_per_survey {
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
    let tokenizer = parse_tokenizer(&config.tokenizer)?;
    let bpe = tiktoken_rs::bpe_for_tokenizer(tokenizer)
        .map_err(|e| format!("failed to load tokenizer {}: {e}", config.tokenizer))?;

    let mut token_counts: HashMap<String, usize> = HashMap::new();
    for pkg in &scoring_packages {
        let files = lang::package_files(pkg)
            .map_err(|e| format!("failed to enumerate files for {}: {e}", pkg.name()))?;
        for file in files {
            // Skip files that are gated behind #[cfg(test)] in their parent
            // module (e.g. `#[cfg(test)] mod tests;` → src/tests.rs).
            if let lang::PackageInfo::Rust(rpkg) = pkg {
                if lang::rust::is_module_file_cfg_test_gated(&file, rpkg) {
                    continue;
                }
            }
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

    // Resolve inline modules to their owning file: any graph node (from edges)
    // that has no token count inherits the count of its longest prefix that
    // does have one.  Example: `crate::foo::nested` (inline) → `crate::foo`.
    let all_graph_nodes: std::collections::HashSet<String> = edges
        .iter()
        .flat_map(|(s, d)| [s.clone(), d.clone()])
        .collect();
    for node in &all_graph_nodes {
        if token_counts.contains_key(node) {
            continue;
        }
        // Find the longest prefix that has a token count.
        let mut best: Option<(&str, usize)> = None;
        for (mod_name, &count) in &token_counts {
            if node.starts_with(mod_name)
                && node.as_str().len() > mod_name.len()
                && node.as_bytes()[mod_name.len()] == b':'
                && best.is_none_or(|(n, _)| mod_name.len() > n.len())
            {
                best = Some((mod_name, count));
            }
        }
        if let Some((_, count)) = best {
            token_counts.insert(node.clone(), count);
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

    // 7. Comprehensibility
    if config.estimates_per_survey < 1 {
        return Err("estimates_per_survey must be >= 1".to_string());
    }

    let cache_state: CacheState = cache_path.clone();

    score_comprehensibility(
        &model_spec,
        cli.verbose || config.verbose,
        config.estimates_per_survey,
        &target_modules,
        &mut scores,
        cache_state.clone(),
        module_cache,
        &config.comprehensibility,
        &comp_config_hash,
    )
    .await?;

    // Any module that still has no comprehensibility score is a hard error.
    let unscored: Vec<&str> = scores
        .iter()
        .filter_map(|s| {
            if s.comprehensibility.is_none() {
                Some(s.module.as_str())
            } else {
                None
            }
        })
        .collect();
    if !unscored.is_empty() {
        return Err(format!(
            "comprehensibility scoring failed for {} module(s): {}",
            unscored.len(),
            unscored.join(", ")
        ));
    }

    // 8. Compute the final drag scores once, after any scoring.
    for s in &mut scores {
        // SAFETY: we just verified every module has a score.
        let comp = s.comprehensibility.unwrap();
        s.drag = compute_drag(config.weights.w2, config.weights.m, s.footprint, comp);
    }
    // ── Package-level maintainability ──
    let package_drags: Vec<PackageDrag> = if !config.skip_maintainability {
        let maint_scores = maint::score_package_maintainability(
            &model_spec,
            cli.verbose || config.verbose,
            config.estimates_per_survey,
            &scoring_packages,
            cache_state.clone(),
            &config.maintainability,
            &maint_config_hash,
        )
        .await?;

        // Build a lookup: (module, file) → pkg_key.
        let module_pkg: HashMap<(&str, &str), &str> = target_modules
            .iter()
            .map(|(m, f, pkg)| ((m.as_str(), f.to_str().unwrap_or("")), pkg.as_str()))
            .collect();

        // Group module drags by package key.
        let mut pkg_module_drags: HashMap<String, Vec<f64>> = HashMap::new();
        for s in &scores {
            let key = (s.module.as_str(), s.file.as_str());
            if let Some(pkg_key) = module_pkg.get(&key) {
                pkg_module_drags
                    .entry((*pkg_key).to_string())
                    .or_default()
                    .push(s.drag);
            }
        }

        let mut pdrags = Vec::new();
        for pkg in &scoring_packages {
            let pkg_key = pkg.key();
            let pkg_name = pkg.name().to_string();
            let maint = maint_scores.get(&pkg_key).copied().unwrap_or(0.5);
            let module_drags = pkg_module_drags.get(&pkg_key);
            let module_drag_sum: f64 = module_drags.map(|v| v.iter().sum()).unwrap_or(0.0);
            let drag = maint::compute_package_drag(config.weights.p, maint, module_drag_sum);
            pdrags.push(PackageDrag {
                package: pkg_name,
                maint_score: maint,
                module_drag_sum,
                drag,
            });
        }
        pdrags
    } else {
        Vec::new()
    };

    // 9. Sort by drag descending
    scores.sort_by(|a, b| {
        b.drag
            .partial_cmp(&a.drag)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // 10. Output
    if matches!(config.format, OutputFormat::Json) {
        let report = DragReport {
            config: ReportConfig {
                w2: config.weights.w2,
                m: config.weights.m,
                p: config.weights.p,
            },
            modules: scores,
            packages: package_drags,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
        );
    } else {
        print_human_table(&scores, &package_drags, config.skip_maintainability);
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
    // Strip #[cfg(test)] items from Rust source before counting.
    // Test code inflates drag scores but moving it around does not
    // improve real maintainability — it just shifts tokens around.
    // Excluding it removes the incentive to game the metric.
    let source = if file.extension().and_then(|e| e.to_str()) == Some("rs") {
        strip_cfg_test(&source).unwrap_or_else(|e| {
            eprintln!(
                "  WARNING: {} — counting raw source (cfg(test) items NOT stripped)",
                e
            );
            source.clone()
        })
    } else {
        source
    };
    Ok(tokenizer.encode_with_special_tokens(&source).len())
}

/// Use `syn` to parse the Rust source and remove all items annotated
/// with `#[cfg(test)]`.  Regenerated source uses `quote` formatting,
/// which may differ from the original; this is fine since we only
/// count tokens.
fn strip_cfg_test(source: &str) -> Result<String, String> {
    use syn::visit_mut::VisitMut;

    let mut file =
        syn::parse_file(source).map_err(|e| format!("failed to strip #[cfg(test)]: {e}"))?;

    // Top-level items.
    file.items
        .retain(|item| !has_cfg_test_attr(item_attrs(item)));

    // Walk nested modules and strip #[cfg(test)] items inside them.
    struct CfgTestStripper;
    impl syn::visit_mut::VisitMut for CfgTestStripper {
        fn visit_file_mut(&mut self, f: &mut syn::File) {
            f.items.retain(|item| !has_cfg_test_attr(item_attrs(item)));
            syn::visit_mut::visit_file_mut(self, f);
        }
        fn visit_item_mod_mut(&mut self, m: &mut syn::ItemMod) {
            if let Some((_, ref mut items)) = &mut m.content {
                items.retain(|item| !has_cfg_test_attr(item_attrs(item)));
            }
            syn::visit_mut::visit_item_mod_mut(self, m);
        }
        fn visit_item_impl_mut(&mut self, i: &mut syn::ItemImpl) {
            i.items.retain(|ii| !has_cfg_test_attr(impl_item_attrs(ii)));
            syn::visit_mut::visit_item_impl_mut(self, i);
        }
    }
    CfgTestStripper.visit_file_mut(&mut file);

    use quote::ToTokens;
    Ok(file.into_token_stream().to_string())
}

fn has_cfg_test_attr(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().segments.iter().any(|s| s.ident == "cfg")
            && a.meta.require_list().is_ok_and(|meta| {
                let tokens = meta.tokens.to_string();
                // Match test as a standalone token: `test` but not `test_foo`.
                let trimmed = tokens.trim();
                trimmed == "test"
                    || trimmed.contains(", test,")
                    || trimmed.starts_with("test,")
                    || trimmed.ends_with(", test")
            })
    })
}

fn item_attrs(item: &syn::Item) -> &[syn::Attribute] {
    use syn::Item;
    match item {
        Item::Const(i) => &i.attrs,
        Item::Enum(i) => &i.attrs,
        Item::ExternCrate(i) => &i.attrs,
        Item::Fn(i) => &i.attrs,
        Item::ForeignMod(i) => &i.attrs,
        Item::Impl(i) => &i.attrs,
        Item::Macro(i) => &i.attrs,
        Item::Mod(i) => &i.attrs,
        Item::Static(i) => &i.attrs,
        Item::Struct(i) => &i.attrs,
        Item::Trait(i) => &i.attrs,
        Item::TraitAlias(i) => &i.attrs,
        Item::Type(i) => &i.attrs,
        Item::Union(i) => &i.attrs,
        Item::Use(i) => &i.attrs,
        _ => &[],
    }
}

fn impl_item_attrs(item: &syn::ImplItem) -> &[syn::Attribute] {
    match item {
        syn::ImplItem::Const(c) => &c.attrs,
        syn::ImplItem::Fn(f) => &f.attrs,
        syn::ImplItem::Type(t) => &t.attrs,
        syn::ImplItem::Macro(m) => &m.attrs,
        syn::ImplItem::Verbatim(_) => &[],
        _ => &[],
    }
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

/// Dimension key → score (0–4). Keys are defined by the comprehensibility
/// config; a missing key means the model omitted that dimension.
type ComprehensibilityScores = HashMap<String, u8>;

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
/// the resolved model URI, the estimate count, and the survey config hash.
///
/// Key = first 16 hex chars of
/// `blake3(content || "\0" || model_uri || "\0" || estimates || "\0" || config_hash)`.
/// The 64-bit truncation is fine at realistic cache sizes but collision
/// risk grows past ~10⁵–10⁶ entries for strict uniqueness guarantees.
fn compute_cache_key(
    content: &str,
    model_uri: &str,
    estimates: usize,
    config_hash: &str,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(content.as_bytes());
    hasher.update(b"\0");
    hasher.update(model_uri.as_bytes());
    hasher.update(b"\0");
    hasher.update(&estimates.to_le_bytes());
    hasher.update(b"\0");
    hasher.update(config_hash.as_bytes());
    hasher.finalize().to_hex()[..16].to_string()
}

/// Load a JSONL cache file, returning a map keyed by `CacheEntry::key`.
///
/// An empty or missing file is treated as a cold cache (empty map).
/// Lines that are not valid JSON or fail to deserialize as a [`CacheEntry`]
/// are skipped with a warning (run with `--clear-cache` to rebuild).
fn load_cache(path: &Path) -> Result<HashMap<String, CacheEntry>, String> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read cache file {}: {e}", path.display()))?;
    let mut map = HashMap::new();
    for (line_no, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<CacheEntry>(line) {
            Ok(entry) => {
                map.insert(entry.key.clone(), entry);
            }
            Err(e) => {
                eprintln!(
                    "  WARNING: {}:{}: skipping invalid cache entry: {e}. Run with --clear-cache to rebuild.",
                    path.display(),
                    line_no + 1
                );
            }
        }
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
fn resolve_comprehensibility_model(model_opt: Option<&str>) -> Result<String, String> {
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

/// Build a compact structural skeleton for a source file, suitable for
/// prepending to the LLM prompt. Uses the landscape scanner for item-level
/// structure and source text for line counts.
fn build_file_skeleton(source: &str, extension: &str) -> String {
    use std::collections::BTreeMap;

    let total_lines = source.lines().count();

    // Run the landscape scanner on the raw source (same regex the explorer
    // uses) to get outline items with test modules collapsed.
    let items = gremlins::landscape::heuristic::scan(extension, source, true);
    let items_no_collapse = gremlins::landscape::heuristic::scan(extension, source, false);

    // Count production functions (non-test)
    let prod_fns: Vec<_> = items.iter().filter(|i| i.kind == "fn").collect();
    let test_fns: Vec<_> = items_no_collapse
        .iter()
        .filter(|i| i.kind == "fn")
        .collect();
    let test_fn_count = test_fns.len().saturating_sub(prod_fns.len());

    // Submodule count (mod items not named "tests")
    let submodule_count = items
        .iter()
        .filter(|i| i.kind == "mod" && i.name != "tests")
        .count();

    // Largest function: approximate by computing spans between consecutive items
    let mut largest_fn_lines = 0usize;
    let mut all_items_sorted: Vec<(usize, &str, &str)> = items
        .iter()
        .map(|i| (i.line, i.kind, i.name.as_str()))
        .collect();
    all_items_sorted.sort_by_key(|(l, _, _)| *l);
    for w in all_items_sorted.windows(2) {
        if w[0].1 == "fn" {
            let span = w[1].0.saturating_sub(w[0].0);
            if span > largest_fn_lines {
                largest_fn_lines = span;
            }
        }
    }
    // Also check the last item if it's a function
    if let Some(last) = all_items_sorted.last() {
        if last.1 == "fn" {
            let span = total_lines.saturating_sub(last.0);
            if span > largest_fn_lines {
                largest_fn_lines = span;
            }
        }
    }

    // Estimate test ratio: find the mod tests entry and compute its span
    let test_ratio_pct =
        if let Some(test_mod) = items.iter().find(|i| i.name == "tests" && i.kind == "mod") {
            // Approximate: the test module starts at test_mod.line and goes to end of file
            let test_lines = total_lines.saturating_sub(test_mod.line.saturating_sub(1));
            ((test_lines as f64 / total_lines as f64) * 100.0) as usize
        } else {
            0
        };

    // Concern clusters: group fns by prefix heuristic
    let mut clusters: BTreeMap<String, usize> = BTreeMap::new();
    for item in &prod_fns {
        let prefix = item.name.split('_').next().unwrap_or(&item.name);
        *clusters.entry(prefix.to_string()).or_default() += 1;
    }
    // Only show clusters with >= 2 fns
    let cluster_parts: Vec<String> = clusters
        .iter()
        .filter(|(_, &c)| c >= 2)
        .map(|(prefix, count)| format!("{prefix} ({count} fns)"))
        .collect();

    // Format outline items compactly
    let mut outline_parts: Vec<String> = Vec::new();
    for item in &items {
        outline_parts.push(format!(
            "  {}|{}|{}",
            item.line,
            item.kind,
            gremlins::landscape::escape_landscape_delimiters(&item.name)
        ));
    }

    let mut skeleton = String::new();
    skeleton.push_str(&format!(
        "FILE SHAPE: {total_lines} lines, {} functions, {} submodules, {} test functions\n",
        prod_fns.len(),
        submodule_count,
        test_fn_count,
    ));
    if largest_fn_lines > 0 {
        skeleton.push_str(&format!(
            "Largest function span: ~{largest_fn_lines} lines\n"
        ));
    }
    skeleton.push_str(&format!("Test ratio: {test_ratio_pct}% of file\n"));
    skeleton.push_str(&format!("Submodule count: {submodule_count}\n"));
    if !cluster_parts.is_empty() {
        skeleton.push_str(&format!("Concern clusters: {}\n", cluster_parts.join(", ")));
    }
    skeleton.push_str("\nStructure:\n");
    skeleton.push_str(&outline_parts.join("\n"));

    skeleton
}

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

#[allow(clippy::too_many_arguments)]
async fn score_comprehensibility(
    model_spec: &str,
    verbose: bool,
    estimates: usize,
    modules: &[(String, PathBuf, String)],
    scores: &mut [ModuleDrag],
    cache_state: CacheState,
    module_cache: HashMap<String, CacheEntry>,
    comprehensibility_config: &ComprehensibilityConfig,
    config_hash: &str,
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
        let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("rs");
        let skeleton = build_file_skeleton(&source, ext);
        let prompt = format!("{skeleton}\n\nSOURCE FILE ({module}):\n\n```{fence}\n{source}\n```");

        let token_count = module_tokens.get(module.as_str()).copied().unwrap_or(0);

        for _ in 0..estimates {
            let module = module.clone();
            let source = source.clone();
            let abs_path = abs_path.clone();
            let prompt = prompt.clone();
            let model_spec = model_spec.to_string();
            let comp_config = comprehensibility_config.clone();
            let config_hash = config_hash.to_string();

            handles.push(tokio::spawn(async move {
                let cache_key = compute_cache_key(&source, &model_spec, estimates, &config_hash);
                let dim_keys: Vec<String> = comp_config
                    .dimensions
                    .iter()
                    .map(|d| d.key.clone())
                    .collect();

                for attempt in 0..MAX_COMP_RETRIES {
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
                        Ok(c) => {
                            let sum: f64 = dim_keys
                                .iter()
                                .map(|k| *c.get(k).unwrap_or(&0) as f64)
                                .sum();
                            let raw = sum / (dim_keys.len() * 4) as f64;
                            let score = raw.clamp(0.0, 1.0);

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
        if med >= 1.0 {
            eprintln!("  [{module}] WARNING: perfect score (all 4s)");
        }
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

// ---------------------------------------------------------------------------
// Drag explanation
// ---------------------------------------------------------------------------

/// Print a self-contained explanation of the drag calculation: the formula,
/// the comprehensibility and maintainability surveys, and how each factor
/// responds to change (curves). This is the single source of truth for the
/// drag model; `gremlins-drag --explain` feeds the drag-optimizer pipeline so
/// its prompts never hardcode a second copy.
fn print_explain(config: &DragConfig) {
    let w2 = config.weights.w2;
    let m = config.weights.m;
    let p = config.weights.p;
    let factor_footprint = |f: usize| -> f64 { (1.0 + f as f64).powf(w2) };
    let factor_comp = |c: f64| -> f64 { (m * (1.0 - c)).exp() };
    let factor_maint = |c: f64| -> f64 { (p * (1.0 - c)).exp() };

    println!("# Drag model\n");
    println!("## Formula\n");
    println!("  drag(m) = (1+footprint)^w2 · exp(m·(1−comprehensibility))");
    println!("  drag(p) = exp(p·(1−maint)) · Σ drag(m)   (Σ over modules in package p)\n");
    println!("  The total minimized is the sum of drag across all items:");
    println!("  - packages present → Σ drag(p)");
    println!("  - modules only     → Σ drag(m)\n");
    println!("  `footprint` is the token count of a module plus every module it");
    println!("  transitively reaches (including itself). Higher drag means the");
    println!("  module or package is harder to maintain and change.");
    println!("  Token counts are computed with `#[cfg(test)]` items stripped: test code");
    println!("  never contributes to token_count or footprint, but it does affect the");
    println!("  test_hygiene comprehensibility dimension.\n");

    let comp_dims: &[ComprehensibilityDimensionDef] = &config.comprehensibility.dimensions;
    let n_dims = comp_dims.len();
    println!("## Comprehensibility survey\n");
    if !config.comprehensibility.preamble.is_empty() {
        println!("### Preamble\n");
        for line in config.comprehensibility.preamble.lines() {
            println!("  {line}");
        }
        println!();
    }
    if !config.comprehensibility.generic_anchors.is_empty() {
        println!("### Generic anchors (0–4 scale)\n");
        for line in config.comprehensibility.generic_anchors.lines() {
            println!("  {line}");
        }
        println!();
    }
    println!("  Comprehensibility is scored by an LLM survey on {n_dims} dimensions, each");
    println!(
        "  rated 0–4 and normalized over {} points ({n_dims}×4):\n",
        n_dims * 4
    );
    for (i, dim) in comp_dims.iter().enumerate() {
        println!("  {}. {}  — {}", i + 1, dim.label, dim.description);
        if let Some(anchors) = config.comprehensibility.structural_anchors.get(&dim.key) {
            println!("     Anchors:");
            for line in anchors.lines() {
                println!("     {line}");
            }
        }
    }
    println!();
    println!("  A FILE SHAPE skeleton is prepended to the scoring prompt: total lines,");
    println!("  function count, submodule count, test-function count, largest function");
    println!("  span, test ratio, concern clusters, and the item outline. Structural");
    println!("  pathologies — god-files, god-functions, inline test bloat, zero submodules —");
    println!("  depress the score directly.\n");

    let n_maint = config.maintainability.dimensions.len();
    println!("## Package maintainability survey\n");
    if !config.maintainability.preamble.is_empty() {
        println!("### Preamble\n");
        for line in config.maintainability.preamble.lines() {
            println!("  {line}");
        }
        println!();
    }
    println!("  Package maintainability is scored on {n_maint} dimensions, each 0–4,");
    println!(
        "  normalized over {} points ({n_maint}×4). Only computed when packages are",
        n_maint * 4
    );
    println!("  present and skip_maintainability is not set.\n");
    for (i, dim) in config.maintainability.dimensions.iter().enumerate() {
        println!("  {}. {}", i + 1, dim.description);
        if !dim.anchors.is_empty() {
            println!("     Anchors:");
            for line in dim.anchors.lines() {
                println!("     {line}");
            }
        }
    }

    println!("# Drag curves (w2={w2}, m={m}, p={p})\n");
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

    println!("\n  Maintainability factor (exp(p·(1−maint)), p={p}):");
    for maint in [0.10, 0.25, 0.50, 0.75, 0.90, 1.00] {
        println!("    {maint:.2}  → {:>10.3}", factor_maint(maint));
    }

    println!("\n  All factors are multiplied together; larger values mean more drag.");
    println!("  Comprehensibility below 1.0 amplifies drag by up to exp(m).");
    println!("  Maintainability below 1.0 amplifies package drag by up to exp(p).");
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

fn print_human_table(scores: &[ModuleDrag], package_drags: &[PackageDrag], skip_maint: bool) {
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

    if !skip_maint && !package_drags.is_empty() {
        println!();
        println!(
            "{:<55} {:>12} {:>14} {:>14}",
            "package", "maint", "Σ module drag", "package drag"
        );
        println!("{}", "-".repeat(99));
        for p in package_drags {
            println!(
                "{:<55} {:>12.3} {:>14.1} {:>14.1}",
                truncate(&p.package, 54),
                p.maint_score,
                p.module_drag_sum,
                p.drag,
            );
        }
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
        let scores: serde_json::Value = serde_json::from_str(&extracted).unwrap();
        assert_eq!(scores["naming"], 3);
        assert_eq!(scores["control_flow"], 2);
    }

    #[test]
    fn extract_json_fenced() {
        let raw = "```json\n{\"naming\": 3, \"control_flow\": 2, \"data_flow\": 2, \"abstractions\": 3}\n```";
        let extracted = extract_json(raw).unwrap();
        let scores: serde_json::Value = serde_json::from_str(&extracted).unwrap();
        assert_eq!(scores["naming"], 3);
    }

    #[test]
    fn extract_json_fenced_no_lang() {
        let raw =
            "```\n{\"naming\": 3, \"control_flow\": 2, \"data_flow\": 2, \"abstractions\": 3}\n```";
        let extracted = extract_json(raw).unwrap();
        let scores: serde_json::Value = serde_json::from_str(&extracted).unwrap();
        assert_eq!(scores["naming"], 3);
    }

    #[test]
    fn extract_json_with_text_before() {
        let raw = "Here is my assessment of the file:\n\n{\"naming\": 3, \"control_flow\": 2, \"data_flow\": 2, \"abstractions\": 3}\n\nHope that helps.";
        let extracted = extract_json(raw).unwrap();
        let scores: serde_json::Value = serde_json::from_str(&extracted).unwrap();
        assert_eq!(scores["naming"], 3);
    }

    #[test]
    fn extract_json_with_trailing_comma() {
        let raw = "{\"naming\": 3, \"control_flow\": 2, \"data_flow\": 2, \"abstractions\": 3,}";
        let extracted = extract_json(raw).unwrap();
        let scores: serde_json::Value = serde_json::from_str(&extracted).unwrap();
        assert_eq!(scores["naming"], 3);
    }

    #[test]
    fn extract_json_with_line_comment() {
        let raw = "{\n// scoring notes\n\"naming\": 3,\n\"control_flow\": 2,\n\"data_flow\": 2,\n\"abstractions\": 3\n}";
        let extracted = extract_json(raw).unwrap();
        let scores: serde_json::Value = serde_json::from_str(&extracted).unwrap();
        assert_eq!(scores["naming"], 3);
    }

    #[test]
    fn extract_json_with_block_comment() {
        let raw = "{\n/* scoring notes */\n\"naming\": 3,\n\"control_flow\": 2,\n\"data_flow\": 2,\n\"abstractions\": 3\n}";
        let extracted = extract_json(raw).unwrap();
        let scores: serde_json::Value = serde_json::from_str(&extracted).unwrap();
        assert_eq!(scores["naming"], 3);
    }

    #[test]
    fn extract_json_fenced_with_text_and_trailing_comma() {
        // The most common failure mode: markdown fence, trailing comma, extra text.
        let raw = "Here's the score:\n\n```json\n{\n  \"naming\": 3,\n  \"control_flow\": 2,\n  \"data_flow\": 2,\n  \"abstractions\": 3,\n}\n```\n\nLet me know if you need anything else.";
        let extracted = extract_json(raw).unwrap();
        let scores: serde_json::Value = serde_json::from_str(&extracted).unwrap();
        assert_eq!(scores["naming"], 3);
    }

    #[test]
    fn extract_json_missing_closing_fence() {
        let raw =
            "```json\n{\"naming\": 3, \"control_flow\": 2, \"data_flow\": 2, \"abstractions\": 3}";
        let extracted = extract_json(raw).unwrap();
        let scores: serde_json::Value = serde_json::from_str(&extracted).unwrap();
        assert_eq!(scores["naming"], 3);
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
        let k1 = compute_cache_key("fn main() {}", "openai/gpt-4o", 1, "cfg-a");
        let k2 = compute_cache_key("fn main() {}", "openai/gpt-4o", 1, "cfg-a");
        assert_eq!(k1, k2);
        assert_eq!(k1.len(), 16);
        // Different content → different key.
        let k3 = compute_cache_key("fn main() { /* changed */ }", "openai/gpt-4o", 1, "cfg-a");
        assert_ne!(k1, k3);
        // Different model → different key.
        let k4 = compute_cache_key("fn main() {}", "anthropic/claude", 1, "cfg-a");
        assert_ne!(k1, k4);
        // Different estimates → different key.
        let k5 = compute_cache_key("fn main() {}", "openai/gpt-4o", 5, "cfg-a");
        assert_ne!(k1, k5);
        // Different config hash → different key.
        let k6 = compute_cache_key("fn main() {}", "openai/gpt-4o", 1, "cfg-b");
        assert_ne!(k1, k6);
    }

    #[test]
    fn load_cache_missing_file_is_cold() {
        let map = load_cache(Path::new("/nonexistent/cache/file.jsonl")).unwrap();
        assert!(map.is_empty());
    }

    #[test]
    fn load_cache_blank_lines_skipped() {
        let dir = std::env::temp_dir().join("drag-test-blank-line");
        let _ = std::fs::create_dir_all(&dir);
        let cache_path = dir.join("cache.jsonl");
        // Line 2 is blank; it should be skipped, leaving the valid entry.
        std::fs::write(
            &cache_path,
            "{\"key\":\"abc\",\"file\":\"/f.rs\",\"model\":\"m\",\"token_count\":10,\"comprehensibility\":0.5,\"breakdown\":{\"naming\":2,\"control_flow\":2,\"data_flow\":2,\"abstractions\":2,\"cohesion\":1,\"sizing\":1,\"test_hygiene\":1}}\n\n",
        )
        .unwrap();
        let map = load_cache(&cache_path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("abc"));
    }

    #[test]
    fn load_cache_malformed_line_skipped() {
        let dir = std::env::temp_dir().join("drag-test-malformed");
        let _ = std::fs::create_dir_all(&dir);
        let cache_path = dir.join("cache.jsonl");
        std::fs::write(&cache_path, "not valid json\n").unwrap();
        let map = load_cache(&cache_path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(map.is_empty());
    }

    #[test]
    fn load_cache_valid_entries() {
        let dir = std::env::temp_dir().join("drag-test-valid");
        let _ = std::fs::create_dir_all(&dir);
        let cache_path = dir.join("cache.jsonl");
        let line = "{\"key\":\"deadbeef12345678\",\"file\":\"/src/main.rs\",\"model\":\"openai/gpt-4o\",\"token_count\":42,\"comprehensibility\":0.75,\"breakdown\":{\"naming\":3,\"control_flow\":3,\"data_flow\":3,\"abstractions\":3,\"cohesion\":3,\"sizing\":3,\"test_hygiene\":3}}\n";
        std::fs::write(&cache_path, line).unwrap();
        let map = load_cache(&cache_path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(map.len(), 1);
        let entry = map.get("deadbeef12345678").unwrap();
        assert_eq!(entry.comprehensibility, 0.75);
        assert_eq!(entry.breakdown.get("naming"), Some(&3));
    }

    #[test]
    fn load_cache_accepts_v1_entries_ignoring_graph_fields() {
        let dir = std::env::temp_dir().join("drag-test-v1-reject");
        let _ = std::fs::create_dir_all(&dir);
        let cache_path = dir.join("cache.jsonl");
        let line = "{\"key\":\"deadbeef12345678\",\"file\":\"/src/main.rs\",\"model\":\"openai/gpt-4o\",\"token_count\":42,\"fan_in\":1,\"fan_out\":2,\"graph_complexity\":3,\"comprehensibility\":0.75,\"breakdown\":{\"naming\":3,\"control_flow\":3,\"data_flow\":3,\"abstractions\":3,\"cohesion\":3,\"sizing\":3,\"test_hygiene\":3}}\n";
        std::fs::write(&cache_path, line).unwrap();
        let map = load_cache(&cache_path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("deadbeef12345678"));
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
            breakdown: [
                ("naming", 2),
                ("control_flow", 2),
                ("data_flow", 2),
                ("abstractions", 2),
                ("cohesion", 0),
                ("sizing", 0),
                ("test_hygiene", 0),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
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

    // ── strip_cfg_test ──────────────────────────────────────────────

    #[test]
    fn strip_cfg_test_removes_test_module() {
        let src =
            "fn main() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn it_works() {}\n}\n";
        let stripped = strip_cfg_test(src).unwrap();
        // The test module should be gone, main should remain.
        assert!(stripped.contains("fn main"));
        assert!(!stripped.contains("it_works"));
    }
    #[test]
    fn strip_cfg_test_keeps_regular_module() {
        let src = "pub mod util {\n    pub fn helper() {}\n}\n";
        let stripped = strip_cfg_test(src).unwrap();
        assert!(stripped.contains("mod util"));
        assert!(stripped.contains("fn helper"));
    }
    #[test]
    fn strip_cfg_test_parse_failure_is_error() {
        let src = "fn broken(\n";
        let result = strip_cfg_test(src);
        assert!(result.is_err());
    }

    #[test]
    fn strip_cfg_test_strips_cfg_test_fn() {
        let src = "fn main() {}\n\n#[cfg(test)]\nfn test_helper() {}\n";
        let stripped = strip_cfg_test(src).unwrap();
        assert!(stripped.contains("fn main"));
        assert!(!stripped.contains("test_helper"));
    }
    #[test]
    fn strip_cfg_test_strips_cfg_test_impl() {
        let src = "struct Foo;\n\n#[cfg(test)]\nimpl Foo {\n    fn test_stuff() {}\n}\n";
        let stripped = strip_cfg_test(src).unwrap();
        assert!(stripped.contains("struct Foo"));
        assert!(!stripped.contains("test_stuff"));
    }
}
