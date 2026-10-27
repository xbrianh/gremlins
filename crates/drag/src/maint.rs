// Package maintainability scoring for gremlins-drag.
//
// This module handles:
// - Building a landscape of a package directory.
// - Prompting ten independent, per-dimension Agent stages that run
//   concurrently, each scoring a single dimension (0–4) with tailored
//   anchors so every dimension gets focused attention.
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
use crate::util::{median, truncate, CacheState, CompAgentError};

// ---------------------------------------------------------------------------
// Dimension registry
// ---------------------------------------------------------------------------

/// Definition for one maintainability dimension.
struct MaintDimension {
    /// Field name in `PackageMaintainability` (also used as the dimension key).
    name: &'static str,
    /// Human-readable label for logging.
    label: &'static str,
    /// One-paragraph description of what the dimension measures.
    description: &'static str,
    /// Concrete 0–4 anchors tailored to this dimension.
    anchors: &'static str,
}

/// Dimension name constants — referenced by both the registry and
/// `from_dim_map` so renaming a dimension requires only one edit.
const DIM_DECOMPOSITION: &str = "decomposition";
const DIM_INTERFACE_DESIGN: &str = "interface_design";
const DIM_CONSISTENCY: &str = "consistency";
const DIM_CHANGE_ISOLATION: &str = "change_isolation";
const DIM_DUPLICATION: &str = "duplication";
const DIM_LOCALITY: &str = "locality";
const DIM_ABSTRACTION_CLARITY: &str = "abstraction_clarity";
const DIM_CONCEPTUAL_SURFACE: &str = "conceptual_surface";
const DIM_PREDICTABILITY: &str = "predictability";
const DIM_EVASION: &str = "evasion";

/// The ten maintainability dimensions, each with tailored anchors.
const DIMENSIONS: &[MaintDimension] = &[
    MaintDimension {
        name: DIM_DECOMPOSITION,
        label: "decomposition",
        description: "Decomposition: Are modules appropriately sized and focused? \
             Does each module have a clear, singular responsibility?",
        anchors: "\
0: Nearly every module is a dumping ground — files mix unrelated concerns and \
responsibilities are scattered.\n\
1: Some modules have a clear focus, but several are overloaded or split \
arbitrarily.\n\
2: Most modules have a discernible responsibility, though a few could be \
split or merged.\n\
3: Modules are focused and responsibilities are clearly separated; minor \
boundary adjustments possible.\n\
4: Every module has a single, well-chosen responsibility; the decomposition \
would serve as a model for other packages.",
    },
    MaintDimension {
        name: DIM_INTERFACE_DESIGN,
        label: "interface design",
        description: "Interface design: Are cross-module contracts minimal and clear? \
             Are public APIs narrow and well-defined?",
        anchors: "\
0: Public APIs expose internal details indiscriminately; modules expose \
dozens of unrelated items.\n\
1: Some modules have reasonable public surfaces, but several are bloated or \
leaky.\n\
2: Most public APIs are reasonably narrow; a few modules expose more than \
necessary.\n\
3: Public APIs are consistently narrow and well-chosen; imports are mostly \
what you'd expect.\n\
4: Every public item is intentionally exposed; the API surface is minimal, \
coherent, and discoverable.",
    },
    MaintDimension {
        name: DIM_CONSISTENCY,
        label: "consistency",
        description: "Consistency: Do naming, error handling, and abstraction levels \
             stay uniform across the package?",
        anchors: "\
0: Naming conventions, error strategies, and abstraction levels vary wildly \
across files.\n\
1: Some patterns are repeated, but the package feels like it was written by \
several teams with different conventions.\n\
2: Conventions are mostly followed; a few outliers exist but don't dominate.\n\
3: The package reads uniformly; naming, error handling, and abstraction \
levels are consistent with only minor deviations.\n\
4: The entire package feels like a single author wrote it with a clear style \
guide; consistency is exemplary.",
    },
    MaintDimension {
        name: DIM_CHANGE_ISOLATION,
        label: "change isolation",
        description: "Change isolation: Would a typical feature change touch one \
             module or scatter across many?",
        anchors: "\
0: Any non-trivial change ripples across most of the package; modules are \
tightly coupled.\n\
1: Some changes are local, but many require coordinated edits across several \
files.\n\
2: Most feature changes touch 1–2 modules; cross-cutting changes are the \
exception.\n\
3: Modules are well-isolated; typical changes are confined to a single module \
with rare cross-module impact.\n\
4: The package is organized so that every likely change is local to one \
module; coupling is minimal and intentional.",
    },
    MaintDimension {
        name: DIM_DUPLICATION,
        label: "duplication",
        description: "Code duplication: Are repeated blocks of logic extracted into \
             shared helpers? Is copy-paste minimal across the package?",
        anchors: "\
0: Substantial copy-paste across files; the same logic appears verbatim in \
3+ places.\n\
1: Several duplicated blocks exist; some could be extracted but aren't.\n\
2: Occasional duplication where extraction would add more indirection than \
value; mostly DRY.\n\
3: Very little duplication; shared logic is extracted into appropriate \
helpers or base types.\n\
4: No meaningful duplication anywhere; every piece of logic has a single \
canonical home.",
    },
    MaintDimension {
        name: DIM_LOCALITY,
        label: "locality",
        description: "Locality: Can a developer understand what a module does by \
             reading it and its direct dependencies, or does understanding \
             require tracing deep transitive chains across the package?",
        anchors: "\
0: Every module transitively imports 80%+ of the package; nothing can be \
understood in isolation.\n\
1: Many modules have deep transitive dependency chains; understanding one \
requires tracing through half the package.\n\
2: Most modules can be understood from their direct deps plus 1–2 levels of \
transitive deps.\n\
3: Modules are mostly understandable from their direct dependencies alone; \
transitive knowledge is rarely needed.\n\
4: Every module is understandable from its direct dependencies alone; the \
dependency graph is shallow and clear.",
    },
    MaintDimension {
        name: DIM_ABSTRACTION_CLARITY,
        label: "abstraction clarity",
        description: "Abstraction clarity: When reading the code, is the core intent \
             immediately visible, or is it buried under layers of indirection, \
             framework machinery, and cross-cutting concerns?",
        anchors: "\
0: The core logic is buried under so many layers of indirection that the \
reader cannot discern what the code actually does.\n\
1: Heavy use of indirection; the reader must untangle several layers to find \
the essential logic.\n\
2: Some indirection exists, but the main flow is discernible with moderate \
effort.\n\
3: Abstractions are mostly helpful; the core intent is clear after a quick \
skim.\n\
4: Abstractions reveal rather than hide intent; the code reads like a \
well-written narrative.",
    },
    MaintDimension {
        name: DIM_CONCEPTUAL_SURFACE,
        label: "conceptual surface area",
        description: "Conceptual surface area: Is the package's public API surface \
             small, focused, and easy to internalize? Or does it expose so \
             many distinct types, traits, functions, and behavioral modes \
             that a developer cannot make changes confidently?",
        anchors: "\
0: The surface area is enormous — hundreds of public items with subtle \
interactions; no one can hold it in their head.\n\
1: The surface is large and several concepts interact in surprising ways; \
developers must consult docs constantly.\n\
2: The surface is manageable; most concepts are straightforward, with a few \
complex interactions.\n\
3: The surface is small and well-documented; a developer can internalize it \
quickly.\n\
4: The surface is minimal, orthogonal, and instantly grokkable; every public \
concept pulls its weight.",
    },
    MaintDimension {
        name: DIM_PREDICTABILITY,
        label: "predictability",
        description: "Predictability: Does the code behave the way its names and \
             public API suggest, or are there hidden behaviors, implicit \
             invariants, and surprising edge cases?",
        anchors: "\
0: Names and signatures are actively misleading; behavior contradicts what \
any reasonable developer would expect.\n\
1: Several functions have hidden side effects or surprising invariants that \
are not documented.\n\
2: Most behavior is predictable; a few edge cases or implicit assumptions \
exist.\n\
3: Names and signatures accurately describe behavior; surprises are rare and \
well-documented.\n\
4: The code is entirely predictable from its public API; a developer's mental \
model will never be wrong.",
    },
    MaintDimension {
        name: DIM_EVASION,
        label: "evasion",
        description: "Evasion: Does the implementation respect its own abstraction \
             boundaries, so that each layer can be understood independently? \
             Or does it reach behind interfaces, punch through layers, and \
             otherwise break separations of concern?",
        anchors: "\
0: Abstraction boundaries are routinely violated; internal details of other \
modules are accessed directly throughout.\n\
1: Several violations exist where code reaches behind interfaces or bypasses \
intended access patterns.\n\
2: Most boundaries are respected; a few pragmatic violations exist but are \
contained.\n\
3: Abstraction boundaries are consistently honored; layers are cleanly \
separated.\n\
4: Every abstraction boundary is airtight; each layer can be understood, \
tested, and modified independently.",
    },
];

// ---------------------------------------------------------------------------
// Explain (single source of truth for --explain)
// ---------------------------------------------------------------------------

/// Render the maintainability dimension registry as human-readable text for
/// `gremlins-drag --explain`. Reuses the same registry the scoring prompt
/// uses, so the explanation cannot drift from what is actually scored.
pub(crate) fn explain_dimensions() -> String {
    let mut out = String::new();
    for (i, d) in DIMENSIONS.iter().enumerate() {
        out.push_str(&format!("{}. {}\n", i + 1, d.description));
        out.push_str("   Anchors:\n");
        for line in d.anchors.lines() {
            out.push_str(&format!("   {line}\n"));
        }
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// Maintainability scores (0-4 each) for ten dimensions.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub(crate) struct PackageMaintainability {
    pub decomposition: u8,
    pub interface_design: u8,
    pub consistency: u8,
    pub change_isolation: u8,
    #[serde(default)]
    pub duplication: u8,
    #[serde(default)]
    pub locality: u8,
    #[serde(default)]
    pub abstraction_clarity: u8,
    #[serde(default)]
    pub conceptual_surface: u8,
    #[serde(default)]
    pub predictability: u8,
    #[serde(default)]
    pub evasion: u8,
}

impl PackageMaintainability {
    /// Build from a map of dimension name → score (0–4).
    /// Build from a map of dimension name → median score (f64 for precision).
    /// Rounds each dimension to u8 for the cacheable breakdown.
    fn from_dim_map(dims: &HashMap<&str, f64>) -> Self {
        PackageMaintainability {
            decomposition: dims.get(DIM_DECOMPOSITION).copied().unwrap_or(2.0).round() as u8,
            interface_design: dims
                .get(DIM_INTERFACE_DESIGN)
                .copied()
                .unwrap_or(2.0)
                .round() as u8,
            consistency: dims.get(DIM_CONSISTENCY).copied().unwrap_or(2.0).round() as u8,
            change_isolation: dims
                .get(DIM_CHANGE_ISOLATION)
                .copied()
                .unwrap_or(2.0)
                .round() as u8,
            duplication: dims.get(DIM_DUPLICATION).copied().unwrap_or(2.0).round() as u8,
            locality: dims.get(DIM_LOCALITY).copied().unwrap_or(2.0).round() as u8,
            abstraction_clarity: dims
                .get(DIM_ABSTRACTION_CLARITY)
                .copied()
                .unwrap_or(2.0)
                .round() as u8,
            conceptual_surface: dims
                .get(DIM_CONCEPTUAL_SURFACE)
                .copied()
                .unwrap_or(2.0)
                .round() as u8,
            predictability: dims.get(DIM_PREDICTABILITY).copied().unwrap_or(2.0).round() as u8,
            evasion: dims.get(DIM_EVASION).copied().unwrap_or(2.0).round() as u8,
        }
    }
}

/// Compute the maintainability score (0–1) directly from unrounded f64
/// dimension medians, avoiding the upward bias of per-dimension rounding.
fn compute_maint_score_from_map(dims: &HashMap<&str, f64>) -> f64 {
    let sum = dims.get(DIM_DECOMPOSITION).copied().unwrap_or(2.0)
        + dims.get(DIM_INTERFACE_DESIGN).copied().unwrap_or(2.0)
        + dims.get(DIM_CONSISTENCY).copied().unwrap_or(2.0)
        + dims.get(DIM_CHANGE_ISOLATION).copied().unwrap_or(2.0)
        + dims.get(DIM_DUPLICATION).copied().unwrap_or(2.0)
        + dims.get(DIM_LOCALITY).copied().unwrap_or(2.0)
        + dims.get(DIM_ABSTRACTION_CLARITY).copied().unwrap_or(2.0)
        + dims.get(DIM_CONCEPTUAL_SURFACE).copied().unwrap_or(2.0)
        + dims.get(DIM_PREDICTABILITY).copied().unwrap_or(2.0)
        + dims.get(DIM_EVASION).copied().unwrap_or(2.0);
    (sum / 40.0).clamp(0.0, 1.0)
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
        let cache_key = compute_pkg_cache_key(&pkg_key, pkg, estimates)
            .map_err(|e| format!("[{pkg_name}] cache key computation failed: {e}"))?;

        if let Some(entry) = cache_map.get(&cache_key) {
            if entry.estimates == estimates && entry.model == model_spec {
                eprintln!("  [{pkg_name}] cache hit ({})", entry.key);
                cache_hits.insert(pkg_key.clone(), entry.maint_score);
                continue;
            }
        }

        // Cache miss: spawn `estimates × 10` tasks — one per dimension per estimate.
        for dim in DIMENSIONS {
            let prompt = build_dim_prompt(&pkg_name, &landscape, &pub_api_source, dim);

            for _ in 0..estimates {
                let pkg_key = pkg_key.clone();
                let pkg_name = pkg_name.clone();
                let prompt = prompt.clone();
                let model_spec = model_spec.to_string();
                let dim_name = dim.name.to_string();
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

        for dim in DIMENSIONS {
            let key = (pkg_key.clone(), dim.name.to_string());
            if let Some(scores) = estimates_map.get(&key) {
                if scores.is_empty() {
                    eprintln!(
                        "  [{pkg_name}] {label}: no estimates — defaulting to 2.0",
                        label = dim.label
                    );
                    dim_medians.insert(dim.name, 2.0);
                } else {
                    let f64_scores: Vec<f64> = scores.iter().map(|&s| s as f64).collect();
                    let med = median(&f64_scores);
                    if verbose {
                        eprintln!("  [{pkg_name}] {label}: {med:.1}", label = dim.label);
                    }
                    dim_medians.insert(dim.name, med);
                }
            } else {
                eprintln!(
                    "  [{pkg_name}] {label}: no estimates collected — defaulting to 2.0",
                    label = dim.label
                );
                dim_medians.insert(dim.name, 2.0);
            }
        }

        // Compute maint_score from the unrounded f64 medians.
        let maint_score = compute_maint_score_from_map(&dim_medians);

        eprintln!("  [{pkg_name}] maint: {maint_score:.3}");

        // Only cache and return when EVERY dimension got at least one real estimate.
        let all_dims_complete = DIMENSIONS.iter().all(|dim| {
            let key = (pkg_key.clone(), dim.name.to_string());
            estimates_map.get(&key).is_some_and(|v| v.len() >= estimates)
        });

        if !all_dims_complete {
            eprintln!(
                "  [{pkg_name}] WARNING: incomplete estimates — skipping cache write and result"
            );
            continue;
        }

        // Build breakdown with rounded values for the cache.
        let breakdown = PackageMaintainability::from_dim_map(&dim_medians);

        let cache_key = compute_pkg_cache_key(&pkg_key, pkg, estimates)
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
    dim: &MaintDimension,
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
         Target audience: an experienced developer evaluating whether the \
         package is well-organized for long-term maintenance. Judge the \
         package's structure — not correctness, not performance.\n\n\
         Score this single dimension on a 0–4 scale using these anchors:\n\n\
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
/// file contents plus the estimate count.  Any change to any source file
/// or the number of estimates invalidates the entry.
fn compute_pkg_cache_key(
    pkg_key: &str,
    pkg: &PackageInfo,
    estimates: usize,
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

    // ── from_dim_map ──

    #[test]
    fn from_dim_map_all_present() {
        let mut m: HashMap<&str, f64> = HashMap::new();
        m.insert(DIM_DECOMPOSITION, 3.0);
        m.insert(DIM_INTERFACE_DESIGN, 2.0);
        m.insert(DIM_CONSISTENCY, 4.0);
        m.insert(DIM_CHANGE_ISOLATION, 1.0);
        m.insert(DIM_DUPLICATION, 3.0);
        m.insert(DIM_LOCALITY, 2.0);
        m.insert(DIM_ABSTRACTION_CLARITY, 0.0);
        m.insert(DIM_CONCEPTUAL_SURFACE, 4.0);
        m.insert(DIM_PREDICTABILITY, 3.0);
        m.insert(DIM_EVASION, 2.0);

        let p = PackageMaintainability::from_dim_map(&m);
        assert_eq!(p.decomposition, 3);
        assert_eq!(p.interface_design, 2);
        assert_eq!(p.consistency, 4);
        assert_eq!(p.change_isolation, 1);
        assert_eq!(p.duplication, 3);
        assert_eq!(p.locality, 2);
        assert_eq!(p.abstraction_clarity, 0);
        assert_eq!(p.conceptual_surface, 4);
        assert_eq!(p.predictability, 3);
        assert_eq!(p.evasion, 2);
    }

    #[test]
    fn from_dim_map_missing_defaults_to_2() {
        let m: HashMap<&str, f64> = HashMap::new();
        let p = PackageMaintainability::from_dim_map(&m);
        assert_eq!(p.decomposition, 2);
        assert_eq!(p.interface_design, 2);
        assert_eq!(p.consistency, 2);
        assert_eq!(p.change_isolation, 2);
        assert_eq!(p.duplication, 2);
        assert_eq!(p.locality, 2);
        assert_eq!(p.abstraction_clarity, 2);
        assert_eq!(p.conceptual_surface, 2);
        assert_eq!(p.predictability, 2);
        assert_eq!(p.evasion, 2);
    }

    #[test]
    fn from_dim_map_rounds_f64() {
        let mut m: HashMap<&str, f64> = HashMap::new();
        // 2.7 rounds to 3, 2.2 rounds to 2.
        m.insert(DIM_DECOMPOSITION, 2.7);
        m.insert(DIM_INTERFACE_DESIGN, 2.2);
        let p = PackageMaintainability::from_dim_map(&m);
        assert_eq!(p.decomposition, 3);
        assert_eq!(p.interface_design, 2);
    }

    // ── compute_maint_score_from_map ──

    #[test]
    fn maint_score_from_f64_map() {
        let mut m: HashMap<&str, f64> = HashMap::new();
        // All 2.0 → score = 20/40 = 0.5
        let score = compute_maint_score_from_map(&m);
        assert!((score - 0.5).abs() < 1e-9);

        m.insert(DIM_DECOMPOSITION, 2.5);
        // With one dimension at 2.5 instead of 2.0, sum = 20.5, score = 0.5125
        let score = compute_maint_score_from_map(&m);
        assert!((score - 0.5125).abs() < 1e-9);
    }

    #[test]
    fn maint_score_preserves_fraction() {
        // When all dims are exactly the same, the median is that value
        // and maint_score should reflect it without rounding bias.
        let mut m: HashMap<&str, f64> = HashMap::new();
        m.insert(DIM_DECOMPOSITION, 1.0);
        // Only one dim filled; others default to 2.0.
        // Sum = 1.0 + 9*2.0 = 19.0, score = 19/40 = 0.475
        let score = compute_maint_score_from_map(&m);
        assert!((score - 0.475).abs() < 1e-9);
    }
}
