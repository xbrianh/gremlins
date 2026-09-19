//! The sequential stage run loop.
//!
//! One gremlin, one stage at a time: [`Gremlin::run`] walks the pipeline's
//! stages in declaration order and drives each through [`run_stage`], while the
//! scoped dispatcher below carries the two pieces of bookkeeping the Python
//! executor kept around every stage — the artifact guard (`skip_if_exists`)
//! checked before dispatch, and the scope key (`a/b`, `loop~2`) a stage is
//! tracked under for `done_children` and bail lookups.
//!
//! This is sequencing only. Parallel groups are a later milestone and say so
//! rather than pretending to fan out.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::artifacts::registry::ArtifactRegistry;
use crate::artifacts::resolve::ResolveError;
use crate::clients::backend::RunParams;
use crate::clients::client::Client;
use crate::config;
use crate::executor::bootstrap::run_pipeline_bootstrap;
use crate::executor::gremlin::Gremlin;
use crate::executor::parallel::run_parallel;
use crate::executor::state;
use crate::executor::RunError;
use crate::stages::agent::{check_bail, commit_agent, prepare_agent, AgentError};
use crate::stages::constants::BAIL_KEY;
use crate::stages::exec::{commit_exec, prepare_exec, run_shell, ExecError};
use crate::stages::node::RunnableStage;

// ---------------------------------------------------------------------------
// Scope bookkeeping
// ---------------------------------------------------------------------------

/// The identity a stage is tracked under: its enclosing scope, or its own
/// name at the top level. Mirrors the Python path propagation (`a/b`).
pub(crate) fn stage_key(scope: &str, name: &str) -> String {
    if scope.is_empty() {
        name.to_string()
    } else {
        format!("{scope}/{name}")
    }
}

/// The `~`-joined loop iteration key (`loop~2`), or `"1"` outside any loop.
///
/// Nested loops join their frames with the same separator
/// (`outer~1~inner~2`), which is what the stage layer substitutes into
/// `{loop_iter}` URIs.
pub(crate) fn loop_iter_of(stack: &[(String, u32)]) -> String {
    if stack.is_empty() {
        return "1".to_string();
    }
    stack
        .iter()
        .map(|(name, iteration)| format!("{name}~{iteration}"))
        .collect::<Vec<_>>()
        .join("~")
}

/// `None` for an empty string, else the string itself.
fn non_empty(text: &str) -> Option<String> {
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// Read the bail reason stored at `uri`, if any.
///
/// A binding whose value is an absolute path is read from disk — an empty or
/// whitespace-only file is not a reason — while any other value *is* the
/// reason, so a `git://` or bare-string bail still reads back.
fn bail_at_uri(registry: &ArtifactRegistry, uri: &str) -> Option<String> {
    if !registry.is_registered(uri) {
        return None;
    }
    let raw = registry.data_uri(uri).ok()?;
    if !raw.starts_with('/') {
        return non_empty(raw.trim());
    }
    let path = Path::new(&raw);
    if !path.exists() {
        return None;
    }
    match std::fs::read_to_string(path) {
        Ok(text) => non_empty(text.trim()),
        Err(_) => non_empty(&raw),
    }
}

/// The bail reason recorded for `scope`: the content of `artifact://<scope>/bail`.
/// `None` when unregistered, when the bound file is gone, or when the content is
/// empty/whitespace.
pub(crate) fn bail_reason(registry: &ArtifactRegistry, scope: &str) -> Option<String> {
    bail_at_uri(registry, &format!("artifact://{scope}/bail"))
}

/// The run-wide bail marker, `artifact://bail`.
pub(crate) fn global_bail_reason(registry: &ArtifactRegistry) -> Option<String> {
    bail_at_uri(registry, BAIL_KEY)
}

/// Whether a bail is recorded for `scope` or for the run as a whole.
pub(crate) fn is_bail_set(registry: &ArtifactRegistry, scope: &str) -> bool {
    bail_reason(registry, scope).is_some() || global_bail_reason(registry).is_some()
}

/// The best reason available for a bail under `scope`: the scoped artifact's
/// content, else the run-wide marker's, else the `detail` of the state bail
/// file. `None` when no bail is recorded anywhere.
fn bail_reason_for(
    registry: &ArtifactRegistry,
    scope: &str,
    state: &state::StateData,
) -> Option<String> {
    bail_reason(registry, scope)
        .or_else(|| global_bail_reason(registry))
        .or_else(|| {
            state
                .read_bail_info()
                .and_then(|info| info.get("detail").and_then(Value::as_str).map(String::from))
        })
}

/// Remove the file behind `artifact://<scope>/bail` if one is bound, so a
/// resumed iteration does not read a previous attempt's bail as its own.
///
/// The binding itself cannot be unbound — the registry has no unbind — but a
/// missing file is already indistinguishable from no bail to [`bail_reason`].
fn clear_stale_bail(registry: &ArtifactRegistry, scope: &str) {
    let Ok(raw) = registry.data_uri(&format!("artifact://{scope}/bail")) else {
        return;
    };
    if !raw.starts_with('/') {
        return;
    }
    if let Err(error) = std::fs::remove_file(&raw) {
        log::debug!("clear_stale_bail: nothing to remove at {raw}: {error}");
    }
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Whether `key` is live, accepting both a bare key and its `artifact://` URI:
/// guards and stop conditions are spelled either way in the wild.
fn is_live_uri(registry: &ArtifactRegistry, key: &str) -> bool {
    registry.is_live(key) || registry.is_live(&format!("artifact://{key}"))
}

/// Run one top-level stage.
pub(crate) async fn run_stage(
    stage: &RunnableStage,
    gremlin: &mut Gremlin,
) -> Result<(), RunError> {
    run_stage_scoped(stage, gremlin, "").await
}

/// Run one stage, tracking it under `scope`.
///
/// The skip guard runs for every stage type before dispatch, exactly as the
/// Python `StageRunner.__call__` did: a live guard artifact means the stage has
/// already produced its output, so the whole subtree is a no-op.
async fn run_stage_scoped(
    stage: &RunnableStage,
    gremlin: &mut Gremlin,
    scope: &str,
) -> Result<(), RunError> {
    let loop_iter = loop_iter_of(&gremlin.loop_stack);
    let skip = stage.skip_if_exists();
    log::debug!(
        "stage '{}' (gremlin={}): entering (type={}, scope={scope:?}, skip_if_exists={skip:?})",
        stage.name(),
        gremlin.id.as_str(),
        stage.stage_type(),
    );
    if !skip.is_empty() {
        let resolved = skip.replace("{loop_iter}", &loop_iter);
        if is_live_uri(&gremlin.registry, &resolved) {
            log::info!("stage skipped (artifact exists): {}", stage.name());
            return Ok(());
        }
    }

    match stage {
        RunnableStage::Agent { .. } => run_agent(stage, gremlin).await,
        RunnableStage::Exec { .. } => run_exec(stage, gremlin).await,
        RunnableStage::Sequence { .. } => {
            log::debug!("stage '{}': entering sequence", stage.name());
            run_sequence(stage, gremlin, scope).await
        }
        RunnableStage::Loop { .. } => run_loop(stage, gremlin).await,
        RunnableStage::Parallel { .. } => {
            log::debug!("dispatching stage '{}' to run_parallel", stage.name());
            run_parallel(stage, gremlin).await
        }
    }
}

/// Resolve the client spec string for a stage, consulting:
/// 1. The stage's own `client:` field (always wins)
/// 2. `default-client-by-stage` from global config (exact → longest prefix)
/// 3. The pipeline's `default_client`
fn resolve_client_spec(stage: &RunnableStage, gremlin: &Gremlin) -> String {
    // 1. Explicit stage client always wins
    if let Some(spec) = stage.client() {
        return spec.0.clone();
    }

    let stage_name = stage.name();

    // 2. Consult default-client-by-stage from global config
    if let Some(cfg) = config::get_global() {
        let (exact, prefix) = cfg.default_client_by_stage();

        // Exact match
        if let Some(client_spec) = exact.get(stage_name) {
            return client_spec.clone();
        }

        // Longest prefix match
        let mut best: Option<(&str, &str)> = None;
        for (prefix_key, client_spec) in prefix {
            if stage_name.starts_with(prefix_key.as_str()) {
                match best {
                    Some((prev_key, _prev_spec)) if prefix_key.len() > prev_key.len() => {
                        best = Some((prefix_key, client_spec));
                    }
                    None => {
                        best = Some((prefix_key, client_spec));
                    }
                    _ => {}
                }
            }
        }
        if let Some((_prefix_key, client_spec)) = best {
            return client_spec.to_string();
        }
    }

    // 3. Fall back to pipeline default
    gremlin.pipeline.default_client.clone()
}

/// The client a stage runs with: resolved via [`resolve_client_spec`], then
/// parsed. When the resolved spec equals the pipeline default, the gremlin's
/// already-constructed client handle is reused to avoid building a second
/// backend for the same spec.
fn resolve_client(stage: &RunnableStage, gremlin: &Gremlin) -> Result<Client, RunError> {
    let spec = resolve_client_spec(stage, gremlin);
    if spec == gremlin.pipeline.default_client {
        Ok(gremlin.client.clone())
    } else {
        Client::parse(&spec).map_err(|message| RunError::StageFailed {
            stage: stage.name().to_string(),
            message,
        })
    }
}

// ---------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------

async fn run_agent(node: &RunnableStage, gremlin: &mut Gremlin) -> Result<(), RunError> {
    let RunnableStage::Agent { stage: agent, .. } = node else {
        unreachable!("run_agent is only called for agent stages")
    };

    let client = resolve_client(node, gremlin)?;
    let framework_subs = gremlin.framework_subs(node);
    let loop_iter = loop_iter_of(&gremlin.loop_stack);

    log::debug!(
        "agent stage '{}' (gremlin={}): preparing (client={})",
        agent.name,
        gremlin.id.as_str(),
        client.model()
    );

    let mut prepared = prepare_agent(agent, &gremlin.registry, &loop_iter, &framework_subs)
        .map_err(|error| match error {
            // An unbound interpolation input is a bail, not a crash: the run
            // cannot proceed, but nothing is broken.
            AgentError::Resolve {
                source: ResolveError::MissingArtifact(key),
                ..
            } => RunError::Bail {
                reason: format!("artifact not bound: {key:?}"),
            },
            other => RunError::StageFailed {
                stage: agent.name.clone(),
                message: other.to_string(),
            },
        })?;

    prepared.cwd = gremlin.cwd().to_string_lossy().into_owned();
    prepared.worktree = gremlin
        .worktree
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());
    prepared.artifact_dir = gremlin.artifact_dir.to_string_lossy().into_owned();
    std::fs::create_dir_all(&gremlin.artifact_dir)?;

    let params = RunParams {
        prompt: prepared.user_prompt(),
        label: prepared.name.clone(),
        model: prepared
            .model
            .clone()
            .or_else(|| Some(client.model().to_string())),
        raw_path: Some(
            gremlin
                .artifact_dir
                .join(format!("stream-{}.jsonl", prepared.name)),
        ),
        capture_events: false,
        on_timeout_prompt: None,
        max_retries: 3,
        cwd: Some(gremlin.cwd()),
        artifact_dir: Some(gremlin.artifact_dir.clone()),
        idle_timeout: None,
        extra_env: Some(gremlin.env.clone()),
        expected_artifact_paths: prepared
            .expected_artifact_paths
            .iter()
            .map(PathBuf::from)
            .collect(),
        system_prompt: Some(prepared.system_prompt()),
    };

    log::debug!(
        "agent stage '{}' (gremlin={}): invoking client.run (model={})",
        prepared.name,
        gremlin.id.as_str(),
        params.model.as_deref().unwrap_or("default")
    );

    let completed = client
        .run(params)
        .await
        .map_err(|error| RunError::StageFailed {
            stage: prepared.name.clone(),
            message: error.to_string(),
        })?;

    log::debug!(
        "agent stage '{}' (gremlin={}): client.run completed (turns={})",
        prepared.name,
        gremlin.id.as_str(),
        completed.token_usage.as_ref().map(|u| u.turns).unwrap_or(0)
    );

    if let Some(usage) = &completed.token_usage {
        let delta = HashMap::from([
            ("prompt_tokens".to_string(), usage.prompt_tokens as i64),
            (
                "completion_tokens".to_string(),
                usage.completion_tokens as i64,
            ),
            (
                "cached_input_tokens".to_string(),
                usage.cached_input_tokens as i64,
            ),
            (
                "cache_creation_input_tokens".to_string(),
                usage.cache_creation_input_tokens as i64,
            ),
            (
                "reasoning_tokens".to_string(),
                usage.reasoning_tokens as i64,
            ),
            ("turns".to_string(), usage.turns as i64),
        ]);
        gremlin.state.accumulate_token_usage(&delta);
    }

    check_bail(&completed).map_err(|error| match error {
        AgentError::Bail { reason, .. } => RunError::Bail { reason },
        other => RunError::StageFailed {
            stage: prepared.name.clone(),
            message: other.to_string(),
        },
    })?;

    commit_agent(&prepared, &gremlin.registry).map_err(|error| match error {
        // A declared output that never materialised is the agent's bail.
        AgentError::MissingArtifact { .. } => RunError::Bail {
            reason: error.to_string(),
        },
        other => RunError::StageFailed {
            stage: prepared.name.clone(),
            message: other.to_string(),
        },
    })?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Exec
// ---------------------------------------------------------------------------

async fn run_exec(node: &RunnableStage, gremlin: &mut Gremlin) -> Result<(), RunError> {
    let RunnableStage::Exec { stage: exec, .. } = node else {
        unreachable!("run_exec is only called for exec stages")
    };

    let framework_subs = gremlin.framework_subs(node);
    let loop_iter = loop_iter_of(&gremlin.loop_stack);

    log::debug!(
        "exec stage '{}' (gremlin={}): preparing",
        exec.name,
        gremlin.id.as_str()
    );

    let mut prepared =
        prepare_exec(exec, &gremlin.registry, &loop_iter, &framework_subs).map_err(|error| {
            match error {
                ExecError::Resolve {
                    source: ResolveError::MissingArtifact(key),
                    ..
                } => RunError::Bail {
                    reason: format!("artifact not bound: {key:?}"),
                },
                other => RunError::StageFailed {
                    stage: exec.name.clone(),
                    message: other.to_string(),
                },
            }
        })?;

    prepared.cwd = gremlin.cwd();
    prepared.artifact_dir = gremlin.artifact_dir.clone();
    prepared.state_dir = gremlin.state_dir.clone();
    prepared.env = gremlin.env.clone();

    if !prepared.cmds.is_empty() {
        log::debug!(
            "exec stage '{}' (gremlin={}): running {} command(s): {:?}",
            prepared.name,
            gremlin.id.as_str(),
            prepared.cmds.len(),
            prepared.cmds
        );
        // `run_shell` runs the commands and hands the result to
        // `process_shell_result`, which is what classifies the exit status: a
        // non-zero status is an error *unless* one of the stage's binds is a
        // bail URI, in which case the stage's failure is its signal — the bail
        // artifact it wrote is what the enclosing loop or the run loop reads.
        run_shell(&prepared)
            .await
            .map_err(|error| RunError::StageFailed {
                stage: prepared.name.clone(),
                message: error.to_string(),
            })?;
    }

    commit_exec(&prepared, &gremlin.registry).map_err(|error| match error {
        // An unproduced output that is not a bail URI aborts the run.
        ExecError::MissingArtifact { .. } => RunError::Bail {
            reason: error.to_string(),
        },
        other => RunError::StageFailed {
            stage: prepared.name.clone(),
            message: other.to_string(),
        },
    })?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Sequence
// ---------------------------------------------------------------------------

/// Run a sequence's children in order, skipping those already marked done.
///
/// `done_children` is keyed by the sequence's own scope, so a resumed run
/// re-enters the sequence and picks up where it stopped; the tracking is
/// cleared once the whole body has run, since the sequence is then spent.
async fn run_sequence(
    node: &RunnableStage,
    gremlin: &mut Gremlin,
    scope: &str,
) -> Result<(), RunError> {
    let RunnableStage::Sequence { attrs, body, .. } = node else {
        unreachable!("run_sequence is only called for sequence stages")
    };

    let key = stage_key(scope, &attrs.name);
    let done = gremlin.state.done_for(&key);
    for child in body {
        if done.contains(child.name()) {
            continue;
        }
        // Boxed: the dispatcher recurses through composites, and an unboxed
        // recursive `async fn` future would have no finite size.
        Box::pin(run_stage_scoped(child, gremlin, &key)).await?;
        gremlin.state.mark_done(&key, child.name());

        // A bail — scoped to the sequence or written for the run as a whole —
        // ends the body, and the sequence reports it: falling through to `Ok`
        // would let the caller read a bailed subtree as a success.
        if is_bail_set(&gremlin.registry, &key) || gremlin.state.read_bail_info().is_some() {
            let reason =
                bail_reason_for(&gremlin.registry, &key, &gremlin.state).unwrap_or_default();
            gremlin.state.clear_done(&key);
            return Err(RunError::Bail { reason });
        }
    }
    gremlin.state.clear_done(&key);
    Ok(())
}

// ---------------------------------------------------------------------------
// Loop
// ---------------------------------------------------------------------------

/// Run a loop's body until it stops, bails, or exhausts its iterations.
///
/// The iteration frame is pushed before the first iteration and popped on every
/// exit path — an early `break` must not leave the stack unbalanced, or every
/// later stage would inherit a stale `{loop_iter}`.
///
/// The frame carries the loop's own name, so nested loops read
/// `outer~1~inner~2`; the enclosing scope is bookkeeping for `done_children`,
/// not part of the iteration substitution.
async fn run_loop(node: &RunnableStage, gremlin: &mut Gremlin) -> Result<(), RunError> {
    let RunnableStage::Loop {
        attrs,
        max_iterations,
        stop_when_exists,
        interval,
        body,
        ..
    } = node
    else {
        unreachable!("run_loop is only called for loop stages")
    };
    let name = attrs.name.as_str();
    let max_iterations = *max_iterations;

    gremlin.loop_stack.push((name.to_string(), 0));

    let mut outcome: Result<(), RunError> = Ok(());
    let mut stopped = false;
    'iterations: for iteration in 1..=max_iterations {
        if let Some(top) = gremlin.loop_stack.last_mut() {
            top.1 = iteration;
        }
        let loop_iter = loop_iter_of(&gremlin.loop_stack);
        // Reset tracking left by an earlier partial iteration, then drop any
        // stale scoped bail file: the binding cannot be unbound, its file can.
        gremlin.state.clear_done(&loop_iter);
        clear_stale_bail(&gremlin.registry, &loop_iter);

        log::info!("loop {name}: iteration {iteration}/{max_iterations} starting");

        for child in body {
            // Boxed for the same reason as the sequence body above.
            if let Err(error) = Box::pin(run_stage_scoped(child, gremlin, &loop_iter)).await {
                outcome = Err(error);
                break 'iterations;
            }
            // A bail — scoped to this iteration or written for the run as a
            // whole — ends the body at once: the children after it would only
            // run against a pipeline that has already stopped.
            if is_bail_set(&gremlin.registry, &loop_iter)
                || gremlin.state.read_bail_info().is_some()
            {
                break;
            }
        }

        if is_bail_set(&gremlin.registry, &loop_iter) || gremlin.state.read_bail_info().is_some() {
            let reason =
                bail_reason_for(&gremlin.registry, &loop_iter, &gremlin.state).unwrap_or_default();
            outcome = Err(RunError::Bail { reason });
            break 'iterations;
        }

        if let Some(stop) = stop_when_exists {
            let resolved = stop.replace("{loop_iter}", &loop_iter);
            if is_live_uri(&gremlin.registry, &resolved) {
                stopped = true;
                break 'iterations;
            }
        }

        if let Some(seconds) = interval {
            tokio::time::sleep(std::time::Duration::from_secs_f64(seconds.max(0.0))).await;
        }
    }
    gremlin.loop_stack.pop();

    // Running the whole budget without meeting a stop condition *is* the
    // failure the loop reports. A zero-iteration budget never enters the range,
    // so it reports the same exhaustion instead of succeeding vacuously.
    if outcome.is_ok() && !stopped {
        outcome = Err(RunError::Bail {
            reason: format!("loop exhausted {max_iterations} iterations"),
        });
    }
    outcome
}

// ---------------------------------------------------------------------------
// The run loop
// ---------------------------------------------------------------------------

impl Gremlin {
    /// Drive every stage from `resume_from` to the end, returning the exit code.
    ///
    /// Mirrors the Python `run_pipeline` walk: a bail records `bail_<attempt>.json`
    /// and yields exit code 1, any other failure is recorded and propagated, and
    /// the terminal state is written either way so `status` and the `finished`
    /// marker always agree with what actually happened.
    pub async fn run(&mut self) -> Result<i32, RunError> {
        // Loading the pipeline, building the registry, creating the client and
        // resolving the environment are all deferred out of the constructors so
        // that a handle nobody runs is cheap. This is the one place they happen
        // — and the one place a missing pipeline becomes a hard error.
        self.init_runtime()?;

        // A first start owes its checkout a bootstrap before any stage can use
        // it. The Python guard was `worktree_dir and not resume_from and
        // _has_bootstrap`; a run without a worktree has no dev environment to
        // prepare, and a resumed run's was prepared by the attempt that made
        // the worktree.
        let bootstrap = &self.pipeline.bootstrap;
        let has_bootstrap = !bootstrap.cmds.is_empty()
            || !bootstrap.launch_cmds.is_empty()
            || !bootstrap.cli_out.is_empty();
        let first_start = self.worktree.is_some() && self.resume_from.is_none();
        if first_start && has_bootstrap {
            if let Err(error) = run_pipeline_bootstrap(self).await {
                log::error!("bootstrap failed");
                self.state.write_bail_file(
                    "other",
                    &truncate(&format!("bootstrap failed: {error}"), 200),
                );
                self.finish(1);
                return Ok(1);
            }
        }

        // Cloning the stage list keeps `self.pipeline` out of the loop's borrow,
        // which `&mut self` would otherwise hold for its whole duration.
        let stages = self.pipeline.stages.clone();
        let start = match self.resume_from.as_deref() {
            Some(name) => stages
                .iter()
                .position(|stage| stage.name() == name)
                .unwrap_or(0),
            None => 0,
        };

        let mut exit_code = 0;
        let mut failure: Option<RunError> = None;
        for stage in &stages[start..] {
            self.state.set_stage(stage.name(), None, "");

            // A fresh attempt per stage is what lets the failure that follows be
            // recorded: `write_bail_file` is a no-op without one.
            let mut fields = Map::new();
            fields.insert(
                "attempt".into(),
                Value::String(format!("{}-{}", stage.name(), state::token_hex(4))),
            );
            self.state.patch(&[], &fields);

            log::debug!(
                "gremlin {}: running top-level stage '{}' (type={})",
                self.id.as_str(),
                stage.name(),
                stage.stage_type()
            );

            match run_stage(stage, self).await {
                Ok(()) => {}
                Err(RunError::Bail { reason }) => {
                    self.state.write_bail_file("other", &truncate(&reason, 200));
                    exit_code = 1;
                    break;
                }
                Err(error) => {
                    self.state.write_bail_file(
                        "other",
                        &truncate(&format!("unexpected error: {error}"), 200),
                    );
                    exit_code = 1;
                    failure = Some(error);
                    break;
                }
            }

            if self.state.read_bail_info().is_some() {
                exit_code = 1;
                break;
            }
            // An exec stage can bail through the registry instead of state:
            // `artifact://bail` is the marker its bind writes. Stop the walk on
            // it too, and record it the way any other bail is recorded.
            if let Some(reason) = global_bail_reason(&self.registry) {
                self.state.write_bail_file("other", &truncate(&reason, 200));
                exit_code = 1;
                break;
            }
        }

        self.finish(exit_code);
        match failure {
            Some(error) => Err(error),
            None => Ok(exit_code),
        }
    }

    /// Reap the clients, fold subprocess spend into the run's total cost, and
    /// write the terminal state — the bookkeeping every exit path owes the
    /// operator, whether the run succeeded, bailed, or blew up.
    fn finish(&mut self, exit_code: i32) {
        self.client.reap_all();
        let mut total = self.client.total_cost_usd().unwrap_or(0.0);
        // Subprocess spend is only meaningful when it is a real, non-negative
        // number; a blank or junk field must not poison the total.
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
    }
}

/// Clamp `text` to at most `max` bytes, cutting on a char boundary.
///
/// The Python executor sliced the operator-facing reason (`reason[:200]`) for
/// the bail file; this keeps that budget without splitting a codepoint.
pub(crate) fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path as StdPath;

    use crate::artifacts::uri::Uri;
    use crate::executor::gremlin::validate_gremlin_id;
    use crate::executor::state::StateData;
    use crate::schemas::bootstrap::Bootstrap;
    use crate::schemas::pipeline::Pipeline;
    use crate::stages::composite::StageAttrs;
    use crate::test_support::EnvGuard;

    fn parse_stages(yaml: &str) -> Vec<RunnableStage> {
        let mut value: serde_yaml::Value = serde_yaml::from_str(yaml).expect("valid YAML");
        let list = value.as_sequence_mut().expect("a stage list");
        RunnableStage::parse_stages(list, 0).expect("valid stages")
    }

    /// A gremlin with no git, no worktree, and a seeded state directory: the
    /// smallest thing `run_stage` needs to dispatch a stage.
    fn test_gremlin(
        stages: Vec<RunnableStage>,
        default_client: &str,
    ) -> (tempfile::TempDir, Gremlin) {
        let tmp = tempfile::tempdir().unwrap();
        let artifact_dir = tmp.path().join("scratch").join("artifacts");
        let state_dir = tmp.path().join("state").join("gr-test");
        std::fs::create_dir_all(&artifact_dir).unwrap();
        std::fs::create_dir_all(&state_dir).unwrap();

        let data = serde_json::json!({
            "id": "gr-test",
            "attempt": "test-0001",
            "stage": "starting",
            "client": default_client,
        });
        state::write_state(&state_dir, data.as_object().unwrap()).unwrap();

        let mut state_data = StateData::new(Some("gr-test".to_string()));
        state_data.state_file = Some(state_dir.join("state.json"));

        let gremlin = Gremlin {
            id: validate_gremlin_id("gr-test").unwrap(),
            state_dir,
            artifact_dir: artifact_dir.clone(),
            pipeline_path: None,
            client_override: None,
            pipeline: Pipeline {
                name: "test".to_string(),
                path: PathBuf::from("test.yaml"),
                default_client: default_client.to_string(),
                base_ref: "main".to_string(),
                bootstrap: Bootstrap::default(),
                stages: stages.clone(),
                land: None,
            },
            registry: ArtifactRegistry::new(artifact_dir),
            worktree: None,
            worktree_parent: None,
            project_root: tmp.path().to_path_buf(),
            base_ref_sha: String::new(),
            base_ref: "main".to_string(),
            resume_from: None,
            state: state_data,
            env: HashMap::new(),
            client: Client::parse(default_client).unwrap(),
            loop_stack: Vec::new(),
            stage_inputs: HashMap::new(),
        };
        (tmp, gremlin)
    }

    /// A registry over a throwaway artifact directory, for the free functions.
    fn scratch_registry() -> (tempfile::TempDir, ArtifactRegistry) {
        let tmp = tempfile::tempdir().unwrap();
        let artifact_dir = tmp.path().join("artifacts");
        std::fs::create_dir_all(&artifact_dir).unwrap();
        let registry = ArtifactRegistry::new(artifact_dir);
        (tmp, registry)
    }

    // --- scope keys ---

    #[test]
    fn stage_key_uses_the_enclosing_scope() {
        assert_eq!(stage_key("", "plan"), "plan");
        assert_eq!(stage_key("a", "b"), "a/b");
        assert_eq!(stage_key("a/b", "c"), "a/b/c");
    }

    #[test]
    fn loop_iter_joins_the_stack() {
        assert_eq!(loop_iter_of(&[]), "1");
        assert_eq!(loop_iter_of(&[("loop".to_string(), 2)]), "loop~2");
        assert_eq!(
            loop_iter_of(&[("outer".to_string(), 1), ("inner".to_string(), 3)]),
            "outer~1~inner~3"
        );
    }

    #[test]
    fn truncate_cuts_on_a_char_boundary() {
        assert_eq!(truncate("short", 200), "short");
        assert_eq!(truncate("abcdef", 3), "abc");
        // "é" is two bytes, so a naive slice at 3 would split it.
        assert_eq!(truncate("aé", 2), "a");
    }

    // --- bail reading ---

    #[test]
    fn bail_reason_reads_the_bound_file() {
        let (_tmp, registry) = scratch_registry();
        assert!(bail_reason(&registry, "scope").is_none());

        let uri = Uri::parse("artifact://scope/bail").unwrap();
        let path = registry.write_into_registry(&uri, "boom\n").unwrap();
        assert_eq!(bail_reason(&registry, "scope").as_deref(), Some("boom"));

        // A binding whose file is gone is not a reason.
        std::fs::remove_file(&path).unwrap();
        assert!(bail_reason(&registry, "scope").is_none());

        // Neither is an empty one.
        registry.write_into_registry(&uri, "").unwrap();
        assert!(bail_reason(&registry, "scope").is_none());
    }

    #[test]
    fn global_bail_and_is_bail_set() {
        let (_tmp, registry) = scratch_registry();
        assert!(!is_bail_set(&registry, "scope"));
        assert!(global_bail_reason(&registry).is_none());

        let uri = Uri::parse(BAIL_KEY).unwrap();
        registry.write_into_registry(&uri, "stopped\n").unwrap();
        assert_eq!(global_bail_reason(&registry).as_deref(), Some("stopped"));
        assert!(is_bail_set(&registry, "scope"));
    }

    #[test]
    fn clear_stale_bail_unreads_a_previous_bail() {
        let (_tmp, registry) = scratch_registry();
        let uri = Uri::parse("artifact://scope/bail").unwrap();
        registry.write_into_registry(&uri, "old\n").unwrap();
        assert!(bail_reason(&registry, "scope").is_some());

        // The binding survives, the reason does not: that is what lets a
        // resumed iteration tell its own bail from the previous one's.
        clear_stale_bail(&registry, "scope");
        assert!(registry.is_registered("artifact://scope/bail"));
        assert!(bail_reason(&registry, "scope").is_none());
    }

    // --- agent ---

    #[tokio::test]
    async fn agent_skips_when_the_guard_artifact_is_live() {
        let yaml = r#"
- name: writer
  type: agent
  skip_if_exists: "artifact://done.md"
  client: "cmd:false"
  prompt: ["hi"]
"#;
        let (_tmp, mut gremlin) = test_gremlin(parse_stages(yaml), "cmd:true");
        gremlin
            .registry
            .write_into_registry(&Uri::parse("artifact://done.md").unwrap(), "done")
            .unwrap();

        let stage = gremlin.pipeline.stages[0].clone();
        assert!(run_stage(&stage, &mut gremlin).await.is_ok());
    }

    #[tokio::test]
    async fn skip_guard_accepts_a_bare_key() {
        // The guard is spelled without the scheme; it must still match the
        // registered URI, which is the only way the artifact is known.
        let yaml = r#"
- name: writer
  type: agent
  skip_if_exists: "done.md"
  client: "cmd:false"
  prompt: ["hi"]
"#;
        let (_tmp, mut gremlin) = test_gremlin(parse_stages(yaml), "cmd:true");
        gremlin
            .registry
            .write_into_registry(&Uri::parse("artifact://done.md").unwrap(), "done")
            .unwrap();

        let stage = gremlin.pipeline.stages[0].clone();
        // `cmd:false` would fail the stage if the guard did not skip it.
        assert!(run_stage(&stage, &mut gremlin).await.is_ok());
    }

    #[tokio::test]
    async fn agent_bails_when_the_client_reports_it() {
        // The command drains stdin before printing: a command that exits in
        // under a millisecond can close the pipe before the harness finishes
        // writing the prompt, and this test is about bail detection, not that
        // race.
        let yaml = r#"
- name: reviewer
  type: agent
  client: "cmd:sh -c \"cat >/dev/null && printf 'BAIL: other: boom'\""
  prompt: ["hi"]
"#;
        let (_tmp, mut gremlin) = test_gremlin(parse_stages(yaml), "cmd:true");
        let stage = gremlin.pipeline.stages[0].clone();

        match run_stage(&stage, &mut gremlin).await {
            Err(RunError::Bail { reason }) => assert_eq!(reason, "boom"),
            other => panic!("expected Bail, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn agent_commits_a_produced_bound_artifact() {
        // `sh -c 'cat >/dev/null'` drains the prompt before exiting; a command
        // that exits immediately can race the harness's stdin write into a
        // broken pipe. `sh -c` also ignores the `--model`/`--add-dir` args the
        // cmd backend appends.
        let yaml = r#"
- name: writer
  type: agent
  client: "cmd:sh -c 'cat >/dev/null'"
  bind:
    out: "artifact://{name}.md"
  prompt: ["hi"]
"#;
        let (_tmp, mut gremlin) = test_gremlin(parse_stages(yaml), "cmd:true");
        // The agent would have written this; a plain shell client cannot.
        std::fs::write(gremlin.artifact_dir.join("writer.md"), "content").unwrap();

        let stage = gremlin.pipeline.stages[0].clone();
        run_stage(&stage, &mut gremlin).await.unwrap();
        assert!(gremlin.registry.is_registered("artifact://writer.md"));
    }

    #[tokio::test]
    async fn agent_missing_artifact_bails() {
        let yaml = r#"
- name: writer
  type: agent
  client: "cmd:sh -c 'cat >/dev/null'"
  bind:
    out: "artifact://out.md"
  prompt: ["hi"]
"#;
        let (_tmp, mut gremlin) = test_gremlin(parse_stages(yaml), "cmd:true");
        let stage = gremlin.pipeline.stages[0].clone();

        match run_stage(&stage, &mut gremlin).await {
            Err(RunError::Bail { .. }) => {}
            other => panic!("expected Bail, got {other:?}"),
        }
    }

    // --- exec ---

    #[tokio::test]
    async fn exec_runs_its_commands() {
        let yaml = r#"
- name: noop
  type: exec
  options:
    cmds: ["true"]
"#;
        let (_tmp, mut gremlin) = test_gremlin(parse_stages(yaml), "cmd:true");
        let stage = gremlin.pipeline.stages[0].clone();
        assert!(run_stage(&stage, &mut gremlin).await.is_ok());
    }

    #[tokio::test]
    async fn exec_non_zero_exit_fails_the_stage() {
        let yaml = r#"
- name: broken
  type: exec
  options:
    cmds: ["gremlins-nonexistent-cmd-xyz"]
"#;
        let (_tmp, mut gremlin) = test_gremlin(parse_stages(yaml), "cmd:true");
        let stage = gremlin.pipeline.stages[0].clone();

        match run_stage(&stage, &mut gremlin).await {
            Err(RunError::StageFailed { stage, .. }) => assert_eq!(stage, "broken"),
            other => panic!("expected StageFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn exec_tolerates_a_non_zero_exit_when_it_bails() {
        let yaml = r#"
- name: guard
  type: exec
  bind:
    bail: "artifact://bail"
  options:
    cmds: ["echo reason > {bail}; exit 1"]
"#;
        let (_tmp, mut gremlin) = test_gremlin(parse_stages(yaml), "cmd:true");
        let stage = gremlin.pipeline.stages[0].clone();

        run_stage(&stage, &mut gremlin).await.unwrap();
        assert!(gremlin.registry.is_registered(BAIL_KEY));
        assert_eq!(
            global_bail_reason(&gremlin.registry).as_deref(),
            Some("reason")
        );
    }

    // --- sequence ---

    #[tokio::test]
    async fn sequence_runs_children_and_clears_done_tracking() {
        let yaml = r#"
- name: seq
  type: sequence
  body:
    - name: one
      type: exec
      options:
        cmds: ["echo one > one.marker"]
    - name: two
      type: exec
      options:
        cmds: ["echo two > two.marker"]
"#;
        let (tmp, mut gremlin) = test_gremlin(parse_stages(yaml), "cmd:true");
        let stage = gremlin.pipeline.stages[0].clone();

        run_stage(&stage, &mut gremlin).await.unwrap();
        assert!(tmp.path().join("one.marker").exists());
        assert!(tmp.path().join("two.marker").exists());
        // The sequence is spent, so its tracking must not survive it.
        assert!(gremlin.state.done_for("seq").is_empty());
    }

    #[tokio::test]
    async fn sequence_skips_children_already_marked_done() {
        let yaml = r#"
- name: seq
  type: sequence
  body:
    - name: one
      type: exec
      options:
        cmds: ["echo one > one.marker"]
    - name: two
      type: exec
      options:
        cmds: ["echo two > two.marker"]
"#;
        let (tmp, mut gremlin) = test_gremlin(parse_stages(yaml), "cmd:true");
        gremlin.state.mark_done("seq", "one");

        let stage = gremlin.pipeline.stages[0].clone();
        run_stage(&stage, &mut gremlin).await.unwrap();
        assert!(!tmp.path().join("one.marker").exists());
        assert!(tmp.path().join("two.marker").exists());
    }

    #[tokio::test]
    async fn sequence_propagates_a_child_failure() {
        let yaml = r#"
- name: seq
  type: sequence
  body:
    - name: broken
      type: exec
      options:
        cmds: ["gremlins-nonexistent-cmd-xyz"]
"#;
        let (_tmp, mut gremlin) = test_gremlin(parse_stages(yaml), "cmd:true");
        let stage = gremlin.pipeline.stages[0].clone();

        match run_stage(&stage, &mut gremlin).await {
            Err(RunError::StageFailed { stage, .. }) => assert_eq!(stage, "broken"),
            other => panic!("expected StageFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn sequence_reports_a_scoped_child_bail() {
        // A resumed run can find the sequence's own bail already on disk; the
        // sequence must surface it rather than reporting a successful subtree.
        let yaml = r#"
- name: seq
  type: sequence
  body:
    - name: one
      type: exec
      options:
        cmds: ["true"]
"#;
        let (_tmp, mut gremlin) = test_gremlin(parse_stages(yaml), "cmd:true");
        gremlin
            .registry
            .write_into_registry(&Uri::parse("artifact://seq/bail").unwrap(), "boom")
            .unwrap();
        let stage = gremlin.pipeline.stages[0].clone();

        match run_stage(&stage, &mut gremlin).await {
            Err(RunError::Bail { reason }) => assert_eq!(reason, "boom"),
            other => panic!("expected Bail, got {other:?}"),
        }
        assert!(gremlin.state.done_for("seq").is_empty());
    }

    #[tokio::test]
    async fn sequence_reports_a_global_child_bail() {
        let yaml = r#"
- name: seq
  type: sequence
  body:
    - name: guard
      type: exec
      bind:
        bail: "artifact://bail"
      options:
        cmds: ["echo stopped > {bail}; exit 1"]
    - name: never
      type: exec
      options:
        cmds: ["touch never.marker"]
"#;
        let (tmp, mut gremlin) = test_gremlin(parse_stages(yaml), "cmd:true");
        let stage = gremlin.pipeline.stages[0].clone();

        match run_stage(&stage, &mut gremlin).await {
            Err(RunError::Bail { reason }) => assert_eq!(reason, "stopped"),
            other => panic!("expected Bail, got {other:?}"),
        }
        assert!(!tmp.path().join("never.marker").exists());
    }

    // --- loop ---

    #[tokio::test]
    async fn loop_stops_when_the_artifact_exists() {
        let yaml = r#"
- name: poll
  type: loop
  max-iterations: 3
  stop_when_exists: "artifact://done"
  body:
    - name: tick
      type: exec
      options:
        cmds: ["true"]
"#;
        let (_tmp, mut gremlin) = test_gremlin(parse_stages(yaml), "cmd:true");
        gremlin
            .registry
            .write_into_registry(&Uri::parse("artifact://done").unwrap(), "yes")
            .unwrap();

        let stage = gremlin.pipeline.stages[0].clone();
        assert!(run_stage(&stage, &mut gremlin).await.is_ok());
        // The frame is popped on the stop path too.
        assert!(gremlin.loop_stack.is_empty());
    }

    #[tokio::test]
    async fn loop_exhaustion_bails() {
        let yaml = r#"
- name: poll
  type: loop
  max-iterations: 2
  body:
    - name: tick
      type: exec
      options:
        cmds: ["true"]
"#;
        let (_tmp, mut gremlin) = test_gremlin(parse_stages(yaml), "cmd:true");
        let stage = gremlin.pipeline.stages[0].clone();

        match run_stage(&stage, &mut gremlin).await {
            Err(RunError::Bail { reason }) => {
                assert!(reason.contains("exhausted"), "{reason}");
            }
            other => panic!("expected Bail, got {other:?}"),
        }
        assert!(gremlin.loop_stack.is_empty());
    }

    #[tokio::test]
    async fn loop_reports_a_scoped_bail() {
        let yaml = r#"
- name: poll
  type: loop
  max-iterations: 3
  body:
    - name: tick
      type: exec
      bind:
        bail: "artifact://{loop_iter}/bail"
      options:
        cmds: ["echo boom > {bail}; exit 1"]
"#;
        let (_tmp, mut gremlin) = test_gremlin(parse_stages(yaml), "cmd:true");
        let stage = gremlin.pipeline.stages[0].clone();

        // The child's non-zero exit is tolerated because it bound a bail URI;
        // the loop is what turns the written artifact into the run's bail.
        match run_stage(&stage, &mut gremlin).await {
            Err(RunError::Bail { reason }) => assert_eq!(reason, "boom"),
            other => panic!("expected Bail, got {other:?}"),
        }
        assert!(gremlin.registry.is_registered("artifact://poll~1/bail"));
    }

    #[tokio::test]
    async fn loop_stops_the_body_at_the_bailing_child() {
        let yaml = r#"
- name: poll
  type: loop
  max-iterations: 3
  body:
    - name: guard
      type: exec
      bind:
        bail: "artifact://{loop_iter}/bail"
      options:
        cmds: ["echo boom > {bail}; exit 1"]
    - name: after
      type: exec
      options:
        cmds: ["touch after.marker"]
"#;
        let (tmp, mut gremlin) = test_gremlin(parse_stages(yaml), "cmd:true");
        let stage = gremlin.pipeline.stages[0].clone();

        match run_stage(&stage, &mut gremlin).await {
            Err(RunError::Bail { .. }) => {}
            other => panic!("expected Bail, got {other:?}"),
        }
        assert!(!tmp.path().join("after.marker").exists());
    }

    #[tokio::test]
    async fn loop_iter_is_the_loop_name_even_inside_a_sequence() {
        let yaml = r#"
- name: seq
  type: sequence
  body:
    - name: poll
      type: loop
      max-iterations: 2
      body:
        - name: guard
          type: exec
          bind:
            bail: "artifact://{loop_iter}/bail"
          options:
            cmds: ["echo boom > {bail}; exit 1"]
"#;
        let (_tmp, mut gremlin) = test_gremlin(parse_stages(yaml), "cmd:true");
        let stage = gremlin.pipeline.stages[0].clone();

        match run_stage(&stage, &mut gremlin).await {
            Err(RunError::Bail { reason }) => assert_eq!(reason, "boom"),
            other => panic!("expected Bail, got {other:?}"),
        }
        // The enclosing sequence scopes `done_children`, not the iteration key.
        assert!(gremlin.registry.is_registered("artifact://poll~1/bail"));
    }

    #[tokio::test]
    async fn nested_loops_join_their_own_names() {
        let yaml = r#"
- name: outer
  type: loop
  max-iterations: 1
  body:
    - name: inner
      type: loop
      max-iterations: 1
      body:
        - name: guard
          type: exec
          bind:
            bail: "artifact://{loop_iter}/bail"
          options:
            cmds: ["echo boom > {bail}; exit 1"]
"#;
        let (_tmp, mut gremlin) = test_gremlin(parse_stages(yaml), "cmd:true");
        let stage = gremlin.pipeline.stages[0].clone();

        match run_stage(&stage, &mut gremlin).await {
            Err(RunError::Bail { reason }) => assert_eq!(reason, "boom"),
            other => panic!("expected Bail, got {other:?}"),
        }
        assert!(gremlin
            .registry
            .is_registered("artifact://outer~1~inner~1/bail"));
    }

    #[tokio::test]
    async fn zero_iteration_loop_bails() {
        // `Loop::with_dict` refuses a zero budget, but the parsed node may still
        // carry one; the runner must report exhaustion rather than succeed.
        let stage = RunnableStage::Loop {
            attrs: {
                let mut attrs = StageAttrs::new("poll".to_string());
                attrs.stage_type = "loop".to_string();
                attrs
            },
            max_iterations: 0,
            stop_when_exists: None,
            interval: None,
            client: None,
            body: Vec::new(),
        };
        let (_tmp, mut gremlin) = test_gremlin(Vec::new(), "cmd:true");

        match run_stage(&stage, &mut gremlin).await {
            Err(RunError::Bail { reason }) => {
                assert!(reason.contains("exhausted"), "{reason}");
            }
            other => panic!("expected Bail, got {other:?}"),
        }
        assert!(gremlin.loop_stack.is_empty());
    }

    // --- parallel ---

    #[tokio::test]
    async fn parallel_is_dispatched_to_run_parallel() {
        // A single-child parallel group with a trivially successful exec
        // stage exercises the new path through `run_parallel`.
        let yaml = r#"
- name: group
  parallel:
    - name: a
      type: exec
      options:
        cmds: ["true"]
"#;
        let (_tmp, mut gremlin) = test_gremlin(parse_stages(yaml), "cmd:true");
        let stage = gremlin.pipeline.stages[0].clone();

        let result = run_stage(&stage, &mut gremlin).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }

    // --- the run loop ---

    #[tokio::test]
    async fn run_walks_every_stage_and_writes_terminal_state() {
        let yaml = r#"
- name: one
  type: exec
  options:
    cmds: ["true"]
- name: two
  type: exec
  options:
    cmds: ["true"]
"#;
        let (tmp, mut gremlin) = test_gremlin(parse_stages(yaml), "cmd:true");
        let state_dir = tmp.path().join("state").join("gr-test");

        assert_eq!(gremlin.run().await.unwrap(), 0);
        assert!(state_dir.join("finished").is_file());
        let raw: Value =
            serde_json::from_str(&std::fs::read_to_string(state_dir.join("state.json")).unwrap())
                .unwrap();
        assert_eq!(raw["status"], "done");
        assert_eq!(raw["exit_code"], 0);
    }

    #[tokio::test]
    async fn run_records_a_bail_and_returns_one() {
        // Drains stdin first, for the same reason as the bail test above.
        let yaml = r#"
- name: reviewer
  type: agent
  client: "cmd:sh -c \"cat >/dev/null && printf 'BAIL: other: boom'\""
  prompt: ["hi"]
"#;
        let (tmp, mut gremlin) = test_gremlin(parse_stages(yaml), "cmd:true");
        let state_dir = tmp.path().join("state").join("gr-test");

        assert_eq!(gremlin.run().await.unwrap(), 1);

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
        assert_eq!(bail["detail"], "boom");

        let raw: Value =
            serde_json::from_str(&std::fs::read_to_string(state_dir.join("state.json")).unwrap())
                .unwrap();
        assert_eq!(raw["status"], "stopped");
        assert_eq!(raw["exit_code"], 1);
    }

    #[tokio::test]
    async fn run_resumes_from_a_named_stage() {
        let yaml = r#"
- name: first
  type: exec
  options:
    cmds: ["touch first.marker"]
- name: second
  type: exec
  options:
    cmds: ["touch second.marker"]
- name: third
  type: exec
  options:
    cmds: ["touch third.marker"]
"#;
        let (tmp, mut gremlin) = test_gremlin(parse_stages(yaml), "cmd:true");
        gremlin.resume_from = Some("second".to_string());

        assert_eq!(gremlin.run().await.unwrap(), 0);
        assert!(!tmp.path().join("first.marker").exists());
        assert!(tmp.path().join("second.marker").exists());
        assert!(tmp.path().join("third.marker").exists());
    }

    #[tokio::test]
    async fn run_propagates_a_non_bail_failure() {
        let yaml = r#"
- name: broken
  type: exec
  options:
    cmds: ["gremlins-nonexistent-cmd-xyz"]
"#;
        let (tmp, mut gremlin) = test_gremlin(parse_stages(yaml), "cmd:true");
        let state_dir = tmp.path().join("state").join("gr-test");

        match gremlin.run().await {
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

    #[tokio::test]
    async fn run_stops_on_a_registry_bail_and_records_it() {
        let yaml = r#"
- name: guard
  type: exec
  bind:
    bail: "artifact://bail"
  options:
    cmds: ["echo stopped > {bail}; exit 1"]
- name: never
  type: exec
  options:
    cmds: ["touch never.marker"]
"#;
        let (tmp, mut gremlin) = test_gremlin(parse_stages(yaml), "cmd:true");
        let state_dir = tmp.path().join("state").join("gr-test");

        assert_eq!(gremlin.run().await.unwrap(), 1);
        assert!(!tmp.path().join("never.marker").exists());

        // The registry bail is recorded in state like any other bail, so
        // `status` and the bail file agree with it.
        let raw: Value =
            serde_json::from_str(&std::fs::read_to_string(state_dir.join("state.json")).unwrap())
                .unwrap();
        assert_eq!(raw["status"], "stopped");
        let attempt = raw["attempt"].as_str().unwrap();
        let bail: Value = serde_json::from_str(
            &std::fs::read_to_string(state_dir.join(format!("bail_{attempt}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(bail["detail"], "stopped");
    }

    // --- bootstrap integration ---

    /// The run-loop half of bootstrap: `run` runs the block before the stage
    /// walk, and a failure is a bail — recorded and terminal, never a crash.
    #[tokio::test]
    async fn run_fails_the_bootstrap_and_records_a_bail() {
        let yaml = r#"
- name: never
  type: exec
  options:
    cmds: ["touch never.marker"]
"#;
        let (tmp, mut gremlin) = test_gremlin(parse_stages(yaml), "cmd:true");
        let worktree = tmp.path().join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        gremlin.worktree = Some(worktree.clone());
        gremlin.pipeline.bootstrap = Bootstrap {
            cmds: vec!["exit 5".to_string()],
            ..Default::default()
        };
        let state_dir = tmp.path().join("state").join("gr-test");

        assert_eq!(gremlin.run().await.unwrap(), 1);
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
        let yaml = r#"
- name: reader
  type: exec
  options:
    cmds: ["cat marker.txt > read.txt"]
"#;
        let (tmp, mut gremlin) = test_gremlin(parse_stages(yaml), "cmd:true");
        let worktree = tmp.path().join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        gremlin.worktree = Some(worktree.clone());
        gremlin.pipeline.bootstrap = Bootstrap {
            cmds: vec!["echo prepared > marker.txt".to_string()],
            ..Default::default()
        };

        // The stage `cat`s a file only the bootstrap wrote: a non-zero exit
        // here would mean the ordering was wrong.
        assert_eq!(gremlin.run().await.unwrap(), 0);
        assert!(worktree.join("marker.txt").is_file());
        assert!(worktree.join("read.txt").is_file());
    }

    #[tokio::test]
    async fn run_skips_the_bootstrap_on_resume() {
        let yaml = r#"
- name: only
  type: exec
  options:
    cmds: ["true"]
"#;
        let (tmp, mut gremlin) = test_gremlin(parse_stages(yaml), "cmd:true");
        let worktree = tmp.path().join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        gremlin.worktree = Some(worktree.clone());
        gremlin.resume_from = Some("only".to_string());
        gremlin.pipeline.bootstrap = Bootstrap {
            cmds: vec!["touch bootstrap.marker".to_string()],
            ..Default::default()
        };

        assert_eq!(gremlin.run().await.unwrap(), 0);
        assert!(!worktree.join("bootstrap.marker").exists());
    }

    // --- git-backed end to end ---

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    fn git(root: &StdPath, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .expect("failed to run git")
    }

    /// A repository with one commit and the given `.gremlins/demo.yaml`.
    fn init_repo(root: &StdPath, pipeline_yaml: &str) -> bool {
        if !git(root, &["init", "-q"]).status.success() {
            return false;
        }
        let overlay = root.join(".gremlins");
        if std::fs::create_dir_all(&overlay).is_err() {
            return false;
        }
        if std::fs::write(overlay.join("demo.yaml"), pipeline_yaml).is_err() {
            return false;
        }
        if !git(root, &["add", "."]).status.success() {
            return false;
        }
        git(
            root,
            &[
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=Test",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        )
        .status
        .success()
    }

    #[tokio::test]
    // The shared env guard holds a plain `std::sync::Mutex` across the run's
    // awaits. That is safe here: `#[tokio::test]` drives a current-thread
    // runtime, so no other task can be scheduled on this thread while the lock
    // is held, and the lock exists precisely to keep the sandbox override from
    // being observed half-swapped by another test.
    #[allow(clippy::await_holding_lock)]
    async fn create_then_run_end_to_end() {
        if !git_available() {
            eprintln!("git unavailable; skipping create_then_run_end_to_end");
            return;
        }

        let mut env = EnvGuard::lock();
        let sandbox = tempfile::tempdir().unwrap();
        env.set("GREMLINS_SANDBOX_ROOT", sandbox.path());

        let repo = tempfile::tempdir().unwrap();
        let prepared = init_repo(
            repo.path(),
            "default_client: 'cmd:true'\n\
             stages:\n\
             \x20 - name: solo\n\
             \x20   type: exec\n\
             \x20   options:\n\
             \x20     cmds: [\"true\"]\n\
             \x20 - name: seq\n\
             \x20   type: sequence\n\
             \x20   body:\n\
             \x20     - name: inner\n\
             \x20       type: exec\n\
             \x20       options:\n\
             \x20         cmds: [\"true\"]\n",
        );

        // Everything fallible runs before the env is restored, so a failure
        // cannot leave the sandbox override behind for another test.
        let outcome: Result<(i32, Value), String> = if prepared {
            async {
                let pipeline_path = repo.path().join(".gremlins").join("demo.yaml");
                let mut gremlin = Gremlin::create(
                    "gr-e2e",
                    &pipeline_path,
                    None,
                    None,
                    None,
                    &HashMap::new(),
                    false,
                    None,
                    None,
                    None,
                )
                .map_err(|error| error.to_string())?;
                let code = gremlin.run().await.map_err(|error| error.to_string())?;
                // Read back through the handle's own state dir. The sandbox
                // override is shared process state, and the pre-existing
                // config tests clear it for their own duration; the path the
                // launch actually resolved is the honest one to assert on.
                let state_file = gremlin.state_dir.join("state.json");
                let raw =
                    std::fs::read_to_string(&state_file).map_err(|error| error.to_string())?;
                let value: Value = serde_json::from_str(&raw).map_err(|error| error.to_string())?;
                Ok((code, value))
            }
            .await
        } else {
            Err("could not prepare a git fixture".to_string())
        };

        let (code, raw) = outcome.unwrap();
        assert_eq!(code, 0);
        assert_eq!(raw["status"], "done");
        assert_eq!(raw["exit_code"], 0);
    }
}
