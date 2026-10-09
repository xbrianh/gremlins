//! The sequential stage run loop.
//!
//! One gremlin, one stage at a time: [`Gremlin::run`] walks the definition's
//! stages in declaration order and drives each through [`run_stage`], while the
//! scoped dispatcher below carries the two pieces of bookkeeping the Python
//! executor kept around every stage — the artifact guard (`skip_if_exists`)
//! checked before dispatch, and the scope key (`a/b`, `loop~2`) a stage is
//! tracked under for bail lookups.
//!
//! This is sequencing only. Parallel groups are a later milestone and say so
//! rather than pretending to fan out.

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::{Map, Value};

use crate::artifacts::resolve::ResolveError;
use crate::clients::backend::{ClientError, RunParams};
use crate::clients::client::Client;
use crate::definition::{ExecutorStage, GremlinDefinition};
use crate::executor::agent_runner::{commit_agent, prepare_agent, AgentError};
use crate::executor::bootstrap::run_definition_bootstrap;
use crate::executor::exec_runner::{commit_exec, prepare_exec, run_shell, ExecError};
use crate::executor::gremlin::Gremlin;
#[cfg(test)]
use crate::executor::gremlin::{GremlinConfig, ScratchDir, WorkDir};
use crate::executor::parallel::run_parallel;
use crate::executor::state;
use crate::executor::state::{Collision, StateStore};
use crate::executor::supervisor::get_run_map;
use crate::executor::vars;
use crate::executor::RunError;

/// Send a log line through the per-gremlin channel if one is configured.
fn send_log(tx: &Option<tokio::sync::mpsc::UnboundedSender<String>>, msg: String) {
    if let Some(tx) = tx {
        let _ = tx.send(msg);
    }
}

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
    run_stage_scoped(stage, gremlin, "", None).await
}

/// Run one stage, tracking it under `scope`.
///
/// The skip guard only runs for Sequence stages — leaf stages (Agent, Exec)
/// no longer carry `skip_if_exists`; the guard lives on the enclosing
/// Sequence that wraps them.
async fn run_stage_scoped(
    stage: &ExecutorStage,
    gremlin: &mut Gremlin,
    scope: &str,
    enclosing_client: Option<&str>,
) -> Result<(), RunError> {
    let _attempt = gremlin.state.read_str("attempt");
    send_log(
        &gremlin.runtime_config.log_tx,
        format!(
            "stage '{}': entering (type={})",
            stage.name(),
            stage.stage_type()
        ),
    );

    match stage {
        ExecutorStage::Agent { .. } => run_agent(stage, gremlin, enclosing_client).await,
        ExecutorStage::Exec { .. } => run_exec(stage, gremlin, enclosing_client).await,
        ExecutorStage::Sequence(_) => {
            log::debug!("stage '{}': entering sequence", stage.name());
            run_sequence(stage, gremlin, scope, enclosing_client).await
        }
        ExecutorStage::Parallel { .. } => {
            log::debug!("dispatching stage '{}' to run_parallel", stage.name());
            run_parallel(stage, gremlin, enclosing_client).await
        }
        ExecutorStage::Done => Ok(()),
    }
}

/// Resolve the client spec string for a stage, consulting:
/// 1. The stage's own `client:` field (always wins)
/// 2. The enclosing composite's explicit `client:` (new — the `fill_client` replacement)
/// 3. `default-client-by-stage` from runtime config (exact → longest prefix)
/// 4. The definition's `default_client`
fn resolve_client_spec(
    stage: &ExecutorStage,
    gremlin: &Gremlin,
    enclosing_client: Option<&str>,
) -> String {
    // 1. Explicit stage client always wins
    if let Some(spec) = stage.client() {
        return spec.0.clone();
    }

    // 2. Enclosing composite's explicit client
    if let Some(client) = enclosing_client {
        return client.to_string();
    }

    let stage_name = stage.name();

    // 3. Consult default-client-by-stage from runtime config
    {
        let exact = &gremlin.runtime_config.stage_clients_exact;
        let prefix = &gremlin.runtime_config.stage_clients_prefix;

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

    // 4. Fall back to definition default
    gremlin.definition.default_client().to_string()
}

/// The client a stage runs with: resolved via [`resolve_client_spec`], then
/// parsed. When the resolved spec equals the definition default, the gremlin's
/// already-constructed client handle is reused to avoid building a second
/// backend for the same spec.
fn resolve_client(
    stage: &ExecutorStage,
    gremlin: &Gremlin,
    enclosing_client: Option<&str>,
) -> Result<Client, RunError> {
    let spec = resolve_client_spec(stage, gremlin, enclosing_client);
    if spec == gremlin.definition.default_client() {
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

async fn run_agent(
    node: &ExecutorStage,
    gremlin: &mut Gremlin,
    enclosing_client: Option<&str>,
) -> Result<(), RunError> {
    let ExecutorStage::Agent {
        stage: agent,
        client: _stage_client,
    } = node
    else {
        unreachable!("run_agent is only called for agent stages")
    };

    let client = resolve_client(node, gremlin, enclosing_client)?;
    let framework_subs = gremlin.framework_subs(&agent.name);
    let _attempt = gremlin.state.read_str("attempt");
    let loop_iter = gremlin.loop_iter.clone();

    send_log(
        &gremlin.runtime_config.log_tx,
        format!(
            "[{name}][{model}] agent: preparing",
            name = agent.name,
            model = client.model()
        ),
    );

    // Compute checkout keys: outputs_map keys + filepath-style interpolation keys.
    // Content interpolation keys are NOT included — they're read once at prepare time.
    //
    // We need to resolve template variables in the URIs before looking them up.
    // Do a preliminary content-only resolution against the main registry, then
    // use those values + framework_subs + string_options to substitute URIs.
    let str_opts = vars::string_options(&agent.options);
    let (content_map, filepath_map) =
        crate::artifacts::resolve::split_interpolation_map(&agent.interpolation_map);
    let content_interpolated = crate::artifacts::resolve::resolve_interpolation_map(
        gremlin.state.store_ref(),
        &content_map,
        &loop_iter,
    )
    .await
    .map_err(|error| match error {
        ResolveError::MissingArtifact(key) => RunError::Bail {
            reason: format!("artifact not bound: {key:?}"),
        },
        other => RunError::StageFailed {
            stage: agent.name.clone(),
            message: other.to_string(),
        },
    })?;

    // Build substitution map for URI resolution: content-interpolated values +
    // framework_subs. (Bind paths and filepath interpolation aren't available yet.)
    let mut uri_subs: HashMap<String, String> = content_interpolated.clone();
    uri_subs.extend(framework_subs.clone());

    let mut checkout_keys: Vec<String> = Vec::new();
    for raw_uri_str in agent.outputs_map.values() {
        let resolved = vars::substitute_vars(raw_uri_str, &str_opts, &uri_subs, &framework_subs);
        // Strip optional marker (? or ?fallback).
        let resolved = match resolved.find('?') {
            Some(pos) => &resolved[..pos],
            None => &resolved[..],
        };
        if !loop_iter.is_empty() {
            let resolved = resolved.replace("{loop_iter}", &loop_iter);
            if resolved.starts_with("artifact://") {
                checkout_keys.push(resolved);
            }
        } else if resolved.starts_with("artifact://") {
            checkout_keys.push(resolved.to_string());
        }
    }
    for raw in filepath_map.values() {
        let resolved = vars::substitute_vars(raw, &str_opts, &uri_subs, &framework_subs);
        let resolved = match resolved.find('?') {
            Some(pos) => &resolved[..pos],
            None => &resolved[..],
        };
        if !loop_iter.is_empty() {
            let resolved = resolved.replace("{loop_iter}", &loop_iter);
            if resolved.starts_with("artifact://") {
                checkout_keys.push(resolved);
            }
        } else if resolved.starts_with("artifact://") {
            checkout_keys.push(resolved.to_string());
        }
    }

    let local_registry = gremlin
        .state
        .checkout_registry(&checkout_keys)
        .await
        .map_err(|error| RunError::StageFailed {
            stage: agent.name.clone(),
            message: error.to_string(),
        })?;

    let mut prepared = prepare_agent(
        agent,
        gremlin.state.store_ref(),
        local_registry.as_ref(),
        &loop_iter,
        &framework_subs,
    )
    .await
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
    prepared.artifact_dir = local_registry.artifact_dir().to_string_lossy().into_owned();

    std::fs::create_dir_all(local_registry.artifact_dir())?;

    let stage_env = gremlin.env.clone();

    let params = RunParams {
        prompt: prepared.user_prompt(),
        label: prepared.name.clone(),
        model: prepared
            .model
            .clone()
            .or_else(|| Some(client.model().to_string())),
        raw_path: Some(
            gremlin
                .state
                .artifact_dir()
                .join(format!("stream-{}.jsonl", prepared.name)),
        ),
        capture_events: false,
        on_timeout_prompt: None,
        max_retries: 3,
        cwd: Some(gremlin.cwd()),
        artifact_dir: Some(local_registry.artifact_dir().to_path_buf()),
        idle_timeout: None,
        extra_env: Some(stage_env),
        expected_artifact_paths: prepared
            .expected_artifact_paths
            .iter()
            .map(PathBuf::from)
            .collect(),
        system_prompt: Some(prepared.system_prompt()),
        gremlin_id: Some(gremlin.id.to_string()),
        log_tx: gremlin.runtime_config.log_tx.clone(),
        base_env: Some(gremlin.env.clone()),
        task_clients_exact: gremlin.runtime_config.task_clients_exact.clone(),
        task_clients_prefix: gremlin.runtime_config.task_clients_prefix.clone(),
        cancel_token: gremlin.cancel_token.clone(),
        stream_events: gremlin.runtime_config.stream_events.clone(),
    };

    // Build interactive session separately from RunParams.
    // Use the pre-created session from launch time if available.
    // The handle's cmd_tx is already paired with this session's
    // cmd_rx from the original split().
    // If the session was already consumed (subsequent stages),
    // create a fresh cmd channel pair and swap the handle's cmd_tx.
    let interactive = gremlin.interactive_session.take().or_else(|| {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let pause = gremlin
            .runtime_config
            .interactive
            .as_ref()
            .map(|h| h.pause.clone());
        if let Ok(mut map) = get_run_map().lock() {
            if let Some(handle) = map.get_mut(gremlin.id.as_str()) {
                handle.interactive.cmd_tx = tx;
            }
        }
        let evt_tx = gremlin
            .runtime_config
            .interactive
            .as_ref()
            .map(|h| h.evt_tx.clone());
        evt_tx.map(|evt_tx| crate::clients::interactive::InteractiveSession {
            cmd_rx: rx,
            evt_tx,
            pause: pause.expect("pause token must be present when evt_tx is"),
        })
    });

    log::debug!(
        "agent stage '{}' (gremlin={}): invoking client.run (model={})",
        prepared.name,
        gremlin.id.as_str(),
        params.model.as_deref().unwrap_or("default")
    );

    let completed = client
        .run(params, interactive)
        .await
        .map_err(|error| match error {
            ClientError::Bail { reason } => RunError::Bail { reason },
            other => RunError::StageFailed {
                stage: prepared.name.clone(),
                message: other.to_string(),
            },
        })?;

    send_log(
        &gremlin.runtime_config.log_tx,
        format!(
            "[{name}][{model}] agent: completed (turns={turns})",
            name = prepared.name,
            model = client.model(),
            turns = completed.token_usage.as_ref().map(|u| u.turns).unwrap_or(0)
        ),
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

    commit_agent(&prepared, local_registry.as_ref())
        .await
        .map_err(|error| match error {
            // A declared output that never materialised is the agent's bail.
            AgentError::MissingArtifact { .. } => RunError::Bail {
                reason: error.to_string(),
            },
            other => RunError::StageFailed {
                stage: prepared.name.clone(),
                message: other.to_string(),
            },
        })?;

    // Merge the localized registry back into the main registry.
    gremlin
        .state
        .join(local_registry.as_ref(), Collision::Ignore, None)
        .await
        .map_err(|error| RunError::StageFailed {
            stage: prepared.name.clone(),
            message: error.to_string(),
        })?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Exec
// ---------------------------------------------------------------------------

async fn run_exec(
    node: &ExecutorStage,
    gremlin: &mut Gremlin,
    _enclosing_client: Option<&str>,
) -> Result<(), RunError> {
    let ExecutorStage::Exec {
        stage: exec,
        client: _stage_client,
    } = node
    else {
        unreachable!("run_exec is only called for exec stages")
    };

    let framework_subs = gremlin.framework_subs(&exec.name);
    let _attempt = gremlin.state.read_str("attempt");
    let loop_iter = gremlin.loop_iter.clone();

    send_log(
        &gremlin.runtime_config.log_tx,
        format!("[{}] exec: preparing", exec.name),
    );

    // Compute checkout keys: outputs_map keys + filepath-style interpolation keys.
    // Content interpolation keys are NOT included — they're read once at prepare time.
    let str_opts = vars::string_options(&exec.options);
    let (content_map, filepath_map) =
        crate::artifacts::resolve::split_interpolation_map(&exec.interpolation_map);
    let content_interpolated = crate::artifacts::resolve::resolve_interpolation_map(
        gremlin.state.store_ref(),
        &content_map,
        &loop_iter,
    )
    .await
    .map_err(|error| match error {
        ResolveError::MissingArtifact(key) => RunError::Bail {
            reason: format!("artifact not bound: {key:?}"),
        },
        other => RunError::StageFailed {
            stage: exec.name.clone(),
            message: other.to_string(),
        },
    })?;

    let mut uri_subs: HashMap<String, String> = content_interpolated.clone();
    uri_subs.extend(framework_subs.clone());

    let mut checkout_keys: Vec<String> = Vec::new();
    for raw_uri_str in exec.outputs_map.values() {
        let resolved = vars::substitute_vars(raw_uri_str, &str_opts, &uri_subs, &framework_subs);
        // Strip optional marker (? or ?fallback).
        let resolved = match resolved.find('?') {
            Some(pos) => &resolved[..pos],
            None => &resolved[..],
        };
        if !loop_iter.is_empty() {
            let resolved = resolved.replace("{loop_iter}", &loop_iter);
            if resolved.starts_with("artifact://") {
                checkout_keys.push(resolved);
            }
        } else if resolved.starts_with("artifact://") {
            checkout_keys.push(resolved.to_string());
        }
    }
    for raw in filepath_map.values() {
        let resolved = vars::substitute_vars(raw, &str_opts, &uri_subs, &framework_subs);
        let resolved = match resolved.find('?') {
            Some(pos) => &resolved[..pos],
            None => &resolved[..],
        };
        if !loop_iter.is_empty() {
            let resolved = resolved.replace("{loop_iter}", &loop_iter);
            if resolved.starts_with("artifact://") {
                checkout_keys.push(resolved);
            }
        } else if resolved.starts_with("artifact://") {
            checkout_keys.push(resolved.to_string());
        }
    }

    let local_registry = gremlin
        .state
        .checkout_registry(&checkout_keys)
        .await
        .map_err(|error| RunError::StageFailed {
            stage: exec.name.clone(),
            message: error.to_string(),
        })?;

    let mut prepared = prepare_exec(
        exec,
        gremlin.state.store_ref(),
        local_registry.as_ref(),
        &loop_iter,
        &framework_subs,
    )
    .await
    .map_err(|error| match error {
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
    })?;

    prepared.cwd = gremlin.cwd();
    prepared.artifact_dir = local_registry.artifact_dir().to_path_buf();
    prepared.state_dir = gremlin.state.state_dir().to_path_buf();
    prepared.env = gremlin.env.clone();
    prepared.base_env = gremlin.runtime_config.base_process_env.clone();
    prepared.log_tx = gremlin.runtime_config.log_tx.clone();

    if !prepared.cmds.is_empty() {
        send_log(
            &gremlin.runtime_config.log_tx,
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
            .map_err(|error| RunError::StageFailed {
                stage: prepared.name.clone(),
                message: error.to_string(),
            })?;
    }

    commit_exec(&prepared, local_registry.as_ref())
        .await
        .map_err(|error| match error {
            // An unproduced output that is not a bail URI aborts the run.
            ExecError::MissingArtifact { .. } => RunError::Bail {
                reason: error.to_string(),
            },
            other => RunError::StageFailed {
                stage: prepared.name.clone(),
                message: other.to_string(),
            },
        })?;

    // Merge the localized registry back into the main registry.
    gremlin
        .state
        .join(local_registry.as_ref(), Collision::Ignore, None)
        .await
        .map_err(|error| RunError::StageFailed {
            stage: prepared.name.clone(),
            message: error.to_string(),
        })?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Sequence
// ---------------------------------------------------------------------------

/// Run a sequence's children in order, repeating when `max_iterations > 1`.
///
/// The `skip_if_exists` guard is checked before the first iteration (skip)
/// and after each subsequent iteration (stop).  When `interval` is set and
/// `max_iterations > 1`, the executor sleeps between iterations.
///
/// On resume, `goto` already positions execution at the correct top-level
/// stage; iterations naturally replay the body from that child.
async fn run_sequence(
    node: &ExecutorStage,
    gremlin: &mut Gremlin,
    scope: &str,
    enclosing_client: Option<&str>,
) -> Result<(), RunError> {
    let ExecutorStage::Sequence(seq) = node else {
        unreachable!("run_sequence is only called for sequence stages")
    };

    let enclosing_client = seq
        .client
        .as_ref()
        .map(|c| c.0.as_str())
        .or(enclosing_client);

    let key = stage_key(scope, &seq.name);
    let max_iterations = seq.max_iterations.max(1);
    let skip_guard = &seq.skip_if_exists;

    // Non-repeating: run children once under the sequence's own scope key,
    // exactly like the old unconditional Sequence.  Do not touch
    // `gremlin.loop_iter` — only repeating sequences push iteration scopes.
    if max_iterations == 1 {
        if !skip_guard.is_empty() {
            let resolved = skip_guard.replace("{loop_iter}", &gremlin.loop_iter);
            if is_registered_uri(gremlin.state.store_ref(), &resolved).await {
                send_log(
                    &gremlin.runtime_config.log_tx,
                    format!("sequence '{}': skipped (artifact exists)", seq.name),
                );
                return Ok(());
            }
        }
        for child in &seq.stages {
            Box::pin(run_stage_scoped(child, gremlin, &key, enclosing_client)).await?;
        }
        return Ok(());
    }

    // Repeating: iteration loop with guard checks before and after the body.
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

        // Guard before body: skip on iteration 1, stop on later iterations.
        if !skip_guard.is_empty() {
            let resolved = skip_guard.replace("{loop_iter}", &loop_iter);
            if is_registered_uri(gremlin.state.store_ref(), &resolved).await {
                if iteration == 1 {
                    send_log(
                        &gremlin.runtime_config.log_tx,
                        format!("sequence '{}': skipped (artifact exists)", seq.name),
                    );
                    gremlin.loop_iter = saved_loop_iter;
                    return Ok(());
                }
                send_log(
                    &gremlin.runtime_config.log_tx,
                    format!("sequence '{}': stopped (artifact exists)", seq.name),
                );
                stopped = true;
                break;
            }
        }

        send_log(
            &gremlin.runtime_config.log_tx,
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
                enclosing_client,
            ))
            .await
            {
                outcome = Err(error);
                gremlin.loop_iter = saved_loop_iter;
                return outcome;
            }
        }

        // Guard after body: an artifact produced this iteration stops the
        // loop immediately (checked with the *current* iteration scope).
        if !skip_guard.is_empty() {
            let resolved = skip_guard.replace("{loop_iter}", &loop_iter);
            if is_registered_uri(gremlin.state.store_ref(), &resolved).await {
                send_log(
                    &gremlin.runtime_config.log_tx,
                    format!("sequence '{}': stopped (artifact produced)", seq.name),
                );
                stopped = true;
                break;
            }
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
    /// Drive every stage from the definition to the end, returning the exit code.
    ///
    /// `resume_from` names a stage to resume from; pass `None` for a fresh start.
    ///
    /// Mirrors the Python `run_definition` walk: a bail records `bail_<attempt>.json`
    /// and yields exit code 1, any other failure is recorded and propagated, and
    /// the terminal state is written either way so `status` and the `finished`
    /// marker always agree with what actually happened.
    pub async fn run(&mut self, resume_from: Option<&str>) -> Result<i32, RunError> {
        // Loading the definition, building the registry, creating the client and
        // resolving the environment are all deferred out of the constructors so
        // that a handle nobody runs is cheap. This is the one place they happen
        // — and the one place a missing definition becomes a hard error.
        self.init_runtime(resume_from).await?;

        // A first start owes its checkout a bootstrap before any stage can use
        // it. The Python guard was `worktree_dir and not resume_from and
        // _has_bootstrap`; a run without a worktree has no dev environment to
        // prepare, and a resumed run's was prepared by the attempt that made
        // the worktree.
        let bootstrap = self.definition.bootstrap();
        let is_fork = !self.state.read_str("parent_id").is_empty();
        let has_bootstrap = !bootstrap.cmds.is_empty()
            || (!is_fork && (!bootstrap.launch_cmds.is_empty() || !bootstrap.cli_out.is_empty()));
        let first_start = self.workdir.is_some() && resume_from.is_none();
        if first_start && has_bootstrap {
            if let Err(error) = run_definition_bootstrap(self, is_fork).await {
                send_log(&self.runtime_config.log_tx, "bootstrap failed".to_string());
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

            // Check for cancellation before each stage.
            if let Some(ref cancel_token) = self.cancel_token {
                if cancel_token.is_cancelled() {
                    send_log(&self.runtime_config.log_tx, "cancelled".to_string());
                    self.finish(-1);
                    return Ok(-1);
                }
            }

            self.state.set_stage(stage.name(), None, "");

            // Reuse the existing attempt on resume so that artifact scopes —
            // and therefore done markers in run_parallel — are stable across
            // retries. A fresh token is generated only when the state has no
            // prior attempt for this stage.
            //
            // Stale bail files from the previous run are removed when the
            // attempt is reused so that stage_error (checked after the
            // stage runs) does not spuriously flag the resumed run as bailed.
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

            // Update loop_iter to include the per-stage attempt so artifact
            // namespaces are distinct across retries — the same pattern
            // run_sequence already applies for nested sequences.
            let saved_loop_iter = self.loop_iter.clone();
            self.loop_iter = format!("{}~{}", saved_loop_iter, attempt);

            send_log(
                &self.runtime_config.log_tx,
                format!(
                    "stage '{}': starting (type={})",
                    stage.name(),
                    stage.stage_type()
                ),
            );

            let stage_result = run_stage(&stage, self).await;
            // Restore loop_iter before any early exit so the next stage
            // (or the enclosing loop) sees the original base value, not
            // the per-attempt decorated form.
            self.loop_iter = saved_loop_iter;

            match stage_result {
                Ok(()) => {}
                Err(RunError::Bail { reason }) => {
                    self.state
                        .record_stage_error("other", &truncate(&reason, 200));
                    exit_code = 1;
                    break;
                }
                Err(error) => {
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

    /// Reap the clients, fold subprocess spend into the run's total cost, and
    /// write the terminal state — the bookkeeping every exit path owes the
    /// operator, whether the run succeeded, bailed, or blew up.
    fn finish(&mut self, exit_code: i32) {
        send_log(
            &self.runtime_config.log_tx,
            format!("finished (exit_code={})", exit_code),
        );
        self.client.reap_all(self.id.as_str());
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

    use crate::config;

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

    /// A gremlin with no git, no worktree, and a seeded state directory: the
    /// smallest thing `run_stage` needs to dispatch a stage.
    fn test_gremlin(stages: Vec<StageSpec>, default_client: &str) -> (Sandbox, Gremlin) {
        test_gremlin_with_bootstrap(stages, default_client, Bootstrap::default())
    }

    fn test_gremlin_with_bootstrap(
        stages: Vec<StageSpec>,
        default_client: &str,
        bootstrap: Bootstrap,
    ) -> (Sandbox, Gremlin) {
        test_gremlin_full(stages, None, default_client, bootstrap)
    }

    fn test_gremlin_full(
        stages: Vec<StageSpec>,
        land: Option<StageSpec>,
        default_client: &str,
        bootstrap: Bootstrap,
    ) -> (Sandbox, Gremlin) {
        let sandbox = Sandbox::new();
        let state_dir = config::state_root().join("gr-test");
        let artifact_dir = state_dir.join("artifacts");
        std::fs::create_dir_all(&artifact_dir).unwrap();
        std::fs::create_dir_all(&state_dir).unwrap();

        let data = serde_json::json!({
            "id": "gr-test",
            "attempt": "test-0001",
            "stage": "starting",
            "client": default_client,
        });
        state::write_state(&state_dir, data.as_object().unwrap()).unwrap();

        let state_data = StateData::open("gr-test").unwrap();

        let definition = StaticDefinition::new(
            "test".to_string(),
            PathBuf::from("test.yaml"),
            default_client.to_string(),
            bootstrap,
            stages,
            land,
            vec![],
            serde_yaml::Value::Null,
        );

        let gremlin = Gremlin {
            id: validate_gremlin_id("gr-test").unwrap(),
            definition_path: None,
            client_override: None,
            definition: Box::new(definition),
            workdir: None,
            project_root: sandbox.path().to_path_buf(),
            state: state_data,
            env: HashMap::new(),
            client: Client::parse(default_client).unwrap(),
            loop_iter: "1".to_string(),
            stage_inputs: HashMap::new(),
            runtime_config: crate::executor::gremlin::RuntimeConfig::snapshot(),
            cancel_token: None,
            interactive_session: None,
            scratch_dir: ScratchDir::Persistent(config::scratch_root(Some("gr-test"))),
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
        let (_sandbox, gremlin) = test_gremlin(vec![], "cmd:true");
        assert_eq!(gremlin.loop_iter, "1");
    }

    #[test]
    fn truncate_cuts_on_a_char_boundary() {
        assert_eq!(truncate("short", 200), "short");
        assert_eq!(truncate("abcdef", 3), "abc");
        // "é" is two bytes, so a naive slice at 3 would split it.
        assert_eq!(truncate("aé", 2), "a");
    }

    // --- agent ---

    #[tokio::test]
    async fn agent_skips_when_the_guard_artifact_is_live() {
        let stages = vec![SequenceBuilder::new("writer")
            .skip_if_exists("artifact://done.md")
            .stage(
                AgentBuilder::new("writer-inner")
                    .client("cmd:false")
                    .prompt("hi")
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages, "cmd:true");
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
                    .client("cmd:false")
                    .prompt("hi")
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages, "cmd:true");
        gremlin
            .state
            .write_into_registry(&Uri::parse("artifact://done.md").unwrap(), "done")
            .await
            .unwrap();

        let stage = take_first_stage(&mut gremlin).await;
        // `cmd:false` would fail the stage if the guard did not skip it.
        assert!(run_stage(&stage, &mut gremlin).await.is_ok());
    }

    #[tokio::test]
    async fn agent_commits_a_produced_bound_artifact() {
        let stages = vec![AgentBuilder::new("writer")
            // The cmd backend appends --model <model> --add-dir <artifact_dir>
            // after the command, so $3 is the artifact_dir.
            .client("cmd:sh -c 'touch \"$3\"/writer.md'")
            .output("out?", output("artifact://{name}.md"))
            .prompt("write {out}")
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages, "cmd:true");
        let stage = take_first_stage(&mut gremlin).await;
        run_stage(&stage, &mut gremlin).await.unwrap();
        assert!(gremlin.state.is_registered("artifact://writer.md").await);
    }

    #[tokio::test]
    async fn agent_missing_artifact_bails() {
        let stages = vec![AgentBuilder::new("writer")
            .client("cmd:sh -c 'cat >/dev/null'")
            .output("out", output("artifact://out.md"))
            .prompt("write {out}")
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages, "cmd:true");
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
        let (_sandbox, mut gremlin) = test_gremlin(stages, "cmd:true");
        let stage = take_first_stage(&mut gremlin).await;
        assert!(run_stage(&stage, &mut gremlin).await.is_ok());
    }

    #[tokio::test]
    async fn exec_non_zero_exit_fails_the_stage() {
        let stages = vec![ExecBuilder::new("broken")
            .cmds(vec!["gremlins-nonexistent-cmd-xyz".to_string()])
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages, "cmd:true");
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
        let (sandbox, mut gremlin) = test_gremlin(stages, "cmd:true");
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
        let (_sandbox, mut gremlin) = test_gremlin(stages, "cmd:true");
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
        let (_sandbox, mut gremlin) = test_gremlin(stages, "cmd:true");
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
        let (_sandbox, mut gremlin) = test_gremlin(stages, "cmd:true");
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
        let (_sandbox, mut gremlin) = test_gremlin(stages, "cmd:true");
        let stage = take_first_stage(&mut gremlin).await;

        match run_stage(&stage, &mut gremlin).await {
            Err(RunError::StageFailed { stage, .. }) => assert_eq!(stage, "bad"),
            other => panic!("expected StageFailed, got {other:?}"),
        }
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
        let (_sandbox, mut gremlin) = test_gremlin(stages, "cmd:true");
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
        let (_sandbox, mut gremlin) = test_gremlin(stages, "cmd:true");
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
        let (_sandbox, mut gremlin) = test_gremlin(stages, "cmd:true");
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
        let (_sandbox, mut gremlin) = test_gremlin(stages, "cmd:true");
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
        let (sandbox, mut gremlin) = test_gremlin(stages, "cmd:true");
        let state_dir = sandbox.path().join("state").join("gr-test");

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
            .client("cmd:sh -c 'cat >/dev/null'")
            .output("out", output("artifact://out.md"))
            .prompt("hi {out}")
            .build()
            .unwrap()];
        let (sandbox, mut gremlin) = test_gremlin(stages, "cmd:true");
        let state_dir = sandbox.path().join("state").join("gr-test");

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
        let (sandbox, mut gremlin) = test_gremlin(stages, "cmd:true");

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
        let (sandbox, mut gremlin) = test_gremlin(stages, "cmd:true");
        let state_dir = sandbox.path().join("state").join("gr-test");

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

        // The successful child "good" has a done marker in the registry. The
        // scope embeds the attempt, so the marker URI is e.g.
        // artifact://1~group-xxxx/group/done/good.
        let scope = stage_key(&format!("1~{first_attempt}"), "group");
        let good_done_uri = format!("artifact://{scope}/done/good");
        assert!(
            gremlin.state.is_registered(&good_done_uri).await,
            "good child should be marked done at {good_done_uri}"
        );

        // Clear terminal markers so run() treats this as a resumable run.
        // `finished` file and `exit_code` must be removed; `status` reset.
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

        // Resume: "bad" fails again, but the attempt is reused so "good" is
        // skipped rather than re-run.
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

        // The good child's done marker must still be present under the
        // original scope — confirming it was skipped, not overwritten.
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
        let (sandbox, mut gremlin) = test_gremlin(stages, "cmd:true");
        let state_dir = sandbox.path().join("state").join("gr-test");

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

    /// The run-loop half of bootstrap: `run` runs the block before the stage
    /// walk, and a failure is a bail — recorded and terminal, never a crash.
    #[tokio::test]
    async fn run_fails_the_bootstrap_and_records_a_bail() {
        let stages = vec![ExecBuilder::new("never")
            .cmds(vec!["touch never.marker".to_string()])
            .build()
            .unwrap()];
        let (sandbox, mut gremlin) = test_gremlin_with_bootstrap(
            stages,
            "cmd:true",
            Bootstrap {
                cmds: vec!["exit 5".to_string()],
                ..Default::default()
            },
        );
        let worktree = sandbox.path().join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        gremlin.workdir = Some(WorkDir::Persistent(worktree.clone()));
        let state_dir = sandbox.path().join("state").join("gr-test");

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
            "cmd:true",
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
            "cmd:true",
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
    // The shared env guard holds a plain `std::sync::Mutex` across the run's
    // awaits. That is safe here: `#[tokio::test]` drives a current-thread
    // runtime, so no other task can be scheduled on this thread while the lock
    // is held, and the lock exists precisely to keep the sandbox override from
    // being observed half-swapped by another test.
    #[allow(clippy::await_holding_lock)]
    async fn create_then_run_end_to_end() {
        let fx = GitSandbox::with_definition(
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
        if fx.is_skipped() {
            eprintln!("git unavailable; skipping create_then_run_end_to_end");
            return;
        }

        let definition =
            StaticDefinition::from_yaml_file(fx.definition_path(), None, None).unwrap();
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
        // Read back through the handle's own state dir. The sandbox
        // override is shared process state, and the pre-existing
        // config tests clear it for their own duration; the path the
        // launch actually resolved is the honest one to assert on.
        let state_file = gremlin.state.state_dir().join("state.json");
        let raw: Value =
            serde_json::from_str(&std::fs::read_to_string(&state_file).unwrap()).unwrap();
        assert_eq!(code, 0);
        assert_eq!(raw["status"], "done");
        assert_eq!(raw["exit_code"], 0);
    }
}
