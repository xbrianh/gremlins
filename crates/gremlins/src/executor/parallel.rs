//! The parallel fan-out / fan-in executor.
//!
//! [`run_parallel`] destructures a [`StageSpec::Parallel`], guards against
//! already-completed groups, forks one child gremlin per stage via
//! [`Gremlin::fork`], launches each through [`supervisor::launch_child`] so
//! they appear in `gremlins ls` with live status, awaits completion via
//! [`watch::Receiver<RunState>`] channels, applies the group's
//! [`ErrorPolicy`], merges artifacts from successful children into the parent
//! registry, cleans up child worktrees (best-effort), and aggregates child
//! costs into the parent.
//!
//! Children are first-class gremlins: the supervisor owns their lifecycle,
//! they can be inspected and stopped like any other gremlin, and the
//! "orphan" display in `gremlins ls` is gone.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use futures::stream::{FuturesUnordered, StreamExt};
use tokio::sync::watch;

use crate::artifacts::uri::Uri;
use crate::core::proc::{run_logged_commands, sanitize_log_filename};
use crate::definition::ErrorPolicy;
use crate::definition::{ExecutorStage, GremlinDefinition};
use crate::executor::gremlin::Gremlin;
use crate::executor::run::stage_key;
use crate::executor::state::{BlobMode, StateStore};
use crate::executor::supervisor::{self, LaunchResult, RunState};
use crate::executor::RunError;

/// The registry URI marking `child_name` as done under `scope`.
fn done_uri(scope: &str, child_name: &str) -> String {
    format!("artifact://{scope}/done/{child_name}")
}

/// Record `child_name` as done under `scope` in the artifact registry.
async fn mark_child_done(state: &dyn StateStore, scope: &str, child_name: &str) {
    let uri_str = done_uri(scope, child_name);
    match Uri::parse(&uri_str) {
        Ok(uri) => {
            if let Err(e) = state.write_into_registry(&uri, "").await {
                log::warn!("parallel group: failed to mark {child_name} done at {uri_str}: {e}");
            }
        }
        Err(e) => log::warn!("parallel group: invalid done URI {uri_str}: {e}"),
    }
}

/// Run a parallel group: fork one child per stage, launch via supervisor,
/// await completion, and apply the error policy.
pub(crate) async fn run_parallel(
    stage: &ExecutorStage,
    gremlin: &mut Gremlin,
    enclosing_client: Option<&str>,
) -> Result<(), RunError> {
    let ExecutorStage::Parallel {
        name: group_name,
        max_concurrent,
        cancel_on_error,
        error_policy,
        client,
        children,
        skip_if_exists: _,
        fork,
        join,
    } = stage
    else {
        unreachable!("run_parallel is only called for parallel stages")
    };

    let total_children = children.len();

    log::debug!(
        "parallel group {group_name}: starting with {total_children} children (max_concurrent={max_concurrent:?}, cancel_on_error={cancel_on_error}, error_policy={error_policy:?})"
    );

    // --- Resumption guard ---
    let scope = stage_key(&gremlin.loop_iter, group_name);
    let mut done: HashSet<String> = HashSet::new();
    for child in children {
        let child_name = child.first_stage_name().to_string();
        if gremlin
            .state
            .is_registered(&done_uri(&scope, &child_name))
            .await
        {
            done.insert(child_name);
        }
    }
    if done.len() == total_children && total_children > 0 {
        log::info!(
            "parallel group {group_name}: all {total_children} children already done — skipping"
        );
        return Ok(());
    }

    // --- Concurrency bound ---
    let semaphore: Option<Arc<tokio::sync::Semaphore>> = max_concurrent
        .filter(|&n| n > 0)
        .map(|n| Arc::new(tokio::sync::Semaphore::new(n as usize)));

    // --- Fork and launch children ---
    //
    // Each child is forked (artifacts copied, worktree branched, state
    // written) then launched via supervisor::launch_child, which registers
    // it in the run_map and spawns its tokio task. The semaphore permit is
    // acquired inside the spawned monitoring task so the launch loop never
    // blocks — max_concurrent bounds in-flight children without deadlocking
    // the parent.
    struct LaunchedChild {
        child_name: String,
        child_id: String,
        state_rx: watch::Receiver<RunState>,
        gremlin_rx: tokio::sync::oneshot::Receiver<Gremlin>,
    }

    let mut launched: Vec<LaunchedChild> = Vec::with_capacity(children.len());

    for child in children {
        let enclosing_spec = enclosing_client.map(|c| crate::definition::ClientSpec(c.to_string()));
        let effective_client: Option<&str> = client
            .as_ref()
            .or(enclosing_spec.as_ref())
            .map(|c| c.0.as_str());

        let child_name = child.first_stage_name().to_string();

        // Skip children already marked done.
        if done.contains(&child_name) {
            log::info!("parallel group {group_name}: child {child_name} already done — skipping");
            continue;
        }

        let child_id = format!("{}-{}", gremlin.id.as_str(), child_name);
        let parent_id = gremlin.id.to_string();
        let group_name_owned = group_name.to_string();

        log::debug!(
            "parallel group {group_name}: forking child {child_name} (child_id={child_id})"
        );

        // Fork the child gremlin.
        let child_gremlin = gremlin
            .fork(
                &child_id,
                &parent_id,
                &group_name_owned,
                &child_name,
                None,
                child.clone_box(),
                effective_client,
            )
            .await?;

        // Populate the child workspace (fork commands run after fork()
        // completes). On failure the child state has already been
        // persisted as `running` by fork(), so record a terminal failure
        // and clean the partial workspace before propagating the error —
        // otherwise no child task ever finalizes the state and the
        // workspace is stranded.
        if let Err(error) = gremlin
            .run_fork_cmds(
                &child_id,
                &child_name,
                &parent_id,
                &group_name_owned,
                child_gremlin.workdir.as_ref().unwrap().path(),
                fork.as_ref().map(|f| f.cmds.as_slice()),
            )
            .await
        {
            child_gremlin.state.write_terminal_state(1);
            child_gremlin.clean(false).await;
            return Err(error);
        }

        log::debug!(
            "parallel group {group_name}: child {child_name} forked (state_dir={}, artifact_dir={})",
            child_gremlin.state.state_dir().display(),
            child_gremlin.state.artifact_dir().display()
        );

        // Launch the child via the supervisor — it becomes a first-class
        // gremlin visible in `gremlins ls`.
        let LaunchResult {
            state_rx,
            gremlin_rx,
        } = supervisor::launch_child(child_gremlin);

        launched.push(LaunchedChild {
            child_name,
            child_id,
            state_rx,
            gremlin_rx,
        });

        log::debug!("parallel group {group_name}: launched child via supervisor");
    }

    if launched.is_empty() {
        return Ok(());
    }

    // --- Await completion ---
    //
    // Each child has a watch::Receiver<RunState> that fires when the child
    // reaches a terminal state. We collect them via FuturesUnordered.
    let mut child_results: Vec<ChildOutcome> = Vec::with_capacity(launched.len());
    let mut first_error: Option<RunError> = None;

    // Indexed by position in `launched` — O(1) lookup for cancel_on_error.
    let child_ids: Vec<String> = launched.iter().map(|lc| lc.child_id.clone()).collect();

    // Track which children are still running (for cancel_on_error).
    let mut running: HashSet<usize> = (0..launched.len()).collect();

    // Spawn a future for each child that waits for its state_rx to change
    // to a terminal status, then reads the child's state.json to determine
    // the real outcome (exit code, error message).
    type ChildResult = (usize, String, String, Result<(), RunError>, Option<Gremlin>);
    let mut futs: FuturesUnordered<tokio::task::JoinHandle<ChildResult>> = FuturesUnordered::new();

    for (idx, lc) in launched.into_iter().enumerate() {
        let child_name = lc.child_name.clone();
        let child_id = lc.child_id.clone();
        let mut state_rx = lc.state_rx;
        let gremlin_rx = lc.gremlin_rx;
        let sem = semaphore.clone();
        let handle = tokio::spawn(async move {
            // Acquire semaphore permit inside the spawned task so the
            // launch loop never blocks on max_concurrent.
            let _permit = match sem {
                Some(s) => Some(s.acquire_owned().await.expect("semaphore closed")),
                None => None,
            };
            // Wait for the state to change to a terminal status.
            loop {
                match state_rx.changed().await {
                    Ok(()) => {
                        let state = state_rx.borrow().clone();
                        if state.status == "done" || state.status == "stopped" {
                            // Receive the gremlin back from the child task so
                            // we can use it for post-processing without
                            // reopening via config::state_root().
                            let gremlin = gremlin_rx.await.ok();
                            // Read the child's state.json to get the real outcome.
                            let outcome = match &gremlin {
                                Some(g) => {
                                    let raw = g.state.state_tree();
                                    let exit_code =
                                        raw.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(1);
                                    if exit_code == 0 {
                                        Ok(())
                                    } else {
                                        // Try to get a meaningful error from the bail file or state.
                                        let reason = raw
                                            .get("stage")
                                            .and_then(|v| v.as_str())
                                            .filter(|s| !s.is_empty() && *s != "starting")
                                            .map(|s| format!("stage {s}: exited {exit_code}"))
                                            .unwrap_or_else(|| format!("exited {exit_code}"));
                                        Err(RunError::StageFailed {
                                            stage: child_name.clone(),
                                            message: reason,
                                        })
                                    }
                                }
                                None => Err(RunError::Message(format!(
                                    "child {child_name} state file not found"
                                ))),
                            };
                            return (idx, child_name, child_id, outcome, gremlin);
                        }
                    }
                    Err(_) => {
                        // The sender dropped — the child task panicked or was
                        // cancelled before sending a terminal state. Treat as
                        // a failure so the parent doesn't hang.
                        return (
                            idx,
                            child_name.clone(),
                            child_id,
                            Err(RunError::Message(format!(
                                "child {child_name} terminated without reporting status"
                            ))),
                            None,
                        );
                    }
                }
            }
        });

        futs.push(handle);
    }

    while let Some(result) = futs.next().await {
        match result {
            Ok((idx, child_name, _child_id, outcome, gremlin)) => {
                running.remove(&idx);
                log::debug!(
                    "parallel group {group_name}: child {child_name} completed (outcome={})",
                    match &outcome {
                        Ok(()) => "Ok".to_string(),
                        Err(e) => format!("Err: {e}"),
                    }
                );

                match outcome {
                    Ok(()) => {
                        child_results.push(ChildOutcome {
                            child_name,
                            outcome: Ok(()),
                            gremlin,
                        });
                    }
                    Err(err) => {
                        if *cancel_on_error && first_error.is_none() {
                            first_error = Some(RunError::StageFailed {
                                stage: child_name.clone(),
                                message: err.to_string(),
                            });
                            // Cancel all remaining running children — O(1)
                            // lookup via the child_ids vec.
                            for &running_idx in &running {
                                if let Some(rid) = child_ids.get(running_idx) {
                                    supervisor::stop_child(rid).await;
                                }
                            }
                            child_results.push(ChildOutcome {
                                child_name,
                                outcome: Err(err),
                                gremlin,
                            });
                        } else {
                            child_results.push(ChildOutcome {
                                child_name,
                                outcome: Err(err),
                                gremlin,
                            });
                        }
                    }
                }
            }
            Err(join_error) => {
                if join_error.is_panic() {
                    let msg =
                        format!("parallel group {group_name}: a child task panicked: {join_error}");
                    log::error!("{msg}");
                    if first_error.is_none() {
                        first_error = Some(RunError::Message(msg));
                    }
                }
            }
        }
    }

    // --- Apply error policy ---
    let mut failed_names: HashSet<String> = HashSet::new();
    let mut real_errors: Vec<RunError> = Vec::new();
    let mut success_count = done.len();

    for outcome in &mut child_results {
        let outcome_ok = outcome.outcome.is_ok();
        if outcome_ok {
            success_count += 1;
        } else {
            failed_names.insert(outcome.child_name.clone());
            let err = std::mem::replace(&mut outcome.outcome, Ok(())).unwrap_err();
            real_errors.push(err);
        }
        log::debug!(
            "parallel group {group_name}: child {} recorded as {} (success_count={}, failed_names={:?})",
            outcome.child_name,
            if outcome_ok { "success" } else { "failure" },
            success_count,
            failed_names
        );
    }

    log::debug!(
        "parallel group {group_name}: applying error_policy={error_policy:?} (success_count={success_count}, failed_count={}, errors={:?})",
        real_errors.len(),
        real_errors.iter().map(|e| e.to_string()).collect::<Vec<_>>()
    );

    let group_error = match *error_policy {
        ErrorPolicy::Any => real_errors.into_iter().next(),
        ErrorPolicy::All => {
            if success_count == 0 && !real_errors.is_empty() {
                Some(real_errors.into_iter().next().unwrap())
            } else {
                None
            }
        }
    };

    let group_error = first_error.or(group_error);

    log::debug!(
        "parallel group {group_name}: group_error={:?}",
        group_error.as_ref().map(|e| e.to_string())
    );

    // --- Run join commands for each child (best-effort) ---
    if let Some(ref join_spec) = join {
        let parent_workdir = gremlin
            .workdir
            .as_ref()
            .map(|w| w.path().to_string_lossy().into_owned())
            .unwrap_or_default();
        let join_cwd = if parent_workdir.is_empty() {
            gremlin.project_root.clone()
        } else {
            PathBuf::from(&parent_workdir)
        };
        let cmd_strings: Vec<String> = join_spec.cmds.clone();
        for outcome in &child_results {
            let child_workdir = match &outcome.gremlin {
                Some(g) => g
                    .workdir
                    .as_ref()
                    .map(|w| w.path().to_string_lossy().into_owned())
                    .unwrap_or_default(),
                None => continue,
            };
            if child_workdir.is_empty() {
                continue;
            }
            let child_name = &outcome.child_name;

            let log_name = format!("join-{child_name}");
            let safe_name = sanitize_log_filename(child_name);
            let blob_name = format!("command_logs/join-{safe_name}.log");
            let log_writer = gremlin
                .state
                .open_blob(&blob_name, BlobMode::Append)
                .ok()
                .map(|b| b as Box<dyn std::io::Write + Send>);

            let mut env: HashMap<String, String> = gremlin.env.clone();
            env.insert("GREMLIN_WORKDIR".to_string(), parent_workdir.clone());
            env.insert("GREMLIN_FORK_WORKDIR".to_string(), child_workdir.clone());
            let empty_subs = HashMap::new();
            let log_tx: Option<tokio::sync::mpsc::UnboundedSender<String>> = None;

            let result = run_logged_commands(
                &log_name,
                &cmd_strings,
                &join_cwd,
                &env,
                &empty_subs,
                None,
                log_writer,
                &log_tx,
            )
            .await;

            match result {
                Ok(r) if r.rc == 0 => {}
                Ok(r) => {
                    log::warn!(
                        "parallel group {group_name}: join command(s) for child {child_name} exited {}: {}",
                        r.rc,
                        crate::executor::run::truncate(&r.output, 500),
                    );
                }
                Err(e) => {
                    log::warn!(
                        "parallel group {group_name}: join command(s) for child {child_name} failed: {e}",
                    );
                }
            }
        }
    }

    // --- Merge artifacts from successful children ---
    log::debug!(
        "parallel group {group_name}: merging artifacts from {} successful children",
        child_results.len() - failed_names.len()
    );
    for outcome in &child_results {
        if !failed_names.contains(&outcome.child_name) {
            if let Err(e) = merge_child_artifacts(gremlin, outcome).await {
                log::warn!(
                    "parallel group {group_name}: failed to merge artifacts from {}: {e}",
                    outcome.child_name
                );
            }
        }
    }

    // --- Aggregate child costs ---
    log::debug!(
        "parallel group {group_name}: aggregating costs from {} children",
        child_results.len()
    );
    for outcome in &child_results {
        aggregate_child_costs(gremlin, outcome);
    }

    // --- Mark children done ---
    for outcome in &child_results {
        if !failed_names.contains(&outcome.child_name) {
            mark_child_done(gremlin.state.store_ref(), &scope, &outcome.child_name).await;
        }
    }

    // --- Clean up children (best-effort) ---
    //
    // On success: remove everything (state, scratch, workspace).
    // On failure: only remove workspace (preserve state for forensics).
    // The pipeline author handles workspace cleanup in join.cmds; here we
    // clean up workspaces that join didn't handle.
    log::debug!(
        "parallel group {group_name}: cleaning up {} children",
        child_results.len()
    );
    for outcome in child_results {
        let child_name = &outcome.child_name;
        if group_error.is_none() {
            cleanup_child_fully(child_name, outcome.gremlin).await;
        } else {
            cleanup_child_workspace(child_name, outcome.gremlin).await;
        }
    }

    match group_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// The result of one child gremlin's run.
struct ChildOutcome {
    child_name: String,
    outcome: Result<(), RunError>,
    gremlin: Option<Gremlin>,
}

/// Merge artifacts from a successful child into the parent registry.
async fn merge_child_artifacts(
    gremlin: &mut Gremlin,
    outcome: &ChildOutcome,
) -> Result<(), RunError> {
    use crate::executor::state::Collision;

    let child_gremlin = outcome.gremlin.as_ref().ok_or_else(|| {
        RunError::Message(format!(
            "child {} gremlin not available",
            outcome.child_name
        ))
    })?;
    gremlin
        .state
        .join(
            child_gremlin.state.store_ref(),
            Collision::Ignore,
            Some(&outcome.child_name),
        )
        .await
        .map_err(|e| RunError::Message(format!("artifact merge failed: {e}")))?;

    Ok(())
}

/// Aggregate token usage and subprocess cost from a child into the parent.
fn aggregate_child_costs(gremlin: &mut Gremlin, outcome: &ChildOutcome) {
    let child_gremlin = match &outcome.gremlin {
        Some(g) => g,
        None => return,
    };
    let tree = child_gremlin.state.state_tree();
    if tree.is_empty() {
        return;
    }

    // Token usage.
    if let Some(usage) = tree.get("token_usage").and_then(|v| v.as_object()) {
        let mut delta = HashMap::new();
        for (key, value) in usage {
            if let Some(n) = value.as_i64() {
                delta.insert(key.clone(), n);
            }
        }
        gremlin.state.accumulate_token_usage(&delta);
    }

    // Subprocess cost.
    if let Some(cost) = tree.get("subprocess_cost_usd") {
        if let Some(n) = cost.as_f64() {
            gremlin.state.add_subprocess_cost(n);
        }
    }
}

/// Remove everything a child owns (state, scratch, workspace).
async fn cleanup_child_fully(child_name: &str, gremlin: Option<Gremlin>) {
    match gremlin {
        Some(child) => child.clean(true).await,
        None => {
            log::debug!("parallel group: child {child_name} left nothing to clean (no gremlin)")
        }
    }
}

/// Remove only the child's workspace, preserving state for forensics.
async fn cleanup_child_workspace(child_name: &str, gremlin: Option<Gremlin>) {
    match gremlin {
        Some(child) => child.clean(false).await,
        None => {
            log::debug!("parallel group: child {child_name} left nothing to clean (no gremlin)")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builders::artifacts::output;
    use crate::builders::composite::ParallelBuilder;
    use crate::builders::exec::ExecBuilder;

    use crate::definition::StageSpec;
    use crate::definition::{ExecutorStage, StaticDefinition};
    use crate::executor::gremlin::{validate_gremlin_id, RuntimeConfig, ScratchDir};
    use crate::executor::state::{self, StateData};
    use crate::schemas::bootstrap::Bootstrap;
    use crate::test_support::Sandbox;

    /// Convert the first parsed stage to an ExecutorStage for dispatch.
    fn first_executor_stage(stages: &[StageSpec]) -> ExecutorStage {
        let def = StaticDefinition::new(
            "test".to_string(),
            PathBuf::from("test.yaml"),
            "cmd:true".to_string(),
            Bootstrap::default(),
            vec![],
            None,
            vec![],
            serde_yaml::Value::Null,
        );
        def.convert_stage(stages[0].clone())
    }
    use crate::config;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn test_gremlin(stages: Vec<StageSpec>, default_client: &str) -> (Sandbox, Gremlin) {
        let sandbox = Sandbox::new();
        let state_dir = config::state_root().join("gr-test");
        std::fs::create_dir_all(&state_dir).unwrap();

        let data = serde_json::json!({
            "id": "gr-test",
            "attempt": "test-0001",
            "stage": "starting",
            "client": default_client,
        });
        state::write_state(&state_dir, data.as_object().unwrap()).unwrap();

        let state_data = StateData::open("gr-test").unwrap();

        let gremlin = Gremlin {
            id: validate_gremlin_id("gr-test").unwrap(),
            definition_path: None,
            client_override: None,
            definition: Box::new(StaticDefinition::new(
                "test".to_string(),
                PathBuf::from("test.yaml"),
                default_client.to_string(),
                Bootstrap::default(),
                stages.clone(),
                None,
                vec![],
                serde_yaml::Value::Null,
            )),
            workdir: None,
            project_root: sandbox.path().to_path_buf(),
            state: state_data,
            env: HashMap::new(),
            client: crate::clients::client::Client::parse(default_client).unwrap(),
            loop_iter: "1".to_string(),
            stage_inputs: HashMap::new(),
            runtime_config: RuntimeConfig::snapshot(),
            cancel_token: None,
            interactive_session: None,
            scratch_dir: ScratchDir::Persistent(config::scratch_root(Some("gr-test"))),
        };
        (sandbox, gremlin)
    }

    // --- Empty group ---

    #[tokio::test]
    async fn empty_parallel_group_succeeds() {
        let stage = ExecutorStage::Parallel {
            name: "empty".to_string(),
            max_concurrent: None,
            cancel_on_error: false,
            error_policy: ErrorPolicy::Any,
            client: None,
            children: vec![],
            skip_if_exists: String::new(),
            fork: None,
            join: None,
        };
        let (_sandbox, mut gremlin) = test_gremlin(vec![], "cmd:true");
        let result = run_parallel(&stage, &mut gremlin, None).await;
        assert!(result.is_ok());
    }

    // --- Single child success ---

    #[tokio::test]
    async fn single_child_succeeds() {
        let stages = vec![ParallelBuilder::new("group")
            .stage(
                ExecBuilder::new("a")
                    .cmds(vec!["true".to_string()])
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages.clone(), "cmd:true");
        let stage = first_executor_stage(&stages);
        let result = run_parallel(&stage, &mut gremlin, None).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }

    // --- ErrorPolicy::Any ---

    #[tokio::test]
    async fn error_policy_any_bails_on_first_child_failure() {
        let stages = vec![ParallelBuilder::new("group")
            .error_policy(crate::definition::ErrorPolicy::Any)
            .stage(
                ExecBuilder::new("bad")
                    .cmds(vec!["gremlins-nonexistent-cmd-xyz".to_string()])
                    .build()
                    .unwrap(),
            )
            .stage(
                ExecBuilder::new("good")
                    .cmds(vec!["true".to_string()])
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages.clone(), "cmd:true");
        let stage = first_executor_stage(&stages);
        let result = run_parallel(&stage, &mut gremlin, None).await;
        match result {
            Err(RunError::StageFailed { stage, .. }) => {
                assert_eq!(stage, "bad");
            }
            other => panic!("expected StageFailed for 'bad', got {other:?}"),
        }
    }

    // --- ErrorPolicy::All ---

    #[tokio::test]
    async fn error_policy_all_succeeds_when_one_child_succeeds() {
        let stages = vec![ParallelBuilder::new("group")
            .error_policy(crate::definition::ErrorPolicy::All)
            .stage(
                ExecBuilder::new("bad")
                    .cmds(vec!["gremlins-nonexistent-cmd-xyz".to_string()])
                    .build()
                    .unwrap(),
            )
            .stage(
                ExecBuilder::new("good")
                    .cmds(vec!["true".to_string()])
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages.clone(), "cmd:true");
        let stage = first_executor_stage(&stages);
        let result = run_parallel(&stage, &mut gremlin, None).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }

    #[tokio::test]
    async fn error_policy_all_bails_when_every_child_fails() {
        let stages = vec![ParallelBuilder::new("group")
            .error_policy(crate::definition::ErrorPolicy::All)
            .stage(
                ExecBuilder::new("bad1")
                    .cmds(vec!["gremlins-nonexistent-cmd-xyz".to_string()])
                    .build()
                    .unwrap(),
            )
            .stage(
                ExecBuilder::new("bad2")
                    .cmds(vec!["gremlins-nonexistent-cmd-xyz".to_string()])
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages.clone(), "cmd:true");
        let stage = first_executor_stage(&stages);
        let result = run_parallel(&stage, &mut gremlin, None).await;
        match result {
            Err(RunError::StageFailed { .. }) => {}
            other => panic!("expected StageFailed, got {other:?}"),
        }
    }

    // --- cancel_on_error ---

    #[tokio::test]
    async fn cancel_on_error_aborts_siblings_on_first_failure() {
        let stages = vec![ParallelBuilder::new("group")
            .cancel_on_error(true)
            .error_policy(crate::definition::ErrorPolicy::Any)
            .max_concurrent(1)
            .stage(
                ExecBuilder::new("bad")
                    .cmds(vec!["gremlins-nonexistent-cmd-xyz".to_string()])
                    .build()
                    .unwrap(),
            )
            .stage(
                ExecBuilder::new("slow")
                    .cmds(vec!["sleep 1".to_string()])
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages.clone(), "cmd:true");
        let stage = first_executor_stage(&stages);
        let result = run_parallel(&stage, &mut gremlin, None).await;
        match result {
            Err(RunError::StageFailed { stage, .. }) => {
                assert_eq!(stage, "bad");
            }
            other => panic!("expected StageFailed for 'bad', got {other:?}"),
        }
    }

    // --- max_concurrent bounding ---

    #[tokio::test]
    async fn max_concurrent_bounds_concurrency() {
        let stages = vec![ParallelBuilder::new("group")
            .max_concurrent(1)
            .stage(
                ExecBuilder::new("a")
                    .cmds(vec!["sleep 0.1".to_string()])
                    .build()
                    .unwrap(),
            )
            .stage(
                ExecBuilder::new("b")
                    .cmds(vec!["sleep 0.1".to_string()])
                    .build()
                    .unwrap(),
            )
            .stage(
                ExecBuilder::new("c")
                    .cmds(vec!["sleep 0.1".to_string()])
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages.clone(), "cmd:true");
        let stage = first_executor_stage(&stages);
        let result = run_parallel(&stage, &mut gremlin, None).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }

    // --- Resumption after partial completion ---

    #[tokio::test]
    async fn resumption_skips_already_done_children() {
        let stages = vec![ParallelBuilder::new("group")
            .stage(
                ExecBuilder::new("a")
                    .cmds(vec!["true".to_string()])
                    .build()
                    .unwrap(),
            )
            .stage(
                ExecBuilder::new("b")
                    .cmds(vec!["true".to_string()])
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages.clone(), "cmd:true");

        let scope = stage_key(&gremlin.loop_iter, "group");
        mark_child_done(gremlin.state.store_ref(), &scope, "a").await;

        let stage = first_executor_stage(&stages);
        let result = run_parallel(&stage, &mut gremlin, None).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");

        assert!(gremlin.state.is_registered(&done_uri(&scope, "a")).await);
        assert!(gremlin.state.is_registered(&done_uri(&scope, "b")).await);
    }

    #[tokio::test]
    async fn fully_done_group_is_skipped_entirely() {
        let stages = vec![ParallelBuilder::new("group")
            .stage(
                ExecBuilder::new("a")
                    .cmds(vec!["gremlins-nonexistent-cmd-xyz".to_string()])
                    .build()
                    .unwrap(),
            )
            .stage(
                ExecBuilder::new("b")
                    .cmds(vec!["gremlins-nonexistent-cmd-xyz".to_string()])
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages.clone(), "cmd:true");

        let scope = stage_key(&gremlin.loop_iter, "group");
        mark_child_done(gremlin.state.store_ref(), &scope, "a").await;
        mark_child_done(gremlin.state.store_ref(), &scope, "b").await;

        let stage = first_executor_stage(&stages);
        let result = run_parallel(&stage, &mut gremlin, None).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }

    // --- Worktree lifecycle ---

    #[tokio::test]
    async fn child_worktrees_are_cleaned_up() {
        let stages = vec![ParallelBuilder::new("group")
            .stage(
                ExecBuilder::new("a")
                    .cmds(vec!["true".to_string()])
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages.clone(), "cmd:true");
        let stage = first_executor_stage(&stages);
        let result = run_parallel(&stage, &mut gremlin, None).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }

    // --- Artifact merge ---

    #[tokio::test]
    async fn child_artifacts_are_merged_into_parent() {
        let stages = vec![ParallelBuilder::new("group")
            .stage(
                ExecBuilder::new("writer")
                    .output("output", output("artifact://out.txt"))
                    .cmds(vec!["echo hello > {output}".to_string()])
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages.clone(), "cmd:true");

        let stage = first_executor_stage(&stages);
        let result = run_parallel(&stage, &mut gremlin, None).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");

        let merged_key = "writer/out.txt";
        let registered = gremlin
            .state
            .is_registered(&format!("artifact://{merged_key}"))
            .await;
        assert!(registered, "parent registry should contain {merged_key}");
    }

    // --- Cost aggregation ---

    #[tokio::test]
    async fn child_costs_are_aggregated() {
        let stages = vec![ParallelBuilder::new("group")
            .stage(
                ExecBuilder::new("a")
                    .cmds(vec!["true".to_string()])
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages.clone(), "cmd:true");
        let stage = first_executor_stage(&stages);
        let result = run_parallel(&stage, &mut gremlin, None).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }

    // --- Client inheritance ---

    #[tokio::test]
    async fn parallel_with_explicit_client_succeeds() {
        let stages = vec![ParallelBuilder::new("group")
            .client("cmd:true")
            .stage(
                ExecBuilder::new("a")
                    .cmds(vec!["true".to_string()])
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap()];
        let (_sandbox, mut gremlin) = test_gremlin(stages.clone(), "cmd:true");
        let stage = first_executor_stage(&stages);
        let result = run_parallel(&stage, &mut gremlin, None).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }
}
