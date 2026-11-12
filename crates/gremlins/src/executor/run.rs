//! Sequential stage run loop: one gremlin, one stage at a time.
//!
//! [`Gremlin::run`] walks the definition's stages and drives each through
//! [`run_stage`]. Parallel groups delegate to [`run_parallel`].

use std::collections::HashMap;
#[cfg(test)]
use std::path::PathBuf;

use serde_json::{Map, Value};

use crate::artifacts::resolve::ResolveError;
use crate::clients::backend::ClientError;
use crate::clients::client::{Client, ModelURI};
use crate::config;
use crate::definition::ExecutorStage;
use crate::executor::agent_runner::AgentError;
use crate::executor::bootstrap::run_definition_bootstrap;
use crate::executor::exec_runner::ExecError;
use crate::executor::gremlin::Gremlin;
#[cfg(test)]
use crate::executor::gremlin::{GremlinConfig, WorkDir};
use crate::executor::parallel::run_parallel;

use crate::executor::stage_stream::send_stage_boundary;
use crate::executor::state;
use crate::executor::state::StateStore;
use crate::executor::supervisor::emit_event;
use crate::executor::vars;
use crate::executor::EventKind;
use crate::executor::RunError;
use indexmap::IndexMap;

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
    emit_event(EventKind::LogLine {
        id: gremlin.id.to_string(),
        line: format!(
            "stage '{}': entering (type={})",
            stage.name(),
            stage.stage_type()
        ),
    });

    // Scoped (sequence/parallel child) stages are not announced by the
    // top-level run loop. Publish their active leaf identity so a stage-stream
    // follow can switch from the enclosing composite's operational log to the
    // child's agent/exec output.
    let scoped = !scope.is_empty();
    if scoped {
        send_stage_boundary(
            &gremlin.runtime_config.stage_stream_tx,
            gremlin.id.as_str(),
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
        crate::executor::supervisor::emit_event(EventKind::StageTransition {
            id: gremlin.id.to_string(),
            stage: event_stage.to_string(),
        });
    }

    let result = match stage {
        ExecutorStage::Agent { .. } => {
            crate::executor::run_agent::run_agent(
                stage,
                gremlin,
                scope,
                stage_usage_path,
                enc_model,
                enc_tm,
            )
            .await
        }
        ExecutorStage::Exec { .. } => {
            crate::executor::run_exec::run_exec(stage, gremlin, enc_model, enc_tm).await
        }
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
        send_stage_boundary(
            &gremlin.runtime_config.stage_stream_tx,
            gremlin.id.as_str(),
            "",
            "other",
        );
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
pub(crate) fn resolve_client(
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

/// Resolve content interpolation and collect artifact checkout keys from
/// an outputs map and interpolation map. Shared by agent and exec paths.
pub(crate) async fn collect_checkout_keys(
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

pub(crate) fn stage_failed(stage: &str, error: impl ToString) -> RunError {
    RunError::StageFailed {
        stage: stage.to_string(),
        message: error.to_string(),
    }
}

pub(crate) fn agent_prepare_err(error: AgentError, name: &str) -> RunError {
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
pub(crate) fn agent_commit_err(error: AgentError, name: &str) -> RunError {
    match error {
        AgentError::MissingArtifact { .. } => RunError::Bail {
            reason: error.to_string(),
        },
        other => stage_failed(name, other),
    }
}
pub(crate) fn exec_prepare_err(error: ExecError, name: &str) -> RunError {
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
pub(crate) fn exec_commit_err(error: ExecError, name: &str) -> RunError {
    match error {
        ExecError::MissingArtifact { .. } => RunError::Bail {
            reason: error.to_string(),
        },
        other => stage_failed(name, other),
    }
}

pub(crate) fn client_run_err(error: ClientError, name: &str) -> RunError {
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
            emit_event(EventKind::LogLine {
                id: gremlin.id.to_string(),
                line: format!("sequence '{}': skipped (artifact exists)", seq.name),
            });
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
            emit_event(EventKind::LogLine {
                id: gremlin.id.to_string(),
                line: format!(
                    "sequence '{}': {}",
                    seq.name,
                    if iteration == 1 {
                        "skipped (artifact exists)"
                    } else {
                        "stopped (artifact exists)"
                    }
                ),
            });
            if iteration == 1 {
                gremlin.loop_iter = saved_loop_iter;
                return Ok(());
            }
            stopped = true;
            break;
        }

        emit_event(EventKind::LogLine {
            id: gremlin.id.to_string(),
            line: format!(
                "sequence '{}': iteration {iteration}/{max_iterations} starting",
                seq.name
            ),
        });

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
                emit_event(EventKind::LogLine {
                    id: gremlin.id.to_string(),
                    line: format!("sequence '{}': child stage failed: {}", seq.name, error),
                });
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
            let msg = format!("sequence '{}': stopped (artifact produced)", seq.name);
            emit_event(EventKind::LogLine {
                id: gremlin.id.to_string(),
                line: msg,
            });
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
                emit_event(EventKind::LogLine {
                    id: self.id.to_string(),
                    line: "bootstrap failed".to_string(),
                });
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
                    emit_event(EventKind::LogLine {
                        id: self.id.to_string(),
                        line: "cancelled".to_string(),
                    });
                    self.finish(-1);
                    return Ok(-1);
                }
            }

            self.state.set_stage(stage.name(), None, "");
            crate::executor::supervisor::emit_event(EventKind::StageTransition {
                id: self.id.to_string(),
                stage: stage.name().to_string(),
            });

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

            emit_event(EventKind::LogLine {
                id: self.id.to_string(),
                line: format!(
                    "stage '{}': starting (type={})",
                    stage.name(),
                    stage.stage_type()
                ),
            });
            send_stage_boundary(
                &self.runtime_config.stage_stream_tx,
                self.id.as_str(),
                stage.name(),
                stage.stage_type(),
            );

            let stage_result = run_stage(&stage, self).await;
            self.loop_iter = saved_loop_iter;
            send_stage_boundary(
                &self.runtime_config.stage_stream_tx,
                self.id.as_str(),
                "",
                "other",
            );

            match stage_result {
                Ok(()) => {}
                Err(RunError::Bail { reason }) => {
                    emit_event(EventKind::LogLine {
                        id: self.id.to_string(),
                        line: format!("stage '{}': bailed: {}", stage.name(), reason),
                    });
                    emit_event(EventKind::Bail {
                        id: self.id.to_string(),
                        reason: reason.clone(),
                    });
                    self.state
                        .record_stage_error("other", &truncate(&reason, 200));
                    exit_code = 1;
                    break;
                }
                Err(error) => {
                    emit_event(EventKind::LogLine {
                        id: self.id.to_string(),
                        line: format!("stage '{}': error: {}", stage.name(), error),
                    });
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
        send_stage_boundary(
            &self.runtime_config.stage_stream_tx,
            self.id.as_str(),
            "",
            "other",
        );
        let msg = format!("finished (exit_code={exit_code})");
        emit_event(EventKind::LogLine {
            id: self.id.to_string(),
            line: msg,
        });
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
            crate::executor::supervisor::emit_event(EventKind::TokenUsage {
                id: self.id.to_string(),
                usage,
                cost_usd,
                last_prompt_tokens: None,
                last_cached_input_tokens: None,
            });
        }
    }
}

/// Clamp `text` to at most `max` bytes, cutting on a char boundary
/// at the **start** of the string.
pub(crate) fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    text[..text.floor_char_boundary(max)].to_string()
}

/// Clamp `text` to the **last** at most `max` bytes, cutting on a char
/// boundary at the start of the retained window.
///
/// When a long combined stdout/scrip trace is the fallback for an
/// empty stderr, the interesting text is at the tail — the failing
/// command — not at the head ("Preparing worktree…" from cmd 1/7).
pub(crate) fn truncate_tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let start = text.ceil_char_boundary(text.len() - max);
    text[start..].to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::run_agent::{
        append_chat_conversation_turn, chat_turn_from_stage_name, clear_chat_turn_artifact,
        messages_from_conversation, persist_chat_conversation_turn, persist_compacted_conversation,
        read_chat_conversation, resolve_task_models, strip_unpaired_tool_messages,
    };
    use crate::executor::state::FileSystemStateStore;
    use crate::task_kind::TaskKind;
    use rig_core::completion::message::AssistantContent;
    use rig_core::completion::Message;

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
            extra_sandbox_roots: Vec::new(),
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

    #[test]
    fn truncate_tail_keeps_the_end() {
        assert_eq!(truncate_tail("short", 200), "short");
        assert_eq!(truncate_tail("abcdef", 3), "def");
        // "é" is two bytes, so a naive tail slice could split it.
        assert_eq!(truncate_tail("éa", 2), "a");
        // The last char is the one most likely to survive.
        assert_eq!(truncate_tail("0123456789", 3), "789");
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

        persist_chat_conversation_turn(&gremlin, 1, &messages)
            .await
            .unwrap();

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

        persist_chat_conversation_turn(&gremlin, 1, &turn1)
            .await
            .unwrap();
        persist_chat_conversation_turn(&gremlin, 2, &turn2)
            .await
            .unwrap();

        let history = read_chat_conversation(&gremlin, None).await;
        assert_eq!(history.len(), 2);
        assert_eq!(history[0], turn1[0]);
        assert_eq!(history[1], turn2[0]);
    }

    #[tokio::test]
    async fn persist_compacted_conversation_clears_turns_beyond_1000() {
        use rig_core::completion::Message;

        let (_sandbox, gremlin) = test_gremlin(vec![], "cmd/true");

        // A long-running chat can accumulate arbitrary turn numbers; the
        // old bounded scan missed 1000+ and replayed duplicate history.
        persist_chat_conversation_turn(&gremlin, 1, &[Message::user("old".to_string())])
            .await
            .unwrap();
        persist_chat_conversation_turn(&gremlin, 1000, &[Message::user("late".to_string())])
            .await
            .unwrap();

        let compacted = vec![Message::user("summary".to_string())];
        persist_compacted_conversation(&gremlin, &compacted)
            .await
            .unwrap();

        // Turn 1 holds the replacement; turn 1000 is cleared to `[]`.
        let turn1: Vec<Message> = serde_json::from_str(
            &gremlin
                .state
                .content("artifact://conversation/0001.json", None)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(turn1, compacted);

        let turn1000: Vec<Message> = serde_json::from_str(
            &gremlin
                .state
                .content("artifact://conversation/1000.json", None)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(
            turn1000.is_empty(),
            "registered later turns must be cleared after compaction"
        );
    }

    #[tokio::test]
    async fn persist_chat_conversation_turn_writes_empty_array() {
        use rig_core::completion::Message;

        let (_sandbox, gremlin) = test_gremlin(vec![], "cmd/true");

        // Write an empty diff — this must produce a valid [] artifact so
        // a rollback's stale artifact is overwritten rather than preserved.
        let empty: Vec<Message> = vec![];
        persist_chat_conversation_turn(&gremlin, 1, &empty)
            .await
            .expect("persisting an empty diff must succeed");

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
        persist_chat_conversation_turn(&gremlin, 1, &turn1)
            .await
            .unwrap();

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
        persist_chat_conversation_turn(&gremlin, 1, &t1)
            .await
            .unwrap();

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

        let (_sub, mut event_rx) = crate::executor::supervisor::subscribe_all();

        let stage = take_first_stage(&mut gremlin).await;

        match run_stage(&stage, &mut gremlin).await {
            Err(RunError::StageFailed { stage, .. }) => assert_eq!(stage, "bad"),
            other => panic!("expected StageFailed, got {other:?}"),
        }

        // The repeating sequence logs the child failure via LogLine events.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut lines = Vec::new();
        while std::time::Instant::now() < deadline {
            match event_rx.try_recv() {
                Ok(dispatch) => {
                    if let EventKind::LogLine { line, .. } = dispatch.kind() {
                        lines.push(line.clone());
                    }
                }
                Err(crate::magic_bus::TryRecvError::Empty) => {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                Err(crate::magic_bus::TryRecvError::Closed)
                | Err(crate::magic_bus::TryRecvError::Lagged(_)) => break,
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

        // Subscribe to the event bus before running so we see the Bail and log lines.
        let (_event_sub, mut event_rx) = crate::executor::supervisor::subscribe_all();

        assert_eq!(gremlin.run(None).await.unwrap(), 1);

        // ── log via bus ──
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut lines = Vec::new();
        let mut got_bail = false;
        while std::time::Instant::now() < deadline {
            match event_rx.try_recv() {
                Ok(dispatch) => match dispatch.kind() {
                    EventKind::LogLine { line, .. } => {
                        lines.push(line.clone());
                    }
                    EventKind::Bail { ref id, ref reason } => {
                        assert!(!id.is_empty(), "Bail event id must not be empty");
                        assert!(
                            reason.contains("out"),
                            "Bail reason must mention the missing artifact: {reason}"
                        );
                        got_bail = true;
                    }
                    _ => {}
                },
                Err(crate::magic_bus::TryRecvError::Empty) => {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                Err(crate::magic_bus::TryRecvError::Closed)
                | Err(crate::magic_bus::TryRecvError::Lagged(_)) => break,
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

        let (_sub, mut event_rx) = crate::executor::supervisor::subscribe_all();

        match gremlin.run(None).await {
            Err(RunError::StageFailed { stage, .. }) => assert_eq!(stage, "broken"),
            other => panic!("expected StageFailed, got {other:?}"),
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut lines = Vec::new();
        while std::time::Instant::now() < deadline {
            match event_rx.try_recv() {
                Ok(dispatch) => {
                    if let EventKind::LogLine { line, .. } = dispatch.kind() {
                        lines.push(line.clone());
                    }
                }
                Err(crate::magic_bus::TryRecvError::Empty) => {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                Err(crate::magic_bus::TryRecvError::Closed)
                | Err(crate::magic_bus::TryRecvError::Lagged(_)) => break,
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
        strip_unpaired_tool_messages(&mut messages);
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
        strip_unpaired_tool_messages(&mut messages);
        assert_eq!(messages.len(), len_before);
    }

    #[test]
    fn strip_unpaired_trailing_assistant_with_tool_calls_is_removed() {
        let mut messages = vec![
            Message::user("hello".to_string()),
            tool_call_msg("call_1", "bash", serde_json::json!({"cmd": "ls"})),
        ];
        strip_unpaired_tool_messages(&mut messages);
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn strip_unpaired_orphaned_tool_results_are_removed() {
        let mut messages = vec![
            Message::user("hello".to_string()),
            Message::assistant("done".to_string()),
            tool_result_msg("call_orphan", "bash", "orphan"),
        ];
        strip_unpaired_tool_messages(&mut messages);
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
        strip_unpaired_tool_messages(&mut messages);
        assert_eq!(messages.len(), 1);
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
