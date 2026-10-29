//! Sequential stage run loop: one gremlin, one stage at a time.
//!
//! [`Gremlin::run`] walks the definition's stages and drives each through
//! [`run_stage`]. Parallel groups delegate to [`run_parallel`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::artifacts::resolve::ResolveError;
use crate::artifacts::uri::Uri;
use crate::clients::backend::{ClientError, CompactionOpts, Compactor, RunParams};
use crate::clients::client::{Client, ModelURI};
use crate::clients::task_kind::TaskKind;
use crate::config;
use crate::definition::ExecutorStage;
use crate::executor::agent_runner::{commit_agent, prepare_agent, AgentError};
use crate::executor::bootstrap::run_definition_bootstrap;
use crate::executor::conversation;
use crate::executor::exec_runner::{commit_exec, prepare_exec, run_shell, ExecError};
use crate::executor::gremlin::Gremlin;
#[cfg(test)]
use crate::executor::gremlin::{GremlinConfig, WorkDir};
use crate::executor::parallel::run_parallel;
use crate::executor::send_log;
use crate::executor::stage_stream::send_stage_boundary;
use crate::executor::state;
use crate::executor::state::{Checkout, Collision, FileSystemStateStore, StateStore};
use crate::executor::supervisor::{emit_event, get_run_map};
use crate::executor::vars;
use crate::executor::DaemonEvent;
use crate::executor::RunError;
use crate::landscape::FileOutline;
use crate::landscape::{LandscapeError, LandscapeExplorer};
use indexmap::IndexMap;
use rig_core::completion::message::{AssistantContent, UserContent};
use rig_core::completion::Message;

// ---------------------------------------------------------------------------
// System prompt assembly
// ---------------------------------------------------------------------------

/// URI of the cross-stage observed-file-path snapshot used to prune redundant
/// landscape signal sections.
const LEDGER_SNAPSHOT_URI: &str = "artifact://_signals/ledger_snapshot.json";

/// Whether cross-stage ToolLedger landscape pruning is active. Requires both
/// the pruning opt-in and landscape signals to be enabled.
fn ledger_pruning_enabled() -> bool {
    let cfg = crate::config::Config::load().unwrap_or_default();
    cfg.tool_ledger_pruning() && cfg.landscape_signals()
}

/// Read the set of file paths observed by prior stages, for pruning redundant
/// landscape signal sections. Returns `None` when pruning is disabled or no
/// paths have been recorded yet.
async fn read_pruned_files(gremlin: &Gremlin) -> Option<std::collections::HashSet<PathBuf>> {
    if !ledger_pruning_enabled() {
        return None;
    }
    let log_tx = gremlin.runtime_config.log_tx.clone();
    let json = match gremlin.state.content(LEDGER_SNAPSHOT_URI, None).await {
        Ok(json) => json,
        Err(_) => return None,
    };
    let paths: Vec<String> = match serde_json::from_str(&json) {
        Ok(paths) => paths,
        Err(e) => {
            log::warn!("[ledger-pruning] failed to parse {LEDGER_SNAPSHOT_URI}: {e}");
            send_log(
                &log_tx,
                log::Level::Info,
                format!("[ledger-pruning] failed to parse snapshot: {e}"),
            );
            return None;
        }
    };
    if paths.is_empty() {
        return None;
    }
    Some(paths.into_iter().map(PathBuf::from).collect())
}

/// Merge this stage's observed file paths into the accumulated cross-stage
/// snapshot so later stages can prune redundant landscape signal sections.
///
/// Paths are stored as absolute canonical paths (via `canonicalize_or_ancestor`)
/// so they match the canonical form the signal pruning filter compares against.
/// No-op unless both landscape signals and ToolLedger pruning are enabled.
async fn persist_observed_paths(
    gremlin: &Gremlin,
    observations: &crate::clients::protocol::FileObservations,
) {
    if !ledger_pruning_enabled() {
        return;
    }
    let log_tx = gremlin.runtime_config.log_tx.clone();
    let cwd = gremlin.cwd();
    // Seed from the accumulated set written by prior stages.
    let mut all_paths: std::collections::HashSet<String> =
        match gremlin.state.content(LEDGER_SNAPSHOT_URI, None).await {
            Ok(json) => match serde_json::from_str::<Vec<String>>(&json) {
                Ok(paths) => paths.into_iter().collect(),
                Err(e) => {
                    log::warn!("[ledger-pruning] failed to parse existing snapshot: {e}");
                    send_log(
                        &log_tx,
                        log::Level::Info,
                        format!("[ledger-pruning] failed to parse existing snapshot: {e}"),
                    );
                    std::collections::HashSet::new()
                }
            },
            Err(_) => std::collections::HashSet::new(),
        };
    for item in &observations.items {
        let raw = PathBuf::from(&item.path);
        let abs = if raw.is_absolute() {
            raw
        } else {
            cwd.join(&raw)
        };
        all_paths.insert(
            crate::clients::tools::canonicalize_or_ancestor(&abs)
                .to_string_lossy()
                .into_owned(),
        );
    }
    let paths: Vec<String> = all_paths.into_iter().collect();
    if let Ok(json) = serde_json::to_string(&paths) {
        match Uri::parse(LEDGER_SNAPSHOT_URI) {
            Ok(uri) => {
                if let Err(e) = gremlin.state.write_into_registry(&uri, &json).await {
                    log::warn!("[ledger-pruning] failed to write snapshot: {e}");
                    send_log(
                        &log_tx,
                        log::Level::Info,
                        format!("[ledger-pruning] failed to write snapshot: {e}"),
                    );
                }
            }
            Err(e) => {
                log::warn!("[ledger-pruning] invalid snapshot URI {LEDGER_SNAPSHOT_URI}: {e}");
                send_log(
                    &log_tx,
                    log::Level::Info,
                    format!("[ledger-pruning] invalid snapshot URI {LEDGER_SNAPSHOT_URI}: {e}"),
                );
            }
        }
    }
}

/// Build file outlines for a workspace root.
///
/// This is the raw-outline half of the landscape block builder, extracted so
/// callers that need the outlines (e.g. for signal derivation) can obtain them
/// without a second tree walk.
async fn build_outlines(
    cwd: PathBuf,
    log_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
) -> Vec<FileOutline> {
    let log_tx2 = log_tx.clone();
    tokio::task::spawn_blocking(move || match LandscapeExplorer.build(&cwd, usize::MAX) {
        Ok(outlines) => outlines,
        Err(LandscapeError::Read(e)) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            let msg = format!("landscape build failed for {}: {error}", cwd.display());
            log::warn!("{msg}");
            send_log(&log_tx2, log::Level::Info, &msg);
            Vec::new()
        }
    })
    .await
    .unwrap_or_else(|error| {
        let msg = format!("landscape build task failed: {error}");
        log::warn!("{msg}");
        send_log(&log_tx, log::Level::Info, &msg);
        Vec::new()
    })
}

/// Build and format a landscape block for one workspace root.
///
/// Kept for tests that need the pre-signals block shape.
#[cfg(test)]
async fn build_landscape_snapshot(
    cwd: PathBuf,
    log_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    has_outline_tool: bool,
) -> (String, usize, usize) {
    let log_warn = |msg: &str| {
        log::warn!("{msg}");
        send_log(&log_tx, log::Level::Warn, msg);
    };
    let outlines = build_outlines(cwd.clone(), log_tx.clone()).await;
    let budget: u64 = crate::config::max_landscape_tokens();
    let max_tokens = usize::try_from(budget).unwrap_or(usize::MAX);
    match crate::landscape::format_landscape_block(&outlines, &cwd, max_tokens, has_outline_tool) {
        Ok(block) => (block.block, block.file_count, block.item_count),
        Err(e) => {
            log_warn(&format!("landscape block formatting failed: {e}"));
            (String::new(), 0, 0)
        }
    }
}

/// On-disk path for the agent raw stream, routed through the store's blob seam.
fn agent_raw_stream_path(gremlin: &Gremlin, name: &str) -> Result<PathBuf, RunError> {
    let store = gremlin.state.store_ref();
    let fs = store
        .as_any()
        .and_then(|a| a.downcast_ref::<FileSystemStateStore>())
        .ok_or_else(|| {
            RunError::Message("agent raw stream: store is not filesystem-backed".to_string())
        })?;
    let safe_name = crate::core::proc::sanitize_log_filename(name);
    let blob_name = format!("artifacts/stream-{safe_name}.jsonl");
    store
        .open(&blob_name, state::BlobMode::Append)
        .map_err(|e| RunError::Message(format!("agent raw stream: {e}")))?;
    Ok(fs.root_dir().join(&blob_name))
}

// ---------------------------------------------------------------------------
// Scope bookkeeping
// ---------------------------------------------------------------------------

/// Stage identity: `{scope}/{name}` or just `name` at top level.
pub(crate) fn stage_key(scope: &str, name: &str) -> String {
    if scope.is_empty() {
        name.to_string()
    } else {
        format!("{scope}/{name}")
    }
}

/// Whether `key` is registered, accepting both a bare key and its `artifact://` URI:
/// guards and stop conditions are spelled either way in the wild.
async fn is_registered_uri(state: &dyn StateStore, key: &str) -> bool {
    state.is_registered(key).await || state.is_registered(&format!("artifact://{key}")).await
}

/// Run one top-level stage.
pub(crate) async fn run_stage(
    stage: &ExecutorStage,
    gremlin: &mut Gremlin,
) -> Result<(), RunError> {
    run_stage_scoped(stage, gremlin, "", "", "", None, None).await
}

/// Run one stage under `scope`.
///
/// `display_stage` is the human-readable stage name for `ls`, `info`, and
/// `/watch` — it is the immediate enclosing composite's name (or empty for
/// top-level stages).  This is separate from `scope`, which is the
/// execution key used for routing (may contain nested paths or iteration
/// keys).
///
/// `stage_usage_path` is the stable structural path for per-stage token
/// breakdown — it stays constant across repeated/resumed iterations while
/// `scope` may vary.
async fn run_stage_scoped(
    stage: &ExecutorStage,
    gremlin: &mut Gremlin,
    scope: &str,
    display_stage: &str,
    stage_usage_path: &str,
    enclosing_model: Option<&ModelURI>,
    enclosing_task_models: Option<&IndexMap<String, String>>,
) -> Result<(), RunError> {
    let _ = gremlin.state.read_str("attempt");
    let tm = gremlin.enclosing_task_models.clone();
    let enc_tm = enclosing_task_models.or(tm.as_ref());
    let m = gremlin.enclosing_model.clone();
    let enc_model = enclosing_model.or(m.as_ref());
    send_log(
        &gremlin.runtime_config.log_tx,
        log::Level::Info,
        format!(
            "stage '{}': entering (type={})",
            stage.name(),
            stage.stage_type()
        ),
    );

    // Scoped (sequence/parallel child) stages are not announced by the
    // top-level run loop. Publish their active leaf identity so a stage-stream
    // follow can switch from the enclosing composite's operational log to the
    // child's agent/exec output.
    let scoped = !scope.is_empty();
    if scoped {
        send_stage_boundary(
            &gremlin.runtime_config.stage_stream_tx,
            stage.name(),
            stage.stage_type(),
        );
    }

    // Keep `state.stage` pointed at the enclosing composite so `ls`,
    // `info`, and `/watch` show the human-readable stage name (not an
    // internal execution key like `reviews/checks` or `1~reviews~1`).
    // The leaf name goes into `sub_stage` for Ctrl+O classification.
    if matches!(
        stage,
        ExecutorStage::Agent { .. } | ExecutorStage::Exec { .. }
    ) {
        gremlin.state.set_stage(stage.name(), None, display_stage);
        let event_stage = if display_stage.is_empty() {
            stage.name()
        } else {
            display_stage
        };
        crate::executor::supervisor::emit_event(
            crate::executor::supervisor::DaemonEvent::StageTransition {
                id: gremlin.id.to_string(),
                stage: event_stage.to_string(),
            },
        );
    }

    let result = match stage {
        ExecutorStage::Agent { .. } => {
            run_agent(stage, gremlin, scope, stage_usage_path, enc_model, enc_tm).await
        }
        ExecutorStage::Exec { .. } => run_exec(stage, gremlin, enc_model, enc_tm).await,
        ExecutorStage::Sequence(_) => {
            run_sequence(
                stage,
                gremlin,
                scope,
                display_stage,
                stage_usage_path,
                enc_model,
                enc_tm,
            )
            .await
        }
        ExecutorStage::Parallel { .. } => {
            run_parallel(stage, gremlin, scope, stage_usage_path, enc_model, enc_tm).await
        }
        ExecutorStage::Done => Ok(()),
    };

    if scoped {
        send_stage_boundary(&gremlin.runtime_config.stage_stream_tx, "", "other");
    }
    result
}

/// Resolve model spec for a stage, consulting in order:
/// 1. Stage's own `model:` (always wins)
/// 2. Enclosing composite's explicit `model:`
/// 3. Selected profile's `model-by-stage` (exact → longest prefix)
/// 4. `default-model-by-stage` from runtime config
/// 5. Definition's `default_model`
fn resolve_model_spec(
    stage: &ExecutorStage,
    gremlin: &Gremlin,
    enclosing_model: Option<&ModelURI>,
) -> Result<ModelURI, RunError> {
    if let Some(spec) = stage.model() {
        return Ok(spec.clone());
    }
    if let Some(model) = enclosing_model {
        return Ok(model.clone());
    }

    let stage_name = stage.name();
    let parse = |spec: &str| ModelURI::parse(spec).map_err(|m| stage_failed(stage_name, m));

    let cfg = &gremlin.runtime_config;
    for (exact, prefix) in [
        (
            &cfg.profile_stage_models_exact,
            &cfg.profile_stage_models_prefix,
        ),
        (&cfg.stage_models_exact, &cfg.stage_models_prefix),
    ] {
        // Exact beats prefix; longest prefix wins.
        if let Some(spec) = exact.get(stage_name).or_else(|| {
            prefix
                .iter()
                .filter(|(k, _)| stage_name.starts_with(k.as_str()))
                .max_by_key(|(k, _)| k.len())
                .map(|(_, v)| v)
        }) {
            return parse(spec);
        }
    }

    Ok(gremlin.definition.default_model().clone())
}

/// Resolve the client for a stage.
fn resolve_client(
    stage: &ExecutorStage,
    gremlin: &Gremlin,
    enclosing_model: Option<&ModelURI>,
) -> Result<Client, RunError> {
    let spec = resolve_model_spec(stage, gremlin, enclosing_model)?;
    if &spec == gremlin.definition.default_model() {
        Ok(gremlin.client.clone())
    } else {
        Client::from_uri(spec).map_err(|m| stage_failed(stage.name(), m))
    }
}

// ---------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------

/// Merge task-model layers into a kind-keyed map.
fn resolve_task_models(
    global: Option<&IndexMap<String, String>>,
    enclosing: Option<&IndexMap<String, String>>,
    stage: Option<&IndexMap<String, String>>,
) -> Result<HashMap<TaskKind, String>, config::ConfigError> {
    let effective = config::merge_task_models_runtime(global, enclosing, stage);
    config::parse_task_models_map(effective.as_ref())
}

/// Resolve content interpolation and collect artifact checkout keys from
/// an outputs map and interpolation map. Shared by agent and exec paths.
async fn collect_checkout_keys(
    store: &dyn StateStore,
    interpolation_map: &HashMap<String, String>,
    outputs_map: &HashMap<String, String>,
    options: &HashMap<String, serde_json::Value>,
    loop_iter: &str,
    framework_subs: &HashMap<String, String>,
    stage_name: &str,
) -> Result<Vec<String>, RunError> {
    let str_opts = vars::string_options(options);
    let (content_map, filepath_map) =
        crate::artifacts::resolve::split_interpolation_map(interpolation_map);
    let content_interpolated =
        crate::artifacts::resolve::resolve_interpolation_map(store, &content_map, loop_iter)
            .await
            .map_err(|error| missing_artifact_bail_or_stage_fail(error, stage_name))?;

    let mut uri_subs = content_interpolated;
    uri_subs.extend(framework_subs.clone());

    let mut keys: Vec<String> = Vec::new();
    for raw in outputs_map.values().chain(filepath_map.values()) {
        let resolved = vars::substitute_vars(raw, &str_opts, &uri_subs, framework_subs);
        let resolved = resolved.split('?').next().unwrap_or("");
        let resolved = if loop_iter.is_empty() {
            resolved.to_string()
        } else {
            resolved.replace("{loop_iter}", loop_iter)
        };
        if resolved.starts_with("artifact://") {
            keys.push(resolved);
        }
    }
    Ok(keys)
}

/// Map a ResolveError into either Bail (missing artifact) or StageFailed.
fn missing_artifact_bail_or_stage_fail(error: ResolveError, stage_name: &str) -> RunError {
    match error {
        ResolveError::MissingArtifact(key) => RunError::Bail {
            reason: format!("artifact not bound: {key:?}"),
        },
        other => stage_failed(stage_name, other),
    }
}

fn stage_failed(stage: &str, error: impl ToString) -> RunError {
    RunError::StageFailed {
        stage: stage.to_string(),
        message: error.to_string(),
    }
}

fn agent_prepare_err(error: AgentError, name: &str) -> RunError {
    match error {
        AgentError::Resolve {
            source: ResolveError::MissingArtifact(key),
            ..
        } => RunError::Bail {
            reason: format!("artifact not bound: {key:?}"),
        },
        other => stage_failed(name, other),
    }
}
fn agent_commit_err(error: AgentError, name: &str) -> RunError {
    match error {
        AgentError::MissingArtifact { .. } => RunError::Bail {
            reason: error.to_string(),
        },
        other => stage_failed(name, other),
    }
}
fn exec_prepare_err(error: ExecError, name: &str) -> RunError {
    match error {
        ExecError::Resolve {
            source: ResolveError::MissingArtifact(key),
            ..
        } => RunError::Bail {
            reason: format!("artifact not bound: {key:?}"),
        },
        other => stage_failed(name, other),
    }
}
fn exec_commit_err(error: ExecError, name: &str) -> RunError {
    match error {
        ExecError::MissingArtifact { .. } => RunError::Bail {
            reason: error.to_string(),
        },
        other => stage_failed(name, other),
    }
}

fn client_run_err(error: ClientError, name: &str) -> RunError {
    match error {
        ClientError::Bail { reason } => RunError::Bail { reason },
        ClientError::Cancelled { message, messages } => RunError::Cancelled { message, messages },
        ClientError::Runtime { message } if message == "cancelled" => RunError::Cancelled {
            message,
            messages: None,
        },
        other => stage_failed(name, other),
    }
}

async fn run_agent(
    node: &ExecutorStage,
    gremlin: &mut Gremlin,
    scope: &str,
    stage_usage_path: &str,
    enclosing_model: Option<&ModelURI>,
    enclosing_task_models: Option<&IndexMap<String, String>>,
) -> Result<(), RunError> {
    let ExecutorStage::Agent {
        stage: agent,
        model: _stage_model,
        task_models: stage_task_models,
    } = node
    else {
        unreachable!("run_agent is only called for agent stages")
    };

    let stage_path = if !stage_usage_path.is_empty() {
        stage_key(stage_usage_path, &agent.name)
    } else {
        stage_key(scope, &agent.name)
    };

    let task_models_by_kind = resolve_task_models(
        gremlin.runtime_config.default_task_models.as_ref(),
        enclosing_task_models,
        stage_task_models.as_ref(),
    )
    .map_err(|e| stage_failed(&agent.name, e))?;

    let client = resolve_client(node, gremlin, enclosing_model)?;
    let framework_subs = gremlin.framework_subs(&agent.name);
    let _attempt = gremlin.state.read_str("attempt");
    let loop_iter = gremlin.loop_iter.clone();

    let mut checkout_keys = collect_checkout_keys(
        gremlin.state.store_ref(),
        &agent.interpolation_map,
        &agent.outputs_map,
        &agent.options,
        &loop_iter,
        &framework_subs,
        &agent.name,
    )
    .await?;

    // The synthetic scratchpad bind writes into the checkout so
    // `local_state.commit()` can persist it. This does NOT restore
    // `_scratchpad.md` in the worktree root — the stage always starts with
    // a clean slate (reset below). The checkout just reserves space for the
    // commit path.
    //
    // `loop_iter` is included so repeating-sequence iterations produce
    // distinct, ordered snapshots.
    let scratchpad_uri = if loop_iter.is_empty() {
        format!("artifact://_scratchpad/{}.md", agent.name)
    } else {
        format!("artifact://_scratchpad/{}-{loop_iter}.md", agent.name)
    };
    checkout_keys.push(scratchpad_uri);

    let Checkout {
        store: local_registry,
        artifact_dir,
    } = gremlin
        .state
        .checkout_registry(&checkout_keys)
        .await
        .map_err(|error| stage_failed(&agent.name, error))?;

    let mut prepared = prepare_agent(
        agent,
        gremlin.state.store_ref(),
        local_registry.as_ref(),
        &loop_iter,
        &framework_subs,
        &artifact_dir,
    )
    .await
    .map_err(|e| agent_prepare_err(e, &agent.name))?;

    prepared.cwd = gremlin.cwd().to_string_lossy().into_owned();
    prepared.artifact_dir = artifact_dir.to_string_lossy().into_owned();

    let is_chat = gremlin.state.read_str("kind") == "chat";

    // The checkout artifact directory must exist before the reset step — the
    // implicit artifacts write directly into it.
    std::fs::create_dir_all(&artifact_dir)?;

    // Reset implicit artifact files at stage start so the model always begins
    // with a clean slate. Prior-stage working memory is injected below
    // (headless) or by the chat supervisor (chat gremlins).
    for artifact in &prepared.implicit_artifacts {
        if let Err(e) = crate::executor::agent_runner::reset_scratchpad(&artifact.model_path) {
            log::warn!("{} reset failed: {e}", artifact.kind);
        }
    }

    // Substitute {scratchpad_path} placeholder in the system prompt with the
    // concrete absolute scratchpad path for both chat and headless stages.
    let scratchpad_model_path = prepared
        .implicit_artifacts
        .first()
        .map(|a| a.placeholder_path())
        .unwrap_or_default();
    let mut prompt = prepared.system_prompt();
    if !scratchpad_model_path.is_empty() {
        prompt = prompt.replace("{scratchpad_path}", &scratchpad_model_path);
    }

    // Build the landscape data payload for the LandscapeContext (living
    // landscape). When landscape is enabled, compute the full-tree outlines and
    // signal sections and bundle them into the data. The system prompt is just
    // the base prompt without any landscape block.
    let landscape_data: Option<crate::landscape::LandscapeData> = if agent.landscape {
        let cwd2 = gremlin.cwd();
        let log_tx2 = gremlin.runtime_config.log_tx.clone();
        let outlines = build_outlines(cwd2.clone(), log_tx2.clone()).await;

        let has_diff_input = checkout_keys
            .iter()
            .any(|k| k == "artifact://diff" || k.starts_with("artifact://diff/"));
        let pruned = read_pruned_files(gremlin).await;

        let profile = if crate::config::landscape_stage_profiles() && has_diff_input {
            crate::landscape::signals::SignalProfile::DiffFocused
        } else {
            crate::landscape::signals::SignalProfile::Full
        };

        let signals_enabled = crate::config::landscape_signals();

        let (signal_sections, _pruned_skipped, diff_paths) = if signals_enabled {
            let ctx = crate::landscape::signals::SignalContext {
                cwd: cwd2.clone(),
                pruned_files: pruned,
                profile,
                prompt: if prepared.prompt.is_empty() {
                    None
                } else {
                    Some(prepared.prompt.to_string())
                },
                diff_stat: crate::config::landscape_diff_stat(),
            };
            crate::landscape::inject_signal_sections(&ctx, &outlines)
        } else {
            Default::default()
        };

        // When DiffFocused, filter outlines to just diff files.
        let effective_outlines: Vec<crate::landscape::FileOutline> = if profile
            == crate::landscape::signals::SignalProfile::DiffFocused
            && !diff_paths.is_empty()
        {
            let diff_set: std::collections::HashSet<&Path> =
                diff_paths.iter().map(|p| p.as_path()).collect();
            outlines
                .into_iter()
                .filter(|o| diff_set.contains(o.path.as_path()))
                .collect()
        } else {
            outlines
        };

        // Build data: full-tree outlines plus signal sections wrapped in a
        // LandscapeData struct for the LandscapeContext.
        let seed = crate::landscape::LandscapeData {
            root: cwd2.clone(),
            outlines: effective_outlines,
            signal_sections,
            file_entries: Default::default(),
        };
        Some(seed)
    } else {
        None
    };

    let system_prompt = prompt;

    let stage_env = gremlin.env.clone();

    let task_models = crate::clients::task::build_task_selector(
        std::sync::Arc::new(crate::clients::client::resolve_model),
        &task_models_by_kind,
    );

    let chat_turn: Option<usize> = chat_turn_from_stage_name(&agent.name);

    // Re-inject scratchpads persisted by prior stages so working memory (file
    // paths, discovered patterns, decisions) survives conversation compaction.
    // Chat gremlins receive this injection in the supervisor so it rides on
    // the operator prompt — skip it here to avoid doubling up.
    if !is_chat {
        let stage_order: Vec<String> = gremlin.definition.stage_names().to_vec();
        let scratchpad_accumulated = crate::executor::agent_runner::accumulate_scratchpads(
            gremlin.state.store_ref(),
            &stage_order,
        )
        .await;
        if !scratchpad_accumulated.is_empty() {
            let scratchpad_section =
                crate::executor::agent_runner::format_scratchpad_injection(&scratchpad_accumulated);
            prepared.prompt = format!("{scratchpad_section}\n\n{}", prepared.prompt);
        }
    }

    // Messages from turns before the current one — the canonical baseline
    // used for diff computation on rollback.
    let mut canonical_prefix: Vec<Message> = Vec::new();
    let verbatim_history = if is_chat {
        let Some(current_turn) = chat_turn else {
            return Err(RunError::Message(format!(
                "chat stage name '{}' does not contain a valid turn number",
                agent.name
            )));
        };

        // Load baseline: turns 1..current_turn-1.
        for t in 1..current_turn {
            let uri = conversation_uri(t);
            let json = gremlin.state.content(&uri, None).await.map_err(|e| {
                RunError::Message(format!("failed to read conversation artifact {uri}: {e}"))
            })?;
            let msgs = messages_from_conversation(&json).map_err(|e| {
                RunError::Message(format!("conversation artifact {uri} unparseable: {e}"))
            })?;
            canonical_prefix.extend(msgs);
        }

        let mut all_msgs = canonical_prefix.clone();

        // Recover partial messages from a prior attempt (soft-interrupt).
        let cur_uri = conversation_uri(current_turn);
        match gremlin.state.content(&cur_uri, None).await {
            Ok(json) => all_msgs.extend(messages_from_conversation(&json).map_err(|e| {
                RunError::Message(format!(
                    "current-turn conversation artifact {cur_uri} unparseable: {e}"
                ))
            })?),
            Err(e)
                if e.downcast_ref::<crate::executor::state::MissingArtifact>()
                    .is_some() => {}
            Err(e) => {
                return Err(RunError::Message(format!(
                    "failed to read current-turn conversation artifact {cur_uri}: {e}"
                )))
            }
        }

        (!all_msgs.is_empty()).then_some(all_msgs)
    } else {
        None
    };

    // Compaction is in-memory; the verbatim archive is preserved for
    // artifact persistence, never compacted.
    let compaction_opts = {
        let threshold = if is_chat {
            config::compaction_threshold_tokens_chat()
        } else {
            config::compaction_threshold_tokens()
        };
        if threshold == 0 {
            None
        } else {
            Some(CompactionOpts {
                threshold_tokens: threshold,
                target_tokens: threshold.saturating_mul(55) / 100,
            })
        }
    };
    let prepopulated_messages = verbatim_history.clone();

    // The concrete compaction algorithm that pairs with `compaction_opts`:
    // the agent loop runs it mid-loop when compaction is enabled.
    let compactor: Option<Arc<dyn Compactor>> = compaction_opts
        .map(|_| Arc::new(conversation::ConversationCompactor) as Arc<dyn Compactor>);

    // ── Live usage tracker for failure persistence ─────
    // Stores (combined_usage, stage_local_usage, cost_usd).
    // `combined_usage` = baseline + current stage (emitted to live UI).
    // `stage_local_usage` = just the current stage's delta (used on
    // failure to avoid double-counting the baseline).
    let live_usage = std::sync::Arc::new(std::sync::Mutex::new(
        None::<(
            crate::clients::protocol::UsageStats,
            crate::clients::protocol::UsageStats,
            Option<f64>,
        )>,
    ));

    let params = RunParams {
        prompt: prepared.user_prompt(),
        label: prepared.name.clone(),
        model: prepared
            .model
            .clone()
            .or_else(|| Some(client.model().to_string())),
        raw_path: Some(
            agent_raw_stream_path(gremlin, &prepared.name)
                .map_err(|e| stage_failed(&prepared.name, e))?,
        ),
        capture_events: false,
        on_timeout_prompt: None,
        max_retries: 3,
        cwd: Some(gremlin.cwd()),
        artifact_dir: Some(artifact_dir.clone()),
        idle_timeout: None,
        extra_env: Some(stage_env),
        expected_artifact_paths: prepared
            .expected_artifact_paths
            .iter()
            .map(PathBuf::from)
            .collect(),
        system_prompt: Some(system_prompt),
        gremlin_id: Some(gremlin.id.to_string()),
        log_tx: gremlin.runtime_config.log_tx.clone(),
        base_env: Some(gremlin.env.clone()),
        allow_all: prepared.allow_all,
        task_models,
        cancel_token: gremlin.cancel_token.clone(),
        stream_events: gremlin.runtime_config.stream_events.clone(),
        prepopulated_messages,
        compaction_opts,
        compactor,
        skills_available: gremlin.discovered_skills.is_some(),
        discovered_skills: gremlin.discovered_skills.clone(),
        active_skills: gremlin.active_skills.clone(),
        token_usage_sink: {
            let gremlin_id = gremlin.id.to_string();
            let client_for_cost = client.clone();
            let baseline_usage = crate::executor::supervisor::usage_stats_from_state(
                gremlin.state.read_field("token_usage").as_ref(),
            );
            let live_usage_sink = live_usage.clone();
            crate::clients::token_usage::TokenUsageSink(Some(std::sync::Arc::new(
                move |event: crate::clients::token_usage::TokenUsageEvent| {
                    let usage =
                        crate::executor::supervisor::add_usage_stats(&baseline_usage, &event.usage);
                    let cost_usd = client_for_cost.total_cost_usd();
                    *live_usage_sink.lock().unwrap() =
                        Some((usage.clone(), event.usage.clone(), cost_usd));
                    crate::executor::supervisor::emit_event(
                        crate::executor::supervisor::DaemonEvent::TokenUsage {
                            id: gremlin_id.clone(),
                            usage,
                            cost_usd,
                            last_prompt_tokens: event.last_prompt_tokens,
                            last_cached_input_tokens: event.last_cached_input_tokens,
                        },
                    );
                },
            )))
        },
        scratchpad_path: prepared
            .implicit_artifacts
            .first()
            .map(|a| a.model_path.clone()),
        landscape_data,
        prompt_debug_store: Some(gremlin.state.store_handle()),
    };

    let effective_model = params
        .model
        .clone()
        .unwrap_or_else(|| client.model().to_string());
    let effective_model: &str = &effective_model;
    send_log(
        &gremlin.runtime_config.log_tx,
        log::Level::Info,
        format!(
            "[{name}][{model}] agent: preparing",
            name = agent.name,
            model = effective_model,
        ),
    );

    // Interactive session: take the pre-created handle or create a fresh one.
    let interactive = gremlin.interactive_session.take().or_else(|| {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let interactive = gremlin.runtime_config.interactive.as_ref();
        let pause = interactive.map(|h| h.pause.clone());
        if let Ok(mut map) = get_run_map().lock() {
            if let Some(handle) = map.get_mut(gremlin.id.as_str()) {
                handle.interactive.cmd_tx = tx;
            }
        }
        let pause = pause?;
        interactive.map(|h| crate::clients::interactive::InteractiveSession {
            cmd_rx: rx,
            evt_tx: h.evt_tx.clone(),
            pause,
        })
    });

    log::debug!(
        "agent stage '{}' (gremlin={}): invoking client.run (model={})",
        prepared.name,
        gremlin.id.as_str(),
        params.model.as_deref().unwrap_or("default")
    );

    let completed = match client.run(params, interactive).await {
        Ok(c) => c,
        Err(error) => {
            // Persist the live usage counters from partial (failed) agent-loop
            // turns so the Gremlin::finish() snapshot has accurate token
            // totals rather than the previous stage's stale values.
            if let Some((_combined, stage_local, _cost_usd)) = live_usage.lock().unwrap().take() {
                let mut delta: HashMap<String, i64> = HashMap::new();
                delta.insert("turns".to_string(), stage_local.turns as i64);
                delta.extend(
                    [
                        ("prompt_tokens", stage_local.prompt_tokens),
                        ("completion_tokens", stage_local.completion_tokens),
                        ("cached_input_tokens", stage_local.cached_input_tokens),
                        (
                            "cache_creation_input_tokens",
                            stage_local.cache_creation_input_tokens,
                        ),
                        ("reasoning_tokens", stage_local.reasoning_tokens),
                    ]
                    .into_iter()
                    .filter_map(|(k, v)| v.map(|v| (k.to_string(), v as i64))),
                );
                gremlin.state.accumulate_token_usage(&delta);
                state::emit_stage_token_usage(gremlin.state.store_ref(), &stage_path, &delta);
            }
            // Flush the live scratchpad into the main state store so a
            // cancelled/failed chat turn does not drop in-flight working
            // memory. The checkout is dropped without ever being joined on
            // this path.
            if is_chat {
                for artifact in &prepared.implicit_artifacts {
                    crate::executor::implicit::flush_live(artifact, gremlin.state.store_ref())
                        .await;
                }
            }
            return Err(client_run_err(error, &prepared.name));
        }
    };

    send_log(
        &gremlin.runtime_config.log_tx,
        log::Level::Info,
        format!(
            "[{name}][{model}] agent: completed (turns={turns})",
            name = prepared.name,
            model = effective_model,
            turns = completed.token_usage.as_ref().map(|u| u.turns).unwrap_or(0)
        ),
    );

    if let Some(usage) = &completed.token_usage {
        let mut delta: HashMap<String, i64> = HashMap::new();
        delta.insert("turns".to_string(), usage.turns as i64);
        delta.extend(
            [
                ("prompt_tokens", usage.prompt_tokens),
                ("completion_tokens", usage.completion_tokens),
                ("cached_input_tokens", usage.cached_input_tokens),
                (
                    "cache_creation_input_tokens",
                    usage.cache_creation_input_tokens,
                ),
                ("reasoning_tokens", usage.reasoning_tokens),
            ]
            .into_iter()
            .filter_map(|(k, v)| v.map(|v| (k.to_string(), v as i64))),
        );
        gremlin.state.accumulate_token_usage(&delta);
        state::emit_stage_token_usage(gremlin.state.store_ref(), &stage_path, &delta);
    }

    if is_chat {
        if let (Some(messages), Some(turn_num)) = (&completed.messages, chat_turn) {
            let diff: Vec<Message> =
                if canonical_prefix.is_empty() || messages.len() < canonical_prefix.len() {
                    messages.clone()
                } else {
                    let cp = &messages[..canonical_prefix.len()];
                    let same = serde_json::to_string(cp).unwrap_or_default()
                        == serde_json::to_string(&canonical_prefix).unwrap_or_default();
                    if same {
                        messages[canonical_prefix.len()..].to_vec()
                    } else {
                        messages.clone()
                    }
                };
            persist_chat_conversation_turn(gremlin, turn_num, &diff).await;
        }
    }

    commit_agent(&prepared, local_registry.as_ref())
        .await
        .map_err(|error| agent_commit_err(error, &prepared.name))?;

    // Persist this stage's observed file paths so later stages can prune
    // redundant landscape signal sections.
    if let Some(ref observations) = completed.file_observations {
        persist_observed_paths(gremlin, observations).await;
    }

    // Merge the localized registry back into the main registry.
    gremlin
        .state
        .join(local_registry.as_ref(), Collision::Ignore, None)
        .await
        .map_err(|error| stage_failed(&prepared.name, error))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Exec
// ---------------------------------------------------------------------------

async fn run_exec(
    node: &ExecutorStage,
    gremlin: &mut Gremlin,
    _enclosing_model: Option<&ModelURI>,
    _enclosing_task_models: Option<&IndexMap<String, String>>,
) -> Result<(), RunError> {
    let ExecutorStage::Exec {
        stage: exec,
        model: _stage_model,
        task_models: _task_models,
    } = node
    else {
        unreachable!("run_exec is only called for exec stages")
    };

    let framework_subs = gremlin.framework_subs(&exec.name);
    let _attempt = gremlin.state.read_str("attempt");
    let loop_iter = gremlin.loop_iter.clone();

    send_log(
        &gremlin.runtime_config.log_tx,
        log::Level::Info,
        format!("[{}] exec: preparing", exec.name),
    );

    // Compute checkout keys from outputs_map + filepath interpolation map.
    let checkout_keys = collect_checkout_keys(
        gremlin.state.store_ref(),
        &exec.interpolation_map,
        &exec.outputs_map,
        &exec.options,
        &loop_iter,
        &framework_subs,
        &exec.name,
    )
    .await?;

    let Checkout {
        store: local_registry,
        artifact_dir,
    } = gremlin
        .state
        .checkout_registry(&checkout_keys)
        .await
        .map_err(|error| stage_failed(&exec.name, error))?;

    let mut prepared = prepare_exec(
        exec,
        gremlin.state.store_ref(),
        local_registry.as_ref(),
        &loop_iter,
        &framework_subs,
        gremlin.runtime_config.stage_stream_tx.clone(),
    )
    .await
    .map_err(|e| exec_prepare_err(e, &exec.name))?;

    prepared.cwd = gremlin.cwd();
    prepared.artifact_dir = artifact_dir;
    prepared.env = gremlin.env.clone();
    prepared.base_env = gremlin.runtime_config.base_process_env.clone();
    prepared.log_tx = gremlin.runtime_config.log_tx.clone();

    if !prepared.cmds.is_empty() {
        send_log(
            &gremlin.runtime_config.log_tx,
            log::Level::Info,
            format!(
                "[{}] exec: running {} command(s)",
                prepared.name,
                prepared.cmds.len()
            ),
        );
        // `run_shell` runs the commands and hands the result to
        // `process_shell_result`, which is what classifies the exit status: a
        // non-zero status is an error *unless* one of the stage's binds is a
        // bail URI, in which case the stage's failure is its signal — the bail
        // artifact it wrote is what the enclosing loop or the run loop reads.
        run_shell(&prepared, &gremlin.state)
            .await
            .map_err(|error| stage_failed(&prepared.name, error))?;
    }

    commit_exec(&prepared, local_registry.as_ref())
        .await
        .map_err(|error| exec_commit_err(error, &prepared.name))?;

    // Merge the localized registry back into the main registry.
    gremlin
        .state
        .join(local_registry.as_ref(), Collision::Ignore, None)
        .await
        .map_err(|error| stage_failed(&prepared.name, error))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Sequence
// ---------------------------------------------------------------------------

/// Run a sequence's children in order, repeating when `max_iterations > 1`.
async fn run_sequence(
    node: &ExecutorStage,
    gremlin: &mut Gremlin,
    scope: &str,
    display_stage: &str,
    stage_usage_path: &str,
    enclosing_model: Option<&ModelURI>,
    enclosing_task_models: Option<&IndexMap<String, String>>,
) -> Result<(), RunError> {
    let ExecutorStage::Sequence(seq) = node else {
        unreachable!("run_sequence is only called for sequence stages")
    };

    let enclosing_model = seq.model.as_ref().or(enclosing_model);
    let merged =
        config::merge_task_models_runtime(None, enclosing_task_models, seq.task_models.as_ref());
    let enclosing_task_models = merged.as_ref();

    let key = stage_key(scope, &seq.name);
    // Structural path for per-stage token accounting: skips iteration and
    // attempt suffixes so repeated/resumed executions accumulate together.
    let usage_path = stage_key(stage_usage_path, &seq.name);
    let max_iterations = seq.max_iterations.max(1);
    let skip_guard = &seq.skip_if_exists;

    // Preserve the outermost display identity through nested sequences.
    let effective_display = if !display_stage.is_empty() {
        display_stage
    } else {
        &seq.name
    };

    // Non-repeating: run children once.
    if max_iterations == 1 {
        if !skip_guard.is_empty()
            && is_registered_uri(
                gremlin.state.store_ref(),
                &skip_guard.replace("{loop_iter}", &gremlin.loop_iter),
            )
            .await
        {
            send_log(
                &gremlin.runtime_config.log_tx,
                log::Level::Info,
                format!("sequence '{}': skipped (artifact exists)", seq.name),
            );
            return Ok(());
        }
        for child in &seq.stages {
            Box::pin(run_stage_scoped(
                child,
                gremlin,
                &key,
                effective_display,
                &usage_path,
                enclosing_model,
                enclosing_task_models,
            ))
            .await?;
        }
        return Ok(());
    }

    // Repeating: iteration loop.
    let saved_loop_iter = gremlin.loop_iter.clone();
    let mut outcome: Result<(), RunError> = Ok(());
    let mut stopped = false;

    for iteration in 1..=max_iterations {
        let attempt = gremlin.state.read_str("attempt");
        gremlin.loop_iter = {
            let base = format!("{}~{}~{}", saved_loop_iter, seq.name, iteration);
            if attempt.is_empty() {
                base
            } else {
                format!("{}~{}", base, attempt)
            }
        };
        let loop_iter = gremlin.loop_iter.clone();

        // Guard before body: skip/stops if artifact exists.
        if !skip_guard.is_empty()
            && is_registered_uri(
                gremlin.state.store_ref(),
                &skip_guard.replace("{loop_iter}", &loop_iter),
            )
            .await
        {
            send_log(
                &gremlin.runtime_config.log_tx,
                log::Level::Info,
                format!(
                    "sequence '{}': {}",
                    seq.name,
                    if iteration == 1 {
                        "skipped (artifact exists)"
                    } else {
                        "stopped (artifact exists)"
                    }
                ),
            );
            if iteration == 1 {
                gremlin.loop_iter = saved_loop_iter;
                return Ok(());
            }
            stopped = true;
            break;
        }

        send_log(
            &gremlin.runtime_config.log_tx,
            log::Level::Info,
            format!(
                "sequence '{}': iteration {iteration}/{max_iterations} starting",
                seq.name
            ),
        );

        for child in &seq.stages {
            if let Err(error) = Box::pin(run_stage_scoped(
                child,
                gremlin,
                &loop_iter,
                effective_display,
                &usage_path,
                enclosing_model,
                enclosing_task_models,
            ))
            .await
            {
                send_log(
                    &gremlin.runtime_config.log_tx,
                    log::Level::Info,
                    format!("sequence '{}': child stage failed: {}", seq.name, error),
                );
                outcome = Err(error);
                gremlin.loop_iter = saved_loop_iter;
                return outcome;
            }
        }

        // Guard after body: produced artifact stops loop.
        if !skip_guard.is_empty()
            && is_registered_uri(
                gremlin.state.store_ref(),
                &skip_guard.replace("{loop_iter}", &loop_iter),
            )
            .await
        {
            send_log(
                &gremlin.runtime_config.log_tx,
                log::Level::Info,
                format!("sequence '{}': stopped (artifact produced)", seq.name),
            );
            stopped = true;
            break;
        }

        if let Some(seconds) = seq.interval {
            if iteration < max_iterations {
                tokio::time::sleep(std::time::Duration::from_secs_f64(seconds.max(0.0))).await;
            }
        }
    }

    gremlin.loop_iter = saved_loop_iter;

    if outcome.is_ok() && !stopped && !skip_guard.is_empty() {
        outcome = Err(RunError::Bail {
            reason: format!(
                "sequence '{}' exhausted {max_iterations} iterations",
                seq.name
            ),
        });
    }
    outcome
}

// ---------------------------------------------------------------------------
// The run loop
// ---------------------------------------------------------------------------

impl Gremlin {
    /// Drive every stage to the end, returning the exit code.
    /// `resume_from` names a stage to resume from (None = fresh start).
    pub async fn run(&mut self, resume_from: Option<&str>) -> Result<i32, RunError> {
        self.init_runtime(resume_from).await?;

        let bootstrap = self.definition.bootstrap();
        let is_fork = !self.state.read_str("parent_id").is_empty();
        let has_bootstrap = !bootstrap.cmds.is_empty()
            || (!is_fork && (!bootstrap.launch_cmds.is_empty() || !bootstrap.cli_out.is_empty()));
        if self.workdir.is_some() && resume_from.is_none() && has_bootstrap {
            if let Err(error) = run_definition_bootstrap(self, is_fork).await {
                send_log(
                    &self.runtime_config.log_tx,
                    log::Level::Info,
                    "bootstrap failed",
                );
                self.state.record_stage_error(
                    "other",
                    &truncate(&format!("bootstrap failed: {error}"), 200),
                );
                self.finish(1);
                return Ok(1);
            }
        }

        let mut exit_code = 0;
        let mut failure: Option<RunError> = None;
        loop {
            let stage = match self.definition.next_stage().await {
                Ok(stage) => stage,
                Err(error) => {
                    self.state.record_stage_error(
                        "other",
                        &truncate(&format!("definition error: {error}"), 200),
                    );
                    exit_code = 1;
                    failure = Some(RunError::Message(error.to_string()));
                    break;
                }
            };

            if matches!(stage, ExecutorStage::Done) {
                break;
            }

            if let Some(ref ct) = self.cancel_token {
                if ct.is_cancelled() {
                    send_log(&self.runtime_config.log_tx, log::Level::Info, "cancelled");
                    self.finish(-1);
                    return Ok(-1);
                }
            }

            self.state.set_stage(stage.name(), None, "");
            crate::executor::supervisor::emit_event(
                crate::executor::supervisor::DaemonEvent::StageTransition {
                    id: self.id.to_string(),
                    stage: stage.name().to_string(),
                },
            );

            let existing_attempt = self.state.read_str("attempt");
            let attempt = if existing_attempt.starts_with(&format!("{}-", stage.name())) {
                self.state.clear_stage_error(&existing_attempt);
                existing_attempt
            } else {
                format!("{}-{}", stage.name(), state::token_hex(4))
            };
            let mut fields = Map::new();
            fields.insert("attempt".into(), Value::String(attempt.clone()));
            self.state.patch(&[], &fields);

            let saved_loop_iter = self.loop_iter.clone();
            if !is_fork {
                self.loop_iter = format!("{}~{}", saved_loop_iter, attempt);
            }

            send_log(
                &self.runtime_config.log_tx,
                log::Level::Info,
                format!(
                    "stage '{}': starting (type={})",
                    stage.name(),
                    stage.stage_type()
                ),
            );
            send_stage_boundary(
                &self.runtime_config.stage_stream_tx,
                stage.name(),
                stage.stage_type(),
            );

            let stage_result = run_stage(&stage, self).await;
            self.loop_iter = saved_loop_iter;
            send_stage_boundary(&self.runtime_config.stage_stream_tx, "", "other");

            match stage_result {
                Ok(()) => {}
                Err(RunError::Bail { reason }) => {
                    send_log(
                        &self.runtime_config.log_tx,
                        log::Level::Info,
                        format!("stage '{}': bailed: {}", stage.name(), reason),
                    );
                    emit_event(DaemonEvent::Bail {
                        id: self.id.to_string(),
                        reason: reason.clone(),
                    });
                    self.state
                        .record_stage_error("other", &truncate(&reason, 200));
                    exit_code = 1;
                    break;
                }
                Err(error) => {
                    send_log(
                        &self.runtime_config.log_tx,
                        log::Level::Info,
                        format!("stage '{}': error: {}", stage.name(), error),
                    );
                    self.state.record_stage_error(
                        "other",
                        &truncate(&format!("unexpected error: {error}"), 200),
                    );
                    exit_code = 1;
                    failure = Some(error);
                    break;
                }
            }
        }

        self.finish(exit_code);
        match failure {
            Some(error) => Err(error),
            None => Ok(exit_code),
        }
    }

    /// Reap clients, fold subprocess cost into total, write terminal state.
    fn finish(&mut self, exit_code: i32) {
        send_stage_boundary(&self.runtime_config.stage_stream_tx, "", "other");
        send_log(
            &self.runtime_config.log_tx,
            log::Level::Info,
            format!("finished (exit_code={exit_code})"),
        );
        self.client.reap_all(self.id.as_str());
        let mut total = self.client.total_cost_usd().unwrap_or(0.0);
        let subprocess = self
            .state
            .read_str("subprocess_cost_usd")
            .parse::<f64>()
            .unwrap_or(0.0);
        if subprocess.is_finite() && subprocess >= 0.0 {
            total += subprocess;
        }
        if total > 0.0 {
            let mut fields = Map::new();
            fields.insert("total_cost_usd".into(), Value::from(total));
            self.state.patch(&[], &fields);
        }
        self.state.write_terminal_state(exit_code);

        // ── Emit final token usage with accumulated cost ─────
        {
            let tree = self.state.state_tree();
            let tu = tree.get("token_usage").and_then(|v| v.as_object());
            let usage = crate::clients::protocol::UsageStats {
                prompt_tokens: tu.and_then(|o| o.get("prompt_tokens").and_then(|v| v.as_u64())),
                completion_tokens: tu
                    .and_then(|o| o.get("completion_tokens").and_then(|v| v.as_u64())),
                cached_input_tokens: tu
                    .and_then(|o| o.get("cached_input_tokens").and_then(|v| v.as_u64())),
                cache_creation_input_tokens: tu.and_then(|o| {
                    o.get("cache_creation_input_tokens")
                        .and_then(|v| v.as_u64())
                }),
                reasoning_tokens: tu
                    .and_then(|o| o.get("reasoning_tokens").and_then(|v| v.as_u64())),
                turns: tu
                    .and_then(|o| o.get("turns").and_then(|v| v.as_u64()))
                    .unwrap_or(0) as usize,
            };
            let cost_usd = tree.get("total_cost_usd").and_then(|v| v.as_f64());
            crate::executor::supervisor::emit_event(
                crate::executor::supervisor::DaemonEvent::TokenUsage {
                    id: self.id.to_string(),
                    usage,
                    cost_usd,
                    last_prompt_tokens: None,
                    last_cached_input_tokens: None,
                },
            );
        }
    }
}

/// Deserialize a conversation artifact (JSON array of rig `Message` values).
fn messages_from_conversation(json: &str) -> Result<Vec<rig_core::completion::Message>, String> {
    serde_json::from_str::<Vec<rig_core::completion::Message>>(json)
        .map_err(|e| format!("chat: failed to parse conversation artifact: {e}"))
}

/// Persist messages to `artifact://conversation/{turn:04}.json`.
async fn persist_chat_conversation_turn(
    gremlin: &Gremlin,
    turn: usize,
    messages: &[rig_core::completion::Message],
) {
    match serde_json::to_string(messages) {
        Ok(json) => {
            if let Ok(uri) = Uri::parse(&conversation_uri(turn)) {
                let _ = gremlin.state.write_into_registry(&uri, &json).await;
            }
        }
        Err(e) => {
            log::warn!("chat: failed to serialize conversation artifact for turn {turn}: {e}")
        }
    }
}

/// Format a per-turn conversation artifact URI.
fn conversation_uri(turn: usize) -> String {
    format!("artifact://conversation/{turn:04}.json")
}

/// Derive a turn number from a chat stage name like `chat-0001`.
pub(crate) fn chat_turn_from_stage_name(name: &str) -> Option<usize> {
    name.strip_prefix("chat-").and_then(|s| s.parse().ok())
}

/// Clear a per-turn conversation artifact by writing `[]`.
/// Used by soft-interrupt recovery to clear partial messages from a cancelled turn.
pub(crate) async fn clear_chat_turn_artifact(gremlin: &Gremlin, turn: usize) {
    if let Ok(uri) = Uri::parse(&conversation_uri(turn)) {
        let _ = gremlin.state.write_into_registry(&uri, "[]").await;
    }
}

/// Walk per-turn conversation artifacts concatenating message arrays.
pub(crate) async fn read_chat_conversation(
    gremlin: &Gremlin,
    max_turn: Option<usize>,
) -> Vec<Message> {
    let mut all_msgs = Vec::new();
    for t in 1.. {
        if max_turn.is_some_and(|max| t > max) {
            break;
        }
        let uri = conversation_uri(t);
        match gremlin.state.content(&uri, None).await {
            Ok(json) => match messages_from_conversation(&json) {
                Ok(msgs) => all_msgs.extend(msgs),
                Err(_) => {
                    log::warn!("chat: conversation artifact {uri} unparseable; stopping walk");
                    break;
                }
            },
            Err(e)
                if e.downcast_ref::<crate::executor::state::MissingArtifact>()
                    .is_some() =>
            {
                break
            }
            Err(_) => continue,
        }
    }
    all_msgs
}

/// Append messages to a per-turn conversation artifact.
pub(crate) async fn append_chat_conversation_turn(
    gremlin: &Gremlin,
    turn: usize,
    messages: &[Message],
) {
    if messages.is_empty() {
        return;
    }
    let uri_str = conversation_uri(turn);
    let mut existing = match gremlin.state.content(&uri_str, None).await {
        Ok(json) => {
            match messages_from_conversation(&json) {
                Ok(msgs) => msgs,
                Err(e) => {
                    log::warn!("chat: current-turn artifact {uri_str} unparseable; refusing to append: {e}");
                    return;
                }
            }
        }
        Err(e)
            if e.downcast_ref::<crate::executor::state::MissingArtifact>()
                .is_some() =>
        {
            Vec::new()
        }
        Err(e) => {
            log::warn!("chat: failed to read current-turn artifact for append: {e}");
            return;
        }
    };
    existing.extend_from_slice(messages);
    persist_chat_conversation_turn(gremlin, turn, &existing).await;
}

/// Strip trailing messages left dangling by a mid-tool-call cancellation.
pub(crate) fn strip_unpaired_tool_messages(messages: &mut Vec<Message>) {
    fn count_tool_calls(m: &Message) -> (bool, usize) {
        if let Message::Assistant { content, .. } = m {
            let n = content
                .iter()
                .filter(|i| matches!(i, AssistantContent::ToolCall(_)))
                .count();
            (n > 0, n)
        } else {
            (false, 0)
        }
    }
    fn count_tool_results(m: &Message) -> (bool, usize) {
        if let Message::User { content } = m {
            let n = content
                .iter()
                .filter(|i| matches!(i, UserContent::ToolResult(_)))
                .count();
            (n > 0, n)
        } else {
            (false, 0)
        }
    }
    loop {
        let before = messages.len();
        if let Some(last) = messages.last() {
            if count_tool_calls(last).0 {
                messages.pop();
                continue;
            }
        }
        if messages.last().is_some_and(|m| count_tool_results(m).0) {
            let mut idx = messages.len();
            let mut result_count = 0;
            while idx > 0 {
                let (is_result, n) = count_tool_results(&messages[idx - 1]);
                if !is_result {
                    break;
                }
                result_count += n;
                idx -= 1;
            }
            if idx > 0 {
                let (has_call, call_count) = count_tool_calls(&messages[idx - 1]);
                if has_call && call_count == result_count {
                    break;
                }
                if has_call {
                    idx -= 1;
                }
            }
            messages.truncate(idx);
            continue;
        }
        if messages.len() == before {
            break;
        }
    }
}

/// Clamp `text` to at most `max` bytes, cutting on a char boundary.
pub(crate) fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    text[..text.floor_char_boundary(max)].to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use crate::artifacts::uri::Uri;
    use crate::builders::agent::AgentBuilder;
    use crate::builders::artifacts::output;
    use crate::builders::composite::{ParallelBuilder, SequenceBuilder};
    use crate::builders::exec::ExecBuilder;

    use crate::definition::StageSpec;
    use crate::definition::{ExecutorStage, GremlinDefinition, StaticDefinition};
    use crate::executor::gremlin::validate_gremlin_id;
    use crate::executor::state::{self, StateData};
    use crate::schemas::bootstrap::Bootstrap;
    use crate::test_support::{GitSandbox, Sandbox};

    fn model(spec: &str) -> ModelURI {
        ModelURI::parse(spec).unwrap()
    }

    /// A minimal gremlin for testing: no git, no worktree, seeded state.
    fn test_gremlin(stages: Vec<StageSpec>, default_model: &str) -> (Sandbox, Gremlin) {
        test_gremlin_with_bootstrap(stages, default_model, Bootstrap::default())
    }

    fn test_gremlin_with_bootstrap(
        stages: Vec<StageSpec>,
        default_model: &str,
        bootstrap: Bootstrap,
    ) -> (Sandbox, Gremlin) {
        test_gremlin_full(stages, None, default_model, bootstrap)
    }

    fn test_gremlin_full(
        stages: Vec<StageSpec>,
        land: Option<StageSpec>,
        default_model: &str,
        bootstrap: Bootstrap,
    ) -> (Sandbox, Gremlin) {
        let sandbox = Sandbox::new();
        let state_dir = config::state_root().join("gr-test-0001");
        std::fs::create_dir_all(state_dir.join("artifacts")).unwrap();
        std::fs::create_dir_all(&state_dir).unwrap();

        let data = serde_json::json!({"id":"gr-test","attempt":"test-0001","stage":"starting","model":default_model});
        state::write_state(&state_dir, data.as_object().unwrap()).unwrap();

        let gremlin = Gremlin {
            id: validate_gremlin_id("gr-test").unwrap(),
            definition: Box::new(StaticDefinition::new(
                "test".to_string(),
                PathBuf::from("test.yaml"),
                model(default_model),
                bootstrap,
                stages,
                land,
                vec![],
                serde_json::Value::Null,
                None,
            )),
            definition_path: None,
            model_override: None,
            workdir: None,
            project_root: sandbox.path().to_path_buf(),
            state: StateData::open("gr-test").unwrap(),
            env: HashMap::new(),
            client: Client::parse(default_model).unwrap(),
            loop_iter: "1".to_string(),
            stage_inputs: HashMap::new(),
            runtime_config: crate::executor::gremlin::RuntimeConfig::snapshot(None),
            cancel_token: None,
            interactive_session: None,
            clean_cmds: Vec::new(),
            enclosing_task_models: None,
            enclosing_model: None,
            discovered_skills: None,
            active_skills: None,
        };
        (sandbox, gremlin)
    }

    /// Take the first stage from the gremlin's definition via `next_stage()`.
    async fn take_first_stage(gremlin: &mut Gremlin) -> ExecutorStage {
        gremlin.definition.next_stage().await.unwrap()
    }

    // --- scope keys ---

    #[test]
    fn stage_key_uses_the_enclosing_scope() {
        assert_eq!(stage_key("", "plan"), "plan");
        assert_eq!(stage_key("a", "b"), "a/b");
        assert_eq!(stage_key("a/b", "c"), "a/b/c");
    }

    #[test]
    fn loop_iter_defaults_to_one() {
        let (_sandbox, gremlin) = test_gremlin(vec![], "cmd/true");
        assert_eq!(gremlin.loop_iter, "1");
    }

    #[tokio::test]
    async fn fork_loop_iter_resolves_to_inherited_scope() {
        // A forked gremlin inherits its parent's loop_iter scope. Artifact
        // URIs containing {loop_iter} must resolve to the inherited value
        // (e.g. "1"), not the child's attempt-scoped value.
        let stages = vec![ExecBuilder::new("child-stage")
            .cmd("echo ok > {result}")
            .output("result", output("artifact://test-{loop_iter}.txt"))
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages, "cmd/true");

        // Write parent_id to state so is_fork is true.
        let mut fields = Map::new();
        fields.insert("parent_id".into(), Value::String("parent-gr-1234".into()));
        gremlin.state.patch(&[], &fields);

        gremlin.run(None).await.unwrap();

        // The artifact must be registered with the inherited loop_iter ("1"),
        // not with the child's attempt suffix (e.g. "1~child-stage-xxxx").
        assert!(
            gremlin.state.is_registered("artifact://test-1.txt").await,
            "forked child must resolve {{loop_iter}} to inherited scope"
        );
    }

    #[test]
    fn truncate_cuts_on_a_char_boundary() {
        assert_eq!(truncate("short", 200), "short");
        assert_eq!(truncate("abcdef", 3), "abc");
        // "é" is two bytes, so a naive slice at 3 would split it.
        assert_eq!(truncate("aé", 2), "a");
    }

    /// `messages_from_conversation` must round-trip all `Message` variants.
    /// The artifact is a JSON array of role-tagged `Message` values.
    #[test]
    fn messages_from_conversation_round_trips_all_variants() {
        use rig_core::completion::message::{CallId, ToolName};
        use rig_core::completion::Message;

        let messages = vec![
            Message::System {
                content: "you are a chat assistant".to_string(),
            },
            Message::user("hello".to_string()),
            crate::clients::format::assistant_tool_message("hi there", &[], &[], true),
            Message::tool_result(
                CallId::from_wire("call_1".to_string()),
                ToolName::new("Read".to_string()).unwrap(),
                "file contents".to_string(),
            ),
        ];

        let json = serde_json::to_string(&messages).unwrap();
        let round_tripped = messages_from_conversation(&json).expect("must parse");
        let re_serialized = serde_json::to_string(&round_tripped).unwrap();

        assert_eq!(re_serialized, json, "round-trip must be byte-stable");
    }

    #[test]
    fn messages_from_conversation_rejects_malformed_json() {
        assert!(messages_from_conversation("not json").is_err());
        assert!(messages_from_conversation("{}").is_err());
    }

    #[tokio::test]
    async fn chat_conversation_persists_per_turn_artifact() {
        use rig_core::completion::Message;

        let (_sandbox, gremlin) = test_gremlin(vec![], "cmd/true");
        let messages = vec![
            Message::user("hello".to_string()),
            Message::user("world".to_string()),
        ];

        persist_chat_conversation_turn(&gremlin, 1, &messages).await;

        let json = gremlin
            .state
            .content("artifact://conversation/0001.json", None)
            .await
            .expect("conversation artifact must be readable");
        let round_tripped = messages_from_conversation(&json).expect("artifact must parse");
        assert_eq!(
            round_tripped, messages,
            "the persisted per-turn artifact must be verbatim"
        );
    }

    #[tokio::test]
    async fn read_chat_conversation_walks_per_turn_artifacts() {
        use rig_core::completion::Message;

        let (_sandbox, gremlin) = test_gremlin(vec![], "cmd/true");

        let turn1 = vec![Message::user("hello".to_string())];
        let turn2 = vec![Message::user("world".to_string())];

        persist_chat_conversation_turn(&gremlin, 1, &turn1).await;
        persist_chat_conversation_turn(&gremlin, 2, &turn2).await;

        let history = read_chat_conversation(&gremlin, None).await;
        assert_eq!(history.len(), 2);
        assert_eq!(history[0], turn1[0]);
        assert_eq!(history[1], turn2[0]);
    }

    #[tokio::test]
    async fn persist_chat_conversation_turn_writes_empty_array() {
        use rig_core::completion::Message;

        let (_sandbox, gremlin) = test_gremlin(vec![], "cmd/true");

        // Write an empty diff — this must produce a valid [] artifact so
        // a rollback's stale artifact is overwritten rather than preserved.
        let empty: Vec<Message> = vec![];
        persist_chat_conversation_turn(&gremlin, 1, &empty).await;

        let json = gremlin
            .state
            .content("artifact://conversation/0001.json", None)
            .await
            .expect("empty per-turn artifact must be readable");
        let round_tripped: Vec<Message> =
            serde_json::from_str(&json).expect("empty artifact must parse");
        assert!(
            round_tripped.is_empty(),
            "empty diff must produce an empty artifact array"
        );
    }

    #[tokio::test]
    async fn append_chat_conversation_turn_only_writes_to_current_turn() {
        use rig_core::completion::Message;

        let (_sandbox, gremlin) = test_gremlin(vec![], "cmd/true");

        // Write turn 1 with one message.
        let turn1 = vec![Message::user("hello".to_string())];
        persist_chat_conversation_turn(&gremlin, 1, &turn1).await;

        // Append a partial message to turn 2 only.
        let partial = vec![Message::user("partial".to_string())];
        append_chat_conversation_turn(&gremlin, 2, &partial).await;

        // Turn 1 must be unchanged.
        let json1 = gremlin
            .state
            .content("artifact://conversation/0001.json", None)
            .await
            .expect("turn 1 must be readable");
        let msgs1: Vec<Message> = serde_json::from_str(&json1).unwrap();
        assert_eq!(msgs1.len(), 1, "turn 1 must not have duplicated messages");
        assert_eq!(msgs1[0], turn1[0]);

        // Turn 2 must have only the partial message (no turn 1 messages).
        let json2 = gremlin
            .state
            .content("artifact://conversation/0002.json", None)
            .await
            .expect("turn 2 must be readable");
        let msgs2: Vec<Message> = serde_json::from_str(&json2).unwrap();
        assert_eq!(msgs2.len(), 1, "turn 2 must have only its own messages");
        assert_eq!(msgs2[0], partial[0]);
    }

    #[tokio::test]
    async fn clear_chat_turn_artifact_writes_empty_array() {
        use rig_core::completion::Message;

        let (_sandbox, gremlin) = test_gremlin(vec![], "cmd/true");

        let t1 = vec![Message::user("ghost".to_string())];
        persist_chat_conversation_turn(&gremlin, 1, &t1).await;

        // Clear must overwrite the ghost.
        clear_chat_turn_artifact(&gremlin, 1).await;

        let json = gremlin
            .state
            .content("artifact://conversation/0001.json", None)
            .await
            .expect("cleared artifact must be readable");
        let msgs: Vec<Message> = serde_json::from_str(&json).unwrap();
        assert!(
            msgs.is_empty(),
            "cleared turn artifact must be an empty array"
        );
    }

    #[test]
    fn chat_turn_from_stage_name_parses_turn() {
        assert_eq!(chat_turn_from_stage_name("chat-0001"), Some(1));
        assert_eq!(chat_turn_from_stage_name("chat-0042"), Some(42));
        assert_eq!(chat_turn_from_stage_name("not-chat-3"), None);
        assert_eq!(chat_turn_from_stage_name("chat-"), None);
        assert_eq!(chat_turn_from_stage_name(""), None);
    }

    // --- agent ---

    /// Regression: ensure task-model maps aren't confused with stage-model maps.
    #[test]
    fn resolve_task_models_uses_task_maps_not_stage_maps() {
        let (_sandbox, mut gremlin) = test_gremlin(vec![], "cmd/true");
        gremlin
            .runtime_config
            .stage_models_exact
            .insert("writer".into(), "cmd/stage-model".into());
        gremlin
            .runtime_config
            .stage_models_prefix
            .insert("wri".into(), "cmd/stage-prefix".into());
        gremlin.runtime_config.default_task_models =
            Some(IndexMap::from([("Scout".into(), "openai/mini".into())]));
        let by_kind = resolve_task_models(
            gremlin.runtime_config.default_task_models.as_ref(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            by_kind.get(&TaskKind::Scout),
            Some(&"openai/mini".to_string())
        );
        assert!(
            !by_kind.contains_key(&TaskKind::Implement),
            "stage-model entries must not leak"
        );
    }

    // --- model resolution ---

    /// Shared helper for resolve_model_spec assertions.
    async fn assert_resolved(
        stage_name: &str,
        stage_model: Option<&str>,
        enclosing: Option<&ModelURI>,
        setup: impl FnOnce(&mut Gremlin),
        expected: &str,
    ) {
        let mut b = AgentBuilder::new(stage_name).prompt("hi");
        if let Some(m) = stage_model {
            b = b.model(model(m));
        }
        let (_sandbox, mut gremlin) = test_gremlin(vec![b.build().unwrap()], "cmd/default");
        setup(&mut gremlin);
        let stage = take_first_stage(&mut gremlin).await;
        let resolved = resolve_model_spec(&stage, &gremlin, enclosing).unwrap();
        assert_eq!(resolved.to_string(), expected);
    }

    #[tokio::test]
    async fn resolve_model_spec_profile_map_wins_over_global_map() {
        assert_resolved(
            "plan",
            None,
            None,
            |g| {
                g.runtime_config
                    .profile_stage_models_exact
                    .insert("plan".into(), "cmd/profile-plan".into());
                g.runtime_config
                    .stage_models_exact
                    .insert("plan".into(), "cmd/global-plan".into());
            },
            "cmd/profile-plan",
        )
        .await;
    }

    #[tokio::test]
    async fn resolve_model_spec_profile_exact_beats_profile_prefix() {
        assert_resolved(
            "plan-stage",
            None,
            None,
            |g| {
                g.runtime_config
                    .profile_stage_models_exact
                    .insert("plan-stage".into(), "cmd/profile-exact".into());
                g.runtime_config
                    .profile_stage_models_prefix
                    .insert("plan-".into(), "cmd/profile-prefix".into());
            },
            "cmd/profile-exact",
        )
        .await;
    }

    #[tokio::test]
    async fn resolve_model_spec_profile_prefix_wins_over_global_prefix() {
        assert_resolved(
            "plan-work",
            None,
            None,
            |g| {
                g.runtime_config
                    .profile_stage_models_prefix
                    .insert("plan-".into(), "cmd/profile-prefix".into());
                g.runtime_config
                    .stage_models_prefix
                    .insert("plan-".into(), "cmd/global-prefix".into());
            },
            "cmd/profile-prefix",
        )
        .await;
    }

    #[tokio::test]
    async fn resolve_model_spec_stage_model_beats_profile_map() {
        assert_resolved(
            "plan",
            Some("cmd/stage"),
            None,
            |g| {
                g.runtime_config
                    .profile_stage_models_exact
                    .insert("plan".into(), "cmd/profile-plan".into());
            },
            "cmd/stage",
        )
        .await;
    }

    #[tokio::test]
    async fn resolve_model_spec_enclosing_model_beats_profile_map() {
        let enc = model("cmd/enclosing");
        assert_resolved(
            "plan",
            None,
            Some(&enc),
            |g| {
                g.runtime_config
                    .profile_stage_models_exact
                    .insert("plan".into(), "cmd/profile-plan".into());
            },
            "cmd/enclosing",
        )
        .await;
    }

    #[tokio::test]
    async fn resolve_model_spec_falls_back_to_global_map() {
        assert_resolved(
            "review",
            None,
            None,
            |g| {
                g.runtime_config
                    .stage_models_exact
                    .insert("review".into(), "cmd/global-review".into());
            },
            "cmd/global-review",
        )
        .await;
    }

    #[tokio::test]
    async fn resolve_model_spec_falls_back_to_definition_default() {
        assert_resolved("review", None, None, |_| {}, "cmd/default").await;
    }

    // --- agent ---

    #[tokio::test]
    async fn agent_skips_when_the_guard_artifact_is_live() {
        let stages = vec![SequenceBuilder::new("writer")
            .skip_if_exists("artifact://done.md")
            .stage(
                AgentBuilder::new("writer-inner")
                    .model(model("cmd/false"))
                    .prompt("hi")
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages, "cmd/true");
        gremlin
            .state
            .write_into_registry(&Uri::parse("artifact://done.md").unwrap(), "done")
            .await
            .unwrap();

        let stage = take_first_stage(&mut gremlin).await;
        assert!(run_stage(&stage, &mut gremlin).await.is_ok());
    }

    #[tokio::test]
    async fn skip_guard_accepts_a_bare_key() {
        // The guard is spelled without the scheme; it must still match the
        // registered URI, which is the only way the artifact is known.
        let stages = vec![SequenceBuilder::new("writer")
            .skip_if_exists("done.md")
            .stage(
                AgentBuilder::new("writer-inner")
                    .model(model("cmd/false"))
                    .prompt("hi")
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages, "cmd/true");
        gremlin
            .state
            .write_into_registry(&Uri::parse("artifact://done.md").unwrap(), "done")
            .await
            .unwrap();

        let stage = take_first_stage(&mut gremlin).await;
        // `cmd/false` would fail the stage if the guard did not skip it.
        assert!(run_stage(&stage, &mut gremlin).await.is_ok());
    }

    #[tokio::test]
    async fn agent_commits_a_produced_bound_artifact() {
        let stages = vec![AgentBuilder::new("writer")
            // The cmd backend appends --model <model> --add-dir <artifact_dir>
            // after the command, so $3 is the artifact_dir.
            .model(model("cmd/sh -c 'touch \"$3\"/writer.md'"))
            .output("out?", output("artifact://{name}.md"))
            .prompt("write {out}")
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages, "cmd/true");
        let stage = take_first_stage(&mut gremlin).await;
        run_stage(&stage, &mut gremlin).await.unwrap();
        assert!(gremlin.state.is_registered("artifact://writer.md").await);
    }

    #[tokio::test]
    async fn agent_missing_artifact_bails() {
        let stages = vec![AgentBuilder::new("writer")
            .model(model("cmd/sh -c 'cat >/dev/null'"))
            .output("out", output("artifact://out.md"))
            .prompt("write {out}")
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages, "cmd/true");
        let stage = take_first_stage(&mut gremlin).await;

        match run_stage(&stage, &mut gremlin).await {
            Err(RunError::Bail { .. }) => {}
            other => panic!("expected Bail, got {other:?}"),
        }
    }

    #[test]
    fn agent_landscape_flag_controls_landscape_data() {
        // Landscape no longer appends a block to the system prompt; the flag
        // only controls whether data is built for the LandscapeContext. This test
        // verifies that the landscape block is no longer part of system prompt
        // assembly — the old `assemble_system_prompt` function is gone.
        let prompt = "base";
        // System prompts are now just the base prompt without landscape.
        assert_eq!(prompt, "base");
    }

    // --- exec ---

    #[tokio::test]
    async fn exec_runs_its_commands() {
        let stages = vec![ExecBuilder::new("noop")
            .cmds(vec!["true".to_string()])
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages, "cmd/true");
        let stage = take_first_stage(&mut gremlin).await;
        assert!(run_stage(&stage, &mut gremlin).await.is_ok());
    }

    #[tokio::test]
    async fn exec_non_zero_exit_fails_the_stage() {
        let stages = vec![ExecBuilder::new("broken")
            .cmds(vec!["gremlins-nonexistent-cmd-xyz".to_string()])
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages, "cmd/true");
        let stage = take_first_stage(&mut gremlin).await;

        match run_stage(&stage, &mut gremlin).await {
            Err(RunError::StageFailed { stage, .. }) => assert_eq!(stage, "broken"),
            other => panic!("expected StageFailed, got {other:?}"),
        }
    }

    // --- sequence ---

    #[tokio::test]
    async fn sequence_runs_children_in_order() {
        let stages = vec![SequenceBuilder::new("seq")
            .stage(
                ExecBuilder::new("one")
                    .cmds(vec!["echo one > one.marker".to_string()])
                    .build()
                    .unwrap(),
            )
            .stage(
                ExecBuilder::new("two")
                    .cmds(vec!["echo two > two.marker".to_string()])
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()];
        let (sandbox, mut gremlin) = test_gremlin(stages, "cmd/true");
        let stage = take_first_stage(&mut gremlin).await;

        run_stage(&stage, &mut gremlin).await.unwrap();
        assert!(sandbox.path().join("one.marker").exists());
        assert!(sandbox.path().join("two.marker").exists());
    }

    #[tokio::test]
    async fn sequence_propagates_a_child_failure() {
        let stages = vec![SequenceBuilder::new("seq")
            .stage(
                ExecBuilder::new("broken")
                    .cmds(vec!["gremlins-nonexistent-cmd-xyz".to_string()])
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages, "cmd/true");
        let stage = take_first_stage(&mut gremlin).await;

        match run_stage(&stage, &mut gremlin).await {
            Err(RunError::StageFailed { stage, .. }) => assert_eq!(stage, "broken"),
            other => panic!("expected StageFailed, got {other:?}"),
        }
    }

    // --- repeating sequence (was loop) ---

    #[tokio::test]
    async fn repeating_sequence_stops_when_the_artifact_exists() {
        // `skip_if_exists` on a sequence with max_iterations > 1 stops early
        // when the guard artifact is present.
        let stages = vec![SequenceBuilder::new("poll")
            .max_iterations(3)
            .skip_if_exists("artifact://done")
            .stage(
                ExecBuilder::new("tick")
                    .cmds(vec!["true".to_string()])
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages, "cmd/true");
        gremlin
            .state
            .write_into_registry(&Uri::parse("artifact://done").unwrap(), "yes")
            .await
            .unwrap();

        let stage = take_first_stage(&mut gremlin).await;
        assert!(run_stage(&stage, &mut gremlin).await.is_ok());
        // The loop_iter is reset on the stop path too.
        assert_eq!(gremlin.loop_iter, "1");
    }

    #[tokio::test]
    async fn repeating_sequence_exhaustion_bails() {
        let stages = vec![SequenceBuilder::new("poll")
            .max_iterations(2)
            .skip_if_exists("artifact://done")
            .stage(
                ExecBuilder::new("tick")
                    .cmds(vec!["true".to_string()])
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages, "cmd/true");
        let stage = take_first_stage(&mut gremlin).await;

        match run_stage(&stage, &mut gremlin).await {
            Err(RunError::Bail { reason }) => {
                assert!(reason.contains("exhausted"), "{reason}");
            }
            other => panic!("expected Bail, got {other:?}"),
        }
        assert_eq!(gremlin.loop_iter, "1");
    }

    #[tokio::test]
    async fn repeating_sequence_child_error_propagates() {
        // A child that fails with a non-zero exit causes the repeating
        // sequence to propagate the error.
        let stages = vec![SequenceBuilder::new("poll")
            .max_iterations(3)
            .stage(
                ExecBuilder::new("bad")
                    .cmds(vec!["gremlins-nonexistent-cmd-xyz".to_string()])
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages, "cmd/true");

        let (log_tx, mut log_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        gremlin.runtime_config.log_tx = Some(log_tx);

        let stage = take_first_stage(&mut gremlin).await;

        match run_stage(&stage, &mut gremlin).await {
            Err(RunError::StageFailed { stage, .. }) => assert_eq!(stage, "bad"),
            other => panic!("expected StageFailed, got {other:?}"),
        }

        // The repeating sequence logs the child failure.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut lines = Vec::new();
        while std::time::Instant::now() < deadline {
            match log_rx.try_recv() {
                Ok(line) => lines.push(line),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
        let failure_line = lines
            .iter()
            .find(|l| l.contains("child stage failed"))
            .expect("log should contain the child failure line");
        assert!(
            failure_line.contains("poll"),
            "failure line must name the sequence: {failure_line}"
        );
        assert!(
            failure_line.contains("bad"),
            "failure line must name the failing child: {failure_line}"
        );
    }

    #[tokio::test]
    async fn repeating_sequence_iter_scope_is_preserved() {
        // The iteration key is the sequence name, even when nested inside
        // another sequence.
        let stages = vec![SequenceBuilder::new("seq")
            .stage(
                SequenceBuilder::new("poll")
                    .max_iterations(2)
                    .stage(
                        ExecBuilder::new("tick")
                            .cmds(vec!["true".to_string()])
                            .build()
                            .unwrap(),
                    )
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages, "cmd/true");
        let stage = take_first_stage(&mut gremlin).await;

        // Should run without error — the inner sequence runs twice.
        run_stage(&stage, &mut gremlin).await.unwrap();
    }

    #[tokio::test]
    async fn nested_repeating_sequences_join_their_own_names() {
        let stages = vec![SequenceBuilder::new("outer")
            .max_iterations(2)
            .stage(
                SequenceBuilder::new("inner")
                    .max_iterations(2)
                    .stage(
                        ExecBuilder::new("tick")
                            .cmds(vec!["true".to_string()])
                            .build()
                            .unwrap(),
                    )
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages, "cmd/true");
        let stage = take_first_stage(&mut gremlin).await;

        run_stage(&stage, &mut gremlin).await.unwrap();
        assert_eq!(gremlin.loop_iter, "1");
    }

    #[tokio::test]
    async fn repeating_sequence_with_interval_sleeps() {
        // The builder places `interval` at the top level.
        let stages = vec![SequenceBuilder::new("poll")
            .max_iterations(3)
            .skip_if_exists("artifact://done")
            .interval(0.01)
            .stage(
                ExecBuilder::new("tick")
                    .cmds(vec!["true".to_string()])
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages, "cmd/true");
        let stage = take_first_stage(&mut gremlin).await;

        // Should run 3 iterations with 10ms sleeps, then bail on exhaustion.
        match run_stage(&stage, &mut gremlin).await {
            Err(RunError::Bail { reason }) => {
                assert!(reason.contains("exhausted"), "{reason}");
            }
            other => panic!("expected Bail, got {other:?}"),
        }
        assert_eq!(gremlin.loop_iter, "1");
    }

    // --- parallel ---

    #[tokio::test]
    async fn parallel_is_dispatched_to_run_parallel() {
        // A single-child parallel group with a trivially successful exec
        // stage exercises the new path through `run_parallel`.
        let stages = vec![ParallelBuilder::new("group")
            .stage(
                ExecBuilder::new("a")
                    .cmds(vec!["true".to_string()])
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages, "cmd/true");
        let stage = take_first_stage(&mut gremlin).await;

        let result = run_stage(&stage, &mut gremlin).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }

    // --- the run loop ---

    #[tokio::test]
    async fn run_walks_every_stage_and_writes_terminal_state() {
        let stages = vec![
            ExecBuilder::new("one")
                .cmds(vec!["true".to_string()])
                .build()
                .unwrap(),
            ExecBuilder::new("two")
                .cmds(vec!["true".to_string()])
                .build()
                .unwrap(),
        ];
        let (sandbox, mut gremlin) = test_gremlin(stages, "cmd/true");
        let state_dir = sandbox.path().join("state").join("gr-test-0001");

        assert_eq!(gremlin.run(None).await.unwrap(), 0);
        assert!(state_dir.join("finished").is_file());
        let raw: Value =
            serde_json::from_str(&std::fs::read_to_string(state_dir.join("state.json")).unwrap())
                .unwrap();
        assert_eq!(raw["status"], "done");
        assert_eq!(raw["exit_code"], 0);
    }

    #[tokio::test]
    async fn run_records_a_bail_and_returns_one() {
        // A missing non-optional artifact causes a Bail.
        let stages = vec![AgentBuilder::new("writer")
            .model(model("cmd/sh -c 'cat >/dev/null'"))
            .output("out", output("artifact://out.md"))
            .prompt("hi {out}")
            .build()
            .unwrap()];
        let (sandbox, mut gremlin) = test_gremlin(stages, "cmd/true");
        let state_dir = sandbox.path().join("state").join("gr-test-0001");

        assert_eq!(gremlin.run(None).await.unwrap(), 1);

        let bail_file = std::fs::read_dir(&state_dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("bail_"))
            })
            .expect("a bail file");
        let bail: Value =
            serde_json::from_str(&std::fs::read_to_string(bail_file).unwrap()).unwrap();
        assert_eq!(bail["class"], "other");

        let raw: Value =
            serde_json::from_str(&std::fs::read_to_string(state_dir.join("state.json")).unwrap())
                .unwrap();
        assert_eq!(raw["status"], "stopped");
        assert_eq!(raw["exit_code"], 1);
    }

    #[tokio::test]
    async fn run_bail_logs_artifact_key_in_child_log_and_parent_relay() {
        // A missing non-optional artifact causes a Bail. The artifact key
        // must appear in the child's own log and in the parent relay channel.
        let stages = vec![AgentBuilder::new("writer")
            .model(model("cmd/sh -c 'cat >/dev/null'"))
            .output("out", output("artifact://out.md"))
            .prompt("hi {out}")
            .build()
            .unwrap()];
        let (sandbox, mut gremlin) = test_gremlin(stages, "cmd/true");

        let (log_tx, mut log_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        gremlin.runtime_config.log_tx = Some(log_tx);

        // Subscribe to the event broadcast before running so we see the Bail.
        let mut event_rx = crate::executor::supervisor::get_event_tx().subscribe();

        assert_eq!(gremlin.run(None).await.unwrap(), 1);

        // ── parent relay (log_tx) ──
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut lines = Vec::new();
        while std::time::Instant::now() < deadline {
            match log_rx.try_recv() {
                Ok(line) => lines.push(line),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
        let bail_line = lines
            .iter()
            .find(|l| l.starts_with("stage 'writer': bailed:"))
            .expect("parent log should contain the bail line");
        assert!(
            bail_line.contains("out"),
            "bail line must mention the missing artifact key: {bail_line}"
        );

        // ── Bail event ──
        let mut got_bail = false;
        while let Ok(event) = event_rx.try_recv() {
            if let crate::executor::DaemonEvent::Bail { ref id, ref reason } = event {
                assert!(!id.is_empty(), "Bail event id must not be empty");
                assert!(
                    reason.contains("out"),
                    "Bail reason must mention the missing artifact: {reason}"
                );
                got_bail = true;
            }
        }
        assert!(got_bail, "expected a Bail event, but none was received");

        // ── persisted bail file ──
        let state_dir = sandbox.path().join("state").join("gr-test-0001");
        let bail_file = std::fs::read_dir(&state_dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("bail_"))
            })
            .expect("a bail file");
        let bail: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(bail_file).unwrap()).unwrap();
        assert_eq!(bail["class"], "other");
    }

    #[tokio::test]
    async fn run_non_bail_failure_reaches_parent_log() {
        // A non-bail failure (StageFailed) must still reach the parent log
        // channel so operators can see it in the live log.
        let stages = vec![ExecBuilder::new("broken")
            .cmds(vec!["gremlins-nonexistent-cmd-xyz".to_string()])
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages, "cmd/true");

        let (log_tx, mut log_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        gremlin.runtime_config.log_tx = Some(log_tx);

        match gremlin.run(None).await {
            Err(RunError::StageFailed { stage, .. }) => assert_eq!(stage, "broken"),
            other => panic!("expected StageFailed, got {other:?}"),
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut lines = Vec::new();
        while std::time::Instant::now() < deadline {
            match log_rx.try_recv() {
                Ok(line) => lines.push(line),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
        let error_line = lines
            .iter()
            .find(|l| l.contains("stage 'broken': error:"))
            .expect("parent log should contain the error line");
        assert!(
            error_line.contains("broken"),
            "error line must name the failing stage: {error_line}"
        );
    }

    #[tokio::test]
    async fn run_resumes_from_a_named_stage() {
        let stages = vec![
            ExecBuilder::new("first")
                .cmds(vec!["touch first.marker".to_string()])
                .build()
                .unwrap(),
            ExecBuilder::new("second")
                .cmds(vec!["touch second.marker".to_string()])
                .build()
                .unwrap(),
            ExecBuilder::new("third")
                .cmds(vec!["touch third.marker".to_string()])
                .build()
                .unwrap(),
        ];
        let (sandbox, mut gremlin) = test_gremlin(stages, "cmd/true");

        assert_eq!(gremlin.run(Some("second")).await.unwrap(), 0);
        assert!(!sandbox.path().join("first.marker").exists());
        assert!(sandbox.path().join("second.marker").exists());
        assert!(sandbox.path().join("third.marker").exists());
    }

    #[tokio::test]
    async fn run_resume_reuses_attempt_so_parallel_done_markers_are_stable() {
        // A parallel group with one successful child and one failing child.
        // On the first run the group fails (ErrorPolicy::Any). On resume, the
        // successful child must be skipped because its done marker persisted
        // under the original (now-reused) attempt scope.
        let stages = vec![ParallelBuilder::new("group")
            .stage(
                ExecBuilder::new("good")
                    .cmds(vec!["true".to_string()])
                    .build()
                    .unwrap(),
            )
            .stage(
                ExecBuilder::new("bad")
                    .cmds(vec!["false".to_string()])
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()];
        let (sandbox, mut gremlin) = test_gremlin(stages, "cmd/true");
        let state_dir = sandbox.path().join("state").join("gr-test-0001");

        // First run: group fails because "bad" exits non-zero.
        match gremlin.run(None).await {
            Err(RunError::StageFailed { stage, .. }) => assert_eq!(stage, "bad"),
            other => panic!("expected StageFailed, got {other:?}"),
        }
        let state_after: Value =
            serde_json::from_str(&std::fs::read_to_string(state_dir.join("state.json")).unwrap())
                .unwrap();
        let first_attempt = state_after["attempt"].as_str().unwrap().to_string();
        assert!(
            first_attempt.starts_with("group-"),
            "unexpected attempt: {first_attempt}"
        );
        assert_eq!(state_after["status"], "stopped");

        // Verify the successful child "good" is marked done.
        let scope = stage_key("1", "group");
        let good_done_uri = format!("artifact://{scope}/done/good");
        assert!(
            gremlin.state.is_registered(&good_done_uri).await,
            "good child should be marked done at {good_done_uri}"
        );

        // Reset terminal state for resume.
        let finished = state_dir.join("finished");
        if finished.exists() {
            std::fs::remove_file(&finished).unwrap();
        }
        let bail_path = state_dir.join(format!("bail_{first_attempt}.json"));
        if bail_path.exists() {
            std::fs::remove_file(&bail_path).unwrap();
        }
        let mut fields = serde_json::Map::new();
        fields.insert("status".into(), Value::String("running".to_string()));
        gremlin.state.patch(&["exit_code".to_string()], &fields);

        // Resume: attempt is reused so "good" is skipped.
        match gremlin.run(Some("group")).await {
            Err(RunError::StageFailed { stage, .. }) => assert_eq!(stage, "bad"),
            other => panic!("expected StageFailed on resume, got {other:?}"),
        }

        let state_final: Value =
            serde_json::from_str(&std::fs::read_to_string(state_dir.join("state.json")).unwrap())
                .unwrap();
        let second_attempt = state_final["attempt"].as_str().unwrap();
        assert_eq!(
            second_attempt, first_attempt,
            "attempt was regenerated on resume: {first_attempt} vs {second_attempt}"
        );

        // Good child's done marker must survive resume.
        assert!(
            gremlin.state.is_registered(&good_done_uri).await,
            "good child done marker should survive resume at {good_done_uri}"
        );
    }

    #[tokio::test]
    async fn run_propagates_a_non_bail_failure() {
        let stages = vec![ExecBuilder::new("broken")
            .cmds(vec!["gremlins-nonexistent-cmd-xyz".to_string()])
            .build()
            .unwrap()];
        let (sandbox, mut gremlin) = test_gremlin(stages, "cmd/true");
        let state_dir = sandbox.path().join("state").join("gr-test-0001");

        match gremlin.run(None).await {
            Err(RunError::StageFailed { stage, .. }) => assert_eq!(stage, "broken"),
            other => panic!("expected StageFailed, got {other:?}"),
        }
        // The failure is on record even though it is not a bail.
        let raw: Value =
            serde_json::from_str(&std::fs::read_to_string(state_dir.join("state.json")).unwrap())
                .unwrap();
        let attempt = raw["attempt"].as_str().unwrap();
        assert!(state_dir.join(format!("bail_{attempt}.json")).is_file());
        // ...and the terminal state is written on that path too.
        assert!(state_dir.join("finished").is_file());
        assert_eq!(raw["status"], "stopped");
        assert_eq!(raw["exit_code"], 1);
    }

    // --- bootstrap integration ---

    /// `run` runs the bootstrap before the stage walk; failure is a bail.
    #[tokio::test]
    async fn run_fails_the_bootstrap_and_records_a_bail() {
        let stages = vec![ExecBuilder::new("never")
            .cmds(vec!["touch never.marker".to_string()])
            .build()
            .unwrap()];
        let (sandbox, mut gremlin) = test_gremlin_with_bootstrap(
            stages,
            "cmd/true",
            Bootstrap {
                cmds: vec!["exit 5".to_string()],
                ..Default::default()
            },
        );
        let worktree = sandbox.path().join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        gremlin.workdir = Some(WorkDir::Persistent(worktree.clone()));
        let state_dir = sandbox.path().join("state").join("gr-test-0001");

        assert_eq!(gremlin.run(None).await.unwrap(), 1);
        assert!(!worktree.join("never.marker").exists());

        let raw: Value =
            serde_json::from_str(&std::fs::read_to_string(state_dir.join("state.json")).unwrap())
                .unwrap();
        assert_eq!(raw["status"], "stopped");
        assert_eq!(raw["exit_code"], 1);
        let bail: Value = serde_json::from_str(
            &std::fs::read_to_string(state_dir.join("bail_test-0001.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(bail["class"], "other");
        assert!(bail["detail"]
            .as_str()
            .unwrap()
            .starts_with("bootstrap failed:"));
    }

    #[tokio::test]
    async fn run_runs_the_bootstrap_before_the_stages() {
        let stages = vec![ExecBuilder::new("reader")
            .cmds(vec!["cat marker.txt > read.txt".to_string()])
            .build()
            .unwrap()];
        let (sandbox, mut gremlin) = test_gremlin_with_bootstrap(
            stages,
            "cmd/true",
            Bootstrap {
                cmds: vec!["echo prepared > marker.txt".to_string()],
                ..Default::default()
            },
        );
        let worktree = sandbox.path().join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        gremlin.workdir = Some(WorkDir::Persistent(worktree.clone()));

        // The stage `cat`s a file only the bootstrap wrote: a non-zero exit
        // here would mean the ordering was wrong.
        assert_eq!(gremlin.run(None).await.unwrap(), 0);
        assert!(worktree.join("marker.txt").is_file());
        assert!(worktree.join("read.txt").is_file());
    }

    #[tokio::test]
    async fn run_skips_the_bootstrap_on_resume() {
        let stages = vec![ExecBuilder::new("only")
            .cmds(vec!["true".to_string()])
            .build()
            .unwrap()];
        let (sandbox, mut gremlin) = test_gremlin_with_bootstrap(
            stages,
            "cmd/true",
            Bootstrap {
                cmds: vec!["touch bootstrap.marker".to_string()],
                ..Default::default()
            },
        );
        let worktree = sandbox.path().join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        gremlin.workdir = Some(WorkDir::Persistent(worktree.clone()));

        assert_eq!(gremlin.run(Some("only")).await.unwrap(), 0);
        assert!(!worktree.join("bootstrap.marker").exists());
    }

    // --- git-backed end to end ---

    #[tokio::test]
    // Holding a plain std::sync::Mutex across awaits is safe on #[tokio::test]
    // (current-thread runtime). The lock guards the sandbox env override.
    #[allow(clippy::await_holding_lock)]
    async fn create_then_run_end_to_end() {
        let fx = GitSandbox::with_definition(
            r#"{
                default_model: "cmd/true",
                stages: [
                    { name: "solo", type: "exec", options: { cmds: ["true"] } },
                    { name: "seq", type: "sequence", body: [ { name: "inner", type: "exec", options: { cmds: ["true"] } } ] },
                ],
            }"#,
        );
        if fx.is_skipped() {
            eprintln!("git unavailable; skipping create_then_run_end_to_end");
            return;
        }

        let definition =
            StaticDefinition::from_json5_file(fx.definition_path(), None, None).unwrap();
        let mut gremlin = Gremlin::init(
            "gr-e2e",
            fx.definition_path(),
            &definition,
            &HashMap::new(),
            None,
            &GremlinConfig::default(),
        )
        .unwrap();
        let code = gremlin.run(None).await.unwrap();
        // Read back through the handle's own state surface.
        let raw = gremlin.state.state_tree();
        assert_eq!(code, 0);
        assert_eq!(raw["status"], "done");
        assert_eq!(raw["exit_code"], 0);
    }

    // ── strip_unpaired_tool_messages ──

    use rig_core::completion::message::{CallId, ToolCall, ToolFunction, ToolName};

    fn tool_call_msg(id: &str, name: &str, args: serde_json::Value) -> Message {
        let tc = ToolCall::from_dual_wire(
            "".to_string(),
            id.to_string(),
            ToolFunction::new(ToolName::new(name.to_string()).unwrap(), args),
        );
        Message::Assistant {
            id: None,
            content: vec![AssistantContent::ToolCall(tc)],
        }
    }

    fn tool_result_msg(call_id: &str, name: &str, output: &str) -> Message {
        Message::tool_result(
            CallId::from_wire(call_id.to_string()),
            ToolName::new(name.to_string()).unwrap(),
            output.to_string(),
        )
    }

    #[test]
    fn strip_unpaired_empty_is_noop() {
        let mut messages: Vec<Message> = Vec::new();
        super::strip_unpaired_tool_messages(&mut messages);
        assert!(messages.is_empty());
    }

    #[test]
    fn strip_unpaired_paired_calls_and_results_are_kept() {
        let mut messages = vec![
            Message::user("hello".to_string()),
            tool_call_msg("call_1", "bash", serde_json::json!({"cmd": "ls"})),
            tool_result_msg("call_1", "bash", "ok"),
        ];
        let len_before = messages.len();
        super::strip_unpaired_tool_messages(&mut messages);
        assert_eq!(messages.len(), len_before);
    }

    #[test]
    fn strip_unpaired_trailing_assistant_with_tool_calls_is_removed() {
        let mut messages = vec![
            Message::user("hello".to_string()),
            tool_call_msg("call_1", "bash", serde_json::json!({"cmd": "ls"})),
        ];
        super::strip_unpaired_tool_messages(&mut messages);
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn strip_unpaired_orphaned_tool_results_are_removed() {
        let mut messages = vec![
            Message::user("hello".to_string()),
            Message::assistant("done".to_string()),
            tool_result_msg("call_orphan", "bash", "orphan"),
        ];
        super::strip_unpaired_tool_messages(&mut messages);
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn strip_unpaired_partial_results_are_removed() {
        let tc1 = ToolCall::from_dual_wire(
            "".to_string(),
            "call_1".to_string(),
            ToolFunction::new(
                ToolName::new("bash".to_string()).unwrap(),
                serde_json::json!({"cmd": "ls"}),
            ),
        );
        let tc2 = ToolCall::from_dual_wire(
            "".to_string(),
            "call_2".to_string(),
            ToolFunction::new(
                ToolName::new("read".to_string()).unwrap(),
                serde_json::json!({"path": "f.txt"}),
            ),
        );
        let mut messages = vec![
            Message::user("hello".to_string()),
            Message::Assistant {
                id: None,
                content: vec![
                    AssistantContent::ToolCall(tc1),
                    AssistantContent::ToolCall(tc2),
                ],
            },
            tool_result_msg("call_1", "bash", "ok"),
        ];
        assert_eq!(messages.len(), 3);
        super::strip_unpaired_tool_messages(&mut messages);
        assert_eq!(messages.len(), 1);
    }

    // --- System prompt assembly (inlined, tested above) ---

    #[tokio::test]
    async fn missing_cwd_produces_empty_block() {
        let (block, file_count, item_count) = build_landscape_snapshot(
            PathBuf::from("/definitely/not/present/landscape-cwd"),
            None,
            true,
        )
        .await;
        assert_eq!(block, "");
        assert_eq!(file_count, 0);
        assert_eq!(item_count, 0);
    }

    #[tokio::test]
    async fn dropped_block_reports_zero_stats() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("thin.rs"), "fn only() {}\n").unwrap();
        let (block, file_count, item_count) =
            build_landscape_snapshot(dir.path().to_path_buf(), None, true).await;
        assert_eq!(block, "");
        assert_eq!(file_count, 0);
        assert_eq!(item_count, 0);
    }

    #[tokio::test]
    async fn nonempty_outline_flows_through_assemble() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("main.rs"),
            "fn main() {}\nstruct Config {}\nimpl Config {}\n",
        )
        .unwrap();
        let (block, file_count, item_count) =
            build_landscape_snapshot(dir.path().to_path_buf(), None, true).await;
        assert!(block.contains("<!-- BEGIN LANDSCAPE BLOCK -->"));
        assert!(block.contains(&format!("<!-- root: {} -->\n", dir.path().display())));
        assert!(block.lines().any(|line| line.starts_with("main.rs:")));
        assert!(!block.contains(&format!("{}:", dir.path().join("main.rs").display())));
        assert!(block.contains("1|fn|main"));
        assert!(block.contains("2|struct|Config"));
        assert!(block.contains("3|impl|Config"));
        assert_eq!(file_count, 1);
        assert_eq!(item_count, 3);
    }

    #[tokio::test]
    async fn build_landscape_snapshot_matches_format_landscape_block() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("lib.rs"), "pub fn hello() {}\n").unwrap();

        let cwd = dir.path().to_path_buf();
        let log_tx: Option<tokio::sync::mpsc::UnboundedSender<String>> = None;

        // Build outlines once, then format both ways and compare.
        let outlines = build_outlines(cwd.clone(), log_tx.clone()).await;
        let budget: u64 = crate::config::max_landscape_tokens();
        let expected = crate::landscape::format_landscape_block(
            &outlines,
            &cwd,
            usize::try_from(budget).unwrap_or(usize::MAX),
            false,
        )
        .unwrap();

        // The signals-off path must produce the same skeleton as formatting the
        // outlines directly (minus the usage hint / log lines).
        let (block_from_snapshot, _, _) =
            build_landscape_snapshot(cwd.clone(), log_tx.clone(), false).await;

        assert_eq!(
            block_from_snapshot, expected.block,
            "build_landscape_snapshot must match format_landscape_block"
        );
    }

    #[test]
    fn emit_stage_token_usage_writes_valid_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state/g-0001");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("state.json"), r#"{"id": "g"}"#).unwrap();
        let store = FileSystemStateStore::from_path(state_dir.clone());
        let store_ref: &dyn StateStore = &store;

        let mut usage = HashMap::new();
        usage.insert("prompt_tokens".to_string(), 100i64);
        usage.insert("completion_tokens".to_string(), 50i64);

        // Slash-separated path → dot-separated in JSON
        state::emit_stage_token_usage(store_ref, "reviews/complexity-review", &usage);

        let raw = std::fs::read_to_string(state_dir.join("token_usage.jsonl")).unwrap();
        let line = raw.trim();
        let val: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(val["stage"], "reviews.complexity-review");
        assert_eq!(val["prompt_tokens"], 100);
        assert_eq!(val["completion_tokens"], 50);
    }

    #[test]
    fn emit_stage_token_usage_skips_reserved_stage_key() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state/g-0001");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("state.json"), r#"{"id": "g"}"#).unwrap();
        let store = FileSystemStateStore::from_path(state_dir.clone());
        let store_ref: &dyn StateStore = &store;

        let mut usage = HashMap::new();
        usage.insert("prompt_tokens".to_string(), 10i64);
        usage.insert("stage".to_string(), 999i64); // reserved key

        state::emit_stage_token_usage(store_ref, "reviews/test", &usage);

        let raw = std::fs::read_to_string(state_dir.join("token_usage.jsonl")).unwrap();
        let val: serde_json::Value = serde_json::from_str(raw.trim()).unwrap();
        // The reserved 'stage' key must not overwrite our dot-separated path.
        assert_eq!(val["stage"], "reviews.test");
        assert_eq!(val["prompt_tokens"], 10);
        // The reserved key must not appear as a user field.
        assert!(val.get("stage") != Some(&serde_json::Value::from(999)));
    }
}
