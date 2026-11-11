mod cache;
mod comp_agent;
mod config;
mod lang;
mod maint;
mod output;
mod scoring;
mod util;

use config::{DragConfig, OutputFormat};
use scoring::{build_drag_tree, compute_drag, compute_footprint, DragNodeOutput};
use util::{CacheState, MAX_CONCURRENT_SCORING_RUNS};

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use std::sync::Arc;

use clap::Parser;
use serde::Serialize;

use tokio::sync::Semaphore;

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
    #[serde(flatten)]
    config: scoring::ReportConfig,
    #[serde(flatten)]
    tree: DragNodeOutput,
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
        output::print_explain(&config);
        return Ok(());
    }

    // When GREMLINS_DRAG_CLI_CONFIG is set, the target comes from
    // config.path; otherwise it must be passed as a positional argument.
    let path = cli
        .path
        .or_else(|| config.path.clone().map(PathBuf::from))
        .ok_or_else(|| {
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
    lang::dedup_target_modules(&mut target_modules, &packages);

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
    let model_spec = comp_agent::resolve_comprehensibility_model(config.model.as_deref())?;

    let cache_map = cache::load_cache(&cache_path)?;
    eprintln!(
        "Loaded {} cached entries from {}",
        cache_map.len(),
        cache_path.display()
    );

    // ── pre-compute cache keys and build module → CacheEntry lookup ──
    let mut module_cache: HashMap<String, cache::CacheEntry> = HashMap::new();
    for (module, file, _pkg) in &target_modules {
        let source = std::fs::read_to_string(file).unwrap_or_default();
        if source.trim().is_empty() {
            continue;
        }
        let key = cache::compute_cache_key(
            &source,
            &model_spec,
            config.estimates_per_survey,
            &comp_config_hash,
        );
        if let Some(entry) = cache_map.get(&key) {
            // Only accept cached entries whose estimate count matches and
            // whose raw_estimates are populated (legacy entries may have
            // an empty vec from #[serde(default)]).
            if entry.estimates == config.estimates_per_survey
                && entry.raw_estimates.len() == config.estimates_per_survey
            {
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

    let mut scores: Vec<scoring::ModuleDrag> = Vec::new();
    for (module, file, _pkg_key) in &target_modules {
        let token_count = token_counts.get(module).copied().unwrap_or(0);
        let footprint = footprint_map.get(module).copied().unwrap_or(0);

        scores.push(scoring::ModuleDrag {
            module: module.clone(),
            file: file.to_string_lossy().into_owned(),
            token_count,
            footprint,
            comprehensibility: None,
            ci_lower: None,
            ci_upper: None,
            drag: 0.0,
        });
    }

    // Shared semaphore to bound concurrent ephemeral gremlin runs.
    // Created once and passed to both scoring phases so the permit count
    // is enforced globally regardless of how the phases are composed.
    let scoring_sem = Arc::new(Semaphore::new(MAX_CONCURRENT_SCORING_RUNS));

    // 7. Comprehensibility
    if config.estimates_per_survey < 1 {
        return Err("estimates_per_survey must be >= 1".to_string());
    }

    let cache_state: CacheState = cache_path.clone();

    comp_agent::score_comprehensibility(
        &model_spec,
        cli.verbose || config.verbose,
        config.estimates_per_survey,
        &target_modules,
        &mut scores,
        cache_state.clone(),
        module_cache,
        &config.comprehensibility,
        &comp_config_hash,
        Arc::clone(&scoring_sem),
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

    // ── Directory-level maintainability ──
    // Enumerate every directory under each scoring package's source tree
    // that contains source files, then score each one.
    let (maint_scores, skipped_dirs): (HashMap<String, maint::DirMaintResult>, HashSet<String>) =
        if !config.skip_maintainability {
            let scoring_dirs: Vec<PathBuf> = {
                let mut dirs = Vec::new();
                for pkg in &scoring_packages {
                    match lang::enumerate_source_dirs(pkg) {
                        Ok(pkg_dirs) => dirs.extend(pkg_dirs),
                        Err(e) => eprintln!(
                            "gremlins-drag: directory enumeration skipped for {}: {e}",
                            pkg.name()
                        ),
                    }
                }
                // Deduplicate (shared roots possible across nested Python packages).
                dirs.sort();
                dirs.dedup();
                dirs
            };

            let (maint_scores, skipped_dirs) = maint::score_directory_maintainability(
                &model_spec,
                cli.verbose || config.verbose,
                config.estimates_per_survey,
                &scoring_dirs,
                cache_state.clone(),
                &config.maintainability,
                &maint_config_hash,
                Arc::clone(&scoring_sem),
            )
            .await?;
            (maint_scores, skipped_dirs)
        } else {
            (
                HashMap::<String, maint::DirMaintResult>::new(),
                HashSet::new(),
            )
        };

    // Build module→pkg, pkg_key→pkg_name, and per-module directory-segment maps.
    let module_pkg: HashMap<(String, String), String> = target_modules
        .iter()
        .map(|(m, f, pkg)| ((m.clone(), f.to_string_lossy().into_owned()), pkg.clone()))
        .collect();

    let module_dir_segments: HashMap<(String, String), Vec<String>> = {
        let mut map = HashMap::new();
        for (m, f, pkg_key) in &target_modules {
            if let Some(pkg) = scoring_packages.iter().find(|p| p.key() == *pkg_key) {
                let file = PathBuf::from(f);
                if let Some(segs) = lang::file_to_dir_segments(&file, pkg) {
                    map.insert((m.clone(), f.to_string_lossy().into_owned()), segs);
                }
            }
        }
        map
    };

    let pkg_names: HashMap<String, String> = scoring_packages
        .iter()
        .map(|p| (p.key(), p.name().to_string()))
        .collect();

    let pkg_src_dirs: HashMap<String, PathBuf> = scoring_packages
        .iter()
        .map(|p| (p.key(), p.src_dir()))
        .collect();

    // ── Build drag tree ──
    let tree = build_drag_tree(
        &scores,
        &module_pkg,
        &module_dir_segments,
        &maint_scores,
        &skipped_dirs,
        &pkg_names,
        &pkg_src_dirs,
        config.weights.p,
        config.skip_maintainability,
    )?;

    // 9. Output
    if matches!(config.format, OutputFormat::Json) {
        let report = DragReport {
            config: scoring::ReportConfig {
                w2: config.weights.w2,
                m: config.weights.m,
                p: config.weights.p,
            },
            tree: DragNodeOutput::from(&tree),
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
        );
    } else {
        output::print_human_table(&tree, config.skip_maintainability);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizer_names_parse() {
        assert!(parse_tokenizer("o200k_base").is_ok());
        assert!(parse_tokenizer("bogus").is_err());
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
