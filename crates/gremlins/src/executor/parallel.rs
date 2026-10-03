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
use std::sync::Arc;

use futures::stream::{FuturesUnordered, StreamExt};
use tokio::sync::watch;

use crate::artifacts::registry::ArtifactRegistry;
use crate::artifacts::uri::Uri;
use crate::definition::ErrorPolicy;
use crate::definition::{ExecutorStage, GremlinDefinition};
use crate::executor::gremlin::Gremlin;
use crate::executor::run::stage_key;
use crate::executor::state;
use crate::executor::supervisor::{self, LaunchResult, RunState};
use crate::executor::RunError;

/// The registry URI marking `child_name` as done under `scope`.
fn done_uri(scope: &str, child_name: &str) -> String {
    format!("artifact://{scope}/done/{child_name}")
}

/// Record `child_name` as done under `scope` in the artifact registry.
async fn mark_child_done(registry: &dyn ArtifactRegistry, scope: &str, child_name: &str) {
    let uri_str = done_uri(scope, child_name);
    match Uri::parse(&uri_str) {
        Ok(uri) => {
            if let Err(e) = registry.write_into_registry(&uri, "").await {
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
            .registry
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
        registry_rx: tokio::sync::oneshot::Receiver<
            Option<Box<dyn crate::artifacts::registry::ArtifactRegistry>>,
        >,
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

        log::debug!(
            "parallel group {group_name}: child {child_name} forked (state_dir={}, artifact_dir={})",
            child_gremlin.state_dir.display(),
            child_gremlin.artifact_dir.display()
        );

        // Launch the child via the supervisor — it becomes a first-class
        // gremlin visible in `gremlins ls`.
        let LaunchResult {
            state_rx,
            registry_rx,
        } = supervisor::launch_child(child_gremlin);

        launched.push(LaunchedChild {
            child_name,
            child_id,
            state_rx,
            registry_rx,
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
    type ChildResult = (
        usize,
        String,
        String,
        Result<(), RunError>,
        Option<Box<dyn crate::artifacts::registry::ArtifactRegistry>>,
    );
    let mut futs: FuturesUnordered<tokio::task::JoinHandle<ChildResult>> = FuturesUnordered::new();

    for (idx, lc) in launched.into_iter().enumerate() {
        let child_name = lc.child_name.clone();
        let child_id = lc.child_id.clone();
        let mut state_rx = lc.state_rx;
        let registry_rx = lc.registry_rx;
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
                            // Read the child's state.json to get the real outcome.
                            let child_state_dir = state::state_dir_for(&child_id);
                            let child_state_file = child_state_dir.join("state.json");
                            let outcome = if child_state_file.is_file() {
                                let raw = state::read_state_json(Some(&child_state_file));
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
                            } else {
                                Err(RunError::Message(format!(
                                    "child {child_name} state file not found"
                                )))
                            };
                            // Collect the child's registry (for dry-run support).
                            let registry = registry_rx.await.unwrap_or(None);
                            return (idx, child_name, child_id, outcome, registry);
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
            Ok((idx, child_name, child_id, outcome, child_registry)) => {
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
                            child_id,
                            outcome: Ok(()),
                            child_registry,
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
                                child_id,
                                outcome: Err(err),
                                child_registry,
                            });
                        } else {
                            child_results.push(ChildOutcome {
                                child_name,
                                child_id,
                                outcome: Err(err),
                                child_registry,
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
            mark_child_done(gremlin.registry.as_ref(), &scope, &outcome.child_name).await;
        }
    }

    // --- Clean up child worktrees (best-effort) ---
    //
    // Iterate over all launched child IDs (not just those with results) so
    // worktrees for panicked or cancelled children are still cleaned up.
    log::debug!(
        "parallel group {group_name}: cleaning up worktrees for {} children",
        child_ids.len()
    );
    for child_id in &child_ids {
        // Find the child_name from child_results, or use the id as fallback.
        let child_name = child_results
            .iter()
            .find(|o| &o.child_id == child_id)
            .map(|o| o.child_name.as_str())
            .unwrap_or(child_id);
        if group_error.is_none() {
            cleanup_child_fully(child_name, child_id);
        } else {
            cleanup_child_worktree(gremlin, child_name, child_id);
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
    child_id: String,
    outcome: Result<(), RunError>,
    child_registry: Option<Box<dyn crate::artifacts::registry::ArtifactRegistry>>,
}

/// Merge artifacts from a successful child into the parent registry.
async fn merge_child_artifacts(
    gremlin: &mut Gremlin,
    outcome: &ChildOutcome,
) -> Result<(), RunError> {
    use crate::artifacts::registry::{Collision, FileSystemArtifactRegistry};

    // Prefer the in-memory registry the child passed back. This handles
    // dry-run children (whose DryRunArtifactRegistry never writes files)
    // and avoids re-reading registry.json from disk for filesystem children.
    if let Some(ref child_registry) = outcome.child_registry {
        gremlin
            .registry
            .merge_registry(
                child_registry.as_ref(),
                Collision::Ignore,
                Some(&outcome.child_name),
            )
            .await
            .map_err(|e| RunError::Message(format!("artifact merge failed: {e}")))?;
        return Ok(());
    }

    // Fallback: construct a FileSystemArtifactRegistry from disk.
    let child_artifact_dir = state::state_dir_for(&outcome.child_id).join("artifacts");
    if !child_artifact_dir.exists() {
        return Ok(());
    }

    let child_registry = FileSystemArtifactRegistry::new(child_artifact_dir);
    gremlin
        .registry
        .merge_registry(
            &child_registry,
            Collision::Ignore,
            Some(&outcome.child_name),
        )
        .await
        .map_err(|e| RunError::Message(format!("artifact merge failed: {e}")))?;

    Ok(())
}

/// Aggregate token usage and subprocess cost from a child into the parent.
fn aggregate_child_costs(gremlin: &mut Gremlin, outcome: &ChildOutcome) {
    let child_state_dir = state::state_dir_for(&outcome.child_id);
    let child_state_file = child_state_dir.join("state.json");
    if !child_state_file.is_file() {
        return;
    }

    let child_state = state::read_state_json(Some(&child_state_file));
    if child_state.is_empty() {
        return;
    }

    // Token usage.
    if let Some(usage) = child_state.get("token_usage").and_then(|v| v.as_object()) {
        let mut delta = HashMap::new();
        for (key, value) in usage {
            if let Some(n) = value.as_i64() {
                delta.insert(key.clone(), n);
            }
        }
        gremlin.state.accumulate_token_usage(&delta);
    }

    // Subprocess cost.
    if let Some(cost) = child_state.get("subprocess_cost_usd") {
        if let Some(n) = cost.as_f64() {
            gremlin.state.add_subprocess_cost(n);
        }
    }
}

/// Remove everything a successfully-completed child owns.
fn cleanup_child_fully(child_name: &str, child_id: &str) {
    match Gremlin::from(child_id) {
        Ok(child) => child.clean(true),
        Err(error) => {
            log::debug!("parallel group: child {child_name} left nothing to clean ({error})")
        }
    }
}

/// Clean up a child's worktree, best-effort.
fn cleanup_child_worktree(gremlin: &mut Gremlin, child_name: &str, child_id: &str) {
    use crate::core::git;

    let child_state_dir = state::state_dir_for(child_id);
    let child_state_file = child_state_dir.join("state.json");
    if !child_state_file.is_file() {
        return;
    }

    let child_state = state::read_state_json(Some(&child_state_file));
    let workdir = child_state
        .get("workdir")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if workdir.is_empty() {
        return;
    }

    let worktree_path = std::path::PathBuf::from(workdir);
    if !worktree_path.is_dir() {
        return;
    }

    git::remove_worktree(&gremlin.project_root, &worktree_path.to_string_lossy());

    if worktree_path.is_dir() {
        if let Err(e) = std::fs::remove_dir_all(&worktree_path) {
            log::warn!("parallel group: failed to clean up worktree for {child_name}: {e}");
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
    use crate::executor::gremlin::{validate_gremlin_id, RuntimeConfig};
    use crate::executor::state::{self, StateData};
    use crate::schemas::bootstrap::Bootstrap;
    use crate::test_support::Sandbox;

    /// Convert the first parsed stage to an ExecutorStage for dispatch.
    fn first_executor_stage(stages: &[StageSpec]) -> ExecutorStage {
        let def = StaticDefinition::new(
            "test".to_string(),
            PathBuf::from("test.yaml"),
            "cmd:true".to_string(),
            "main".to_string(),
            Bootstrap::default(),
            vec![],
            None,
            serde_yaml::Value::Null,
        );
        def.convert_stage(stages[0].clone())
    }
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn test_gremlin(stages: Vec<StageSpec>, default_client: &str) -> (Sandbox, Gremlin) {
        let sandbox = Sandbox::new();
        let state_dir = state::state_dir_for("gr-test");
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

        let state_data = StateData::new(Some("gr-test".to_string()));

        let gremlin = Gremlin {
            id: validate_gremlin_id("gr-test").unwrap(),
            state_dir,
            artifact_dir: artifact_dir.clone(),
            definition_path: None,
            client_override: None,
            definition: Box::new(StaticDefinition::new(
                "test".to_string(),
                PathBuf::from("test.yaml"),
                default_client.to_string(),
                "main".to_string(),
                Bootstrap::default(),
                stages.clone(),
                None,
                serde_yaml::Value::Null,
            )),
            registry: Box::new(crate::artifacts::registry::FileSystemArtifactRegistry::new(
                artifact_dir,
            )),
            worktree: None,
            worktree_parent: None,
            project_root: sandbox.path().to_path_buf(),
            base_ref_sha: String::new(),
            base_ref: "main".to_string(),
            state: state_data,
            env: HashMap::new(),
            client: crate::clients::client::Client::parse(default_client).unwrap(),
            loop_iter: "1".to_string(),
            stage_inputs: HashMap::new(),
            dry_run: false,
            runtime_config: RuntimeConfig::snapshot("gr-test"),
            cancel_token: None,
            interactive_session: None,
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
        let start = std::time::Instant::now();
        let result = run_parallel(&stage, &mut gremlin, None).await;
        let elapsed = start.elapsed();
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        assert!(
            elapsed >= std::time::Duration::from_millis(250),
            "max_concurrent=1 should serialize, but took only {elapsed:?}"
        );
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
        mark_child_done(gremlin.registry.as_ref(), &scope, "a").await;

        let stage = first_executor_stage(&stages);
        let result = run_parallel(&stage, &mut gremlin, None).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");

        assert!(
            gremlin
                .registry
                .as_ref()
                .is_registered(&done_uri(&scope, "a"))
                .await
        );
        assert!(
            gremlin
                .registry
                .as_ref()
                .is_registered(&done_uri(&scope, "b"))
                .await
        );
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
        mark_child_done(gremlin.registry.as_ref(), &scope, "a").await;
        mark_child_done(gremlin.registry.as_ref(), &scope, "b").await;

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
            .registry
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
