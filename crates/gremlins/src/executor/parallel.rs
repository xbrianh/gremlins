//! The parallel fan-out / fan-in executor.
//!
//! [`run_parallel`] destructures a [`RunnableStage::Parallel`], guards against
//! already-completed groups, spawns one tokio task per child (bounded by a
//! [`Semaphore`]), collects results from a [`JoinSet`], applies the group's
//! [`ErrorPolicy`], merges artifacts from successful children into the parent
//! registry, cleans up child worktrees (best-effort), and aggregates child
//! costs into the parent.
//!
//! Children run as forked gremlins via [`Gremlin::fork_with_stages`], which
//! accepts a `Vec<RunnableStage>` instead of a child definition path — the child
//! definition inherits parent metadata but runs only the given stages.
//!
//! Each child runs on a dedicated [`std::thread`] worker thread (spawned via
//! [`std::thread::spawn`]) with its own single-threaded tokio runtime.
//! The [`JoinSet`] manages the concurrency bound and cancellation: each
//! spawned task awaits a [`oneshot`] receiver from its worker thread. Thread
//! join handles are collected so that every worker thread is joined before
//! `run_parallel` returns — no orphaned threads, even after `cancel_on_error`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::oneshot;
use tokio::task::JoinSet;

use crate::executor::gremlin::Gremlin;
use crate::executor::state;
use crate::executor::RunError;
use crate::stages::node::RunnableStage;
use crate::stages::parallel::ErrorPolicy;

/// Run a parallel group: fork one child per stage, fan out via [`JoinSet`],
/// join, and apply the error policy.
pub(crate) async fn run_parallel(
    stage: &RunnableStage,
    gremlin: &mut Gremlin,
    enclosing_client: Option<&str>,
) -> Result<(), RunError> {
    let RunnableStage::Parallel {
        attrs,
        max_concurrent,
        cancel_on_error,
        error_policy,
        client,
        body,
    } = stage
    else {
        unreachable!("run_parallel is only called for parallel stages")
    };

    let group_name = &attrs.name;
    let total_children = body.len();

    log::debug!(
        "parallel group {group_name}: starting with {total_children} children (max_concurrent={max_concurrent:?}, cancel_on_error={cancel_on_error}, error_policy={error_policy:?})"
    );

    // --- Resumption guard ---
    //
    // A group is fully complete when every child is in `done_for`. The check
    // is against `total_children`, not the number of successful children.
    let done = gremlin.state.done_for(group_name);
    if done.len() == total_children && total_children > 0 {
        log::info!(
            "parallel group {group_name}: all {total_children} children already done — skipping"
        );
        return Ok(());
    }

    // --- Concurrency bound ---
    //
    // `max_concurrent` caps the number of in-flight tasks. When absent, every
    // child spawns at once. The semaphore is acquired *inside* each worker
    // thread so that all JoinSet tasks are immediately enqueued and can be
    // cancelled by `abort_all` before their threads acquire a permit.
    let semaphore: Option<Arc<tokio::sync::Semaphore>> = max_concurrent
        .filter(|&n| n > 0)
        .map(|n| Arc::new(tokio::sync::Semaphore::new(n as usize)));

    // Cancellation flag: set when `cancel_on_error` fires so worker threads
    // that are still waiting on the semaphore can exit immediately instead
    // of starting real work.
    let cancel_flag = Arc::new(AtomicBool::new(false));

    // We use a JoinSet to manage the concurrency bound and cancellation.
    // Each task awaits a oneshot receiver from a worker thread.
    type ChildResult = (
        String,
        String,
        Result<(), RunError>,
        Option<Box<dyn crate::artifacts::registry::ArtifactRegistry>>,
    );

    let mut join_set: JoinSet<ChildResult> = JoinSet::new();
    let mut thread_handles: Vec<std::thread::JoinHandle<()>> = Vec::new();
    let mut spawned_ids: Vec<(String, String)> = Vec::new(); // (child_name, child_id)
    let mut spawned = 0usize;

    for child in body {
        let child_name = child.name().to_string();

        // Skip children already marked done.
        if done.contains(&child_name) {
            log::info!("parallel group {group_name}: child {child_name} already done — skipping");
            continue;
        }

        let child_id = format!("{}-{}", gremlin.id.as_str(), child_name);
        let parent_id = gremlin.id.to_string();
        let group_name_owned = group_name.to_string();
        let child_stages = vec![child.clone()];

        log::debug!(
            "parallel group {group_name}: forking child {child_name} (child_id={child_id})"
        );
        // Fork the child gremlin (before spawning), so the
        // worker thread only has to call `run()`.
        let mut child_gremlin = gremlin
            .fork_with_stages(
                &child_id,
                &parent_id,
                &group_name_owned,
                &child_name,
                child_stages,
            )
            .await?;

        // Resolve the effective client for this parallel group:
        // 1. The group's own `client:` always wins.
        // 2. Otherwise, the enclosing client from the parent sequence/loop.
        let enclosing_spec =
            enclosing_client.map(|c| crate::stages::composite::ClientSpec(c.to_string()));
        let effective_client = client.as_ref().or(enclosing_spec.as_ref());
        if let Some(c) = &effective_client {
            // The definition default is what `resolve_client_spec` step 4
            // falls back to.
            child_gremlin.definition.default_client = c.0.clone();
            // Also set the client handle directly on the child, so
            // `resolve_client`'s early return (when the spec equals the
            // definition default) picks up the right backend.
            child_gremlin.client = crate::clients::client::Client::parse(&c.0)
                .unwrap_or_else(|_| child_gremlin.client.clone());
        }

        log::debug!(
            "parallel group {group_name}: child {child_name} forked (state_dir={}, artifact_dir={})",
            child_gremlin.state_dir.display(),
            child_gremlin.artifact_dir.display()
        );

        spawned_ids.push((child_name.clone(), child_id.clone()));

        let (tx, rx) = oneshot::channel::<ChildResult>();

        let child_name_for_thread = child_name.clone();
        let child_id_for_thread = child_id.clone();
        let sem = semaphore.clone();
        let cancel = Arc::clone(&cancel_flag);

        let handle = std::thread::spawn(move || {
            // Each child gets its own single-threaded runtime.
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build child runtime");

            // Acquire the permit inside the thread (and its runtime) so the
            // spawn loop is not blocked and the JoinSet task can be aborted
            // before the permit is acquired.
            let _permit = sem
                .as_ref()
                .map(|sem| rt.block_on(sem.clone().acquire_owned()));

            // Check the cancellation flag after acquiring the permit but
            // before starting real work. If the group has already been
            // cancelled, exit immediately.
            if cancel.load(Ordering::Acquire) {
                log::debug!(
                    "parallel group {group_name_owned}: child {child_name_for_thread} cancel flag set before run() — exiting"
                );
                // Mark the child terminal so its state directory isn't left
                // permanently as "running" with no finished marker.
                child_gremlin.state.write_terminal_state(-1);
                let _ = tx.send((
                    child_name_for_thread,
                    child_id_for_thread,
                    Err(RunError::Message("cancelled".to_string())),
                    Some(child_gremlin.registry),
                ));
                return;
            }

            log::debug!(
                "parallel group {group_name_owned}: child {child_name_for_thread} (id={child_id_for_thread}) calling run()"
            );
            let outcome = rt.block_on(async { child_gremlin.run().await.map(|_| ()) });
            log::debug!(
                "parallel group {group_name_owned}: child {child_name_for_thread} (id={child_id_for_thread}) run() completed (outcome={})",
                match &outcome {
                    Ok(()) => "Ok".to_string(),
                    Err(e) => format!("Err: {e}"),
                }
            );

            let _ = tx.send((
                child_name_for_thread,
                child_id_for_thread,
                outcome,
                if child_gremlin.dry_run {
                    Some(child_gremlin.registry)
                } else {
                    None
                },
            ));
        });

        thread_handles.push(handle);

        // Spawn a JoinSet task that awaits the oneshot receiver.
        let child_name_js = child_name.clone();
        join_set.spawn(async move {
            match rx.await {
                Ok(result) => result,
                Err(_) => (
                    child_name_js.clone(),
                    String::new(),
                    Err(RunError::Message(format!(
                        "child {child_name_js} thread terminated unexpectedly"
                    ))),
                    None,
                ),
            }
        });

        spawned += 1;
        log::debug!("parallel group {group_name}: spawned child {child_name} on thread");
    }

    if spawned == 0 {
        // Every child was already done — the group is complete.
        gremlin.state.clear_done(group_name);
        return Ok(());
    }

    // --- Collect results ---
    //
    // On `cancel_on_error`, abort all remaining JoinSet tasks before
    // draining, then drain whatever is left. The worker threads continue
    // running, but we join them all at the end so none are orphaned.
    let mut child_results: Vec<ChildOutcome> = Vec::with_capacity(spawned);
    let mut first_error: Option<RunError> = None;

    while let Some(result) = join_set.join_next().await {
        match result {
            Ok((child_name, child_id, outcome, child_registry)) => {
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
                            // Save the error for the group result. The child
                            // still gets its real outcome recorded so
                            // downstream merge / mark_done can correctly
                            // classify it as failed.
                            first_error = Some(RunError::StageFailed {
                                stage: child_name.clone(),
                                message: err.to_string(),
                            });
                            cancel_flag.store(true, Ordering::Release);
                            join_set.abort_all();
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
                // A task panicked or was cancelled.
                if join_error.is_panic() {
                    let msg =
                        format!("parallel group {group_name}: a child task panicked: {join_error}");
                    log::error!("{msg}");
                    if first_error.is_none() {
                        first_error = Some(RunError::Message(msg));
                    }
                }
                // Cancelled tasks are expected after abort_all — ignore.
            }
        }
    }

    // --- Join all worker threads ---
    //
    // After the JoinSet is fully drained (including after abort_all), join
    // every worker thread so none are orphaned. Threads that were waiting on
    // the semaphore will have seen the cancel flag and exited quickly;
    // threads that were already running will finish naturally.
    for handle in thread_handles {
        if let Err(e) = handle.join() {
            log::error!("parallel group {group_name}: a worker thread panicked: {e:?}");
        }
    }

    // --- Apply error policy ---
    //
    // `Any`: bail the group when any child bailed or failed.
    // `All`: bail only when every child bailed or failed.
    //
    // Previously-completed children (in `done`) count as successes so that a
    // resumed group with prior successes does not incorrectly fail under
    // `ErrorPolicy::All` when every *remaining* child fails.

    let mut failed_names: HashSet<String> = HashSet::new();
    let mut real_errors: Vec<RunError> = Vec::new();
    let mut success_count = done.len();

    for outcome in &mut child_results {
        let outcome_ok = outcome.outcome.is_ok();
        if outcome_ok {
            success_count += 1;
        } else {
            failed_names.insert(outcome.child_name.clone());
            // Take the error out for policy evaluation. The outcome is
            // replaced with Ok(()) but `failed_names` tracks the truth —
            // downstream merge / mark_done consult `failed_names`, not
            // `outcome.outcome`.
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

    // If we already captured a first_error from cancel_on_error or a panic,
    // use that in preference to the policy-derived error.
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
    //
    // This writes the *parent's* state, so it must run before the cleanup below
    // starts removing directories; keeping the order this way means no step can
    // ever consult a child's state directory after it is gone.
    for outcome in &child_results {
        if !failed_names.contains(&outcome.child_name) {
            gremlin.state.mark_done(group_name, &outcome.child_name);
        }
    }

    // If all children succeeded, clear the done tracking.
    if group_error.is_none() {
        gremlin.state.clear_done(group_name);
    }

    // --- Clean up child worktrees (best-effort) ---
    log::debug!(
        "parallel group {group_name}: cleaning up worktrees for {} spawned children",
        spawned_ids.len()
    );
    //
    // Iterate over *all* spawned children — not just those that reported a
    // result — so worktrees created during `fork_with_stages` for cancelled
    // or otherwise missing tasks are still cleaned up.
    //
    // On success the child is spent: `clean(true)` drops its worktree, its
    // scratch directory and its state directory, and the parent has already
    // merged everything worth keeping. On failure the state directory is the
    // only record of what went wrong, so only the worktree is removed.
    for (child_name, child_id) in &spawned_ids {
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
///
/// Uses [`ArtifactRegistry::merge_registry`] with [`Collision::Ignore`]
/// and the child name as `key_prefix`. The child's filesystem registry is
/// constructed from its artifact directory on disk; the file copy inside
/// `merge_registry` makes merged artifacts survive child cleanup.
async fn merge_child_artifacts(
    gremlin: &mut Gremlin,
    outcome: &ChildOutcome,
) -> Result<(), RunError> {
    use crate::artifacts::registry::{Collision, FileSystemArtifactRegistry};
    use crate::config;

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
    let child_artifact_dir = config::state_root()
        .join(&outcome.child_id)
        .join("artifacts");
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
    // Derive the child state path from the parent's state directory, matching
    // the layout `fork_with_stages` uses (`state_dir.parent() / child_id`).
    let parent_state_root = gremlin
        .state_dir
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let child_state_dir = parent_state_root.join(&outcome.child_id);
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
///
/// Reconstructs a cheap handle with [`Gremlin::from`] — which reads paths only
/// and never loads a definition — and hands it to [`Gremlin::clean`]. Failure is
/// expected when the child's state directory is already gone; it is logged and
/// swallowed, because a group that succeeded must not fail on cleanup.
fn cleanup_child_fully(child_name: &str, child_id: &str) {
    match Gremlin::from(child_id) {
        Ok(child) => child.clean(true),
        Err(error) => {
            log::debug!("parallel group: child {child_name} left nothing to clean ({error})")
        }
    }
}

/// Clean up a child's worktree, best-effort.
///
/// Calls `git worktree remove` first so git can clean up its internal
/// metadata; falls back to manual filesystem deletion only if the git
/// command fails or the directory still exists afterward.
fn cleanup_child_worktree(gremlin: &mut Gremlin, child_name: &str, child_id: &str) {
    use crate::core::git;

    // Derive the child state path from the parent's state directory, matching
    // the layout `fork_with_stages` uses.
    let parent_state_root = gremlin
        .state_dir
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let child_state_dir = parent_state_root.join(child_id);
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

    // Let git unregister (and remove) the worktree first.
    git::remove_worktree(&gremlin.project_root, &worktree_path.to_string_lossy());

    // If the directory still exists (e.g. git failed or we're not in a repo),
    // remove it manually as a fallback.
    if worktree_path.is_dir() {
        if let Err(e) = std::fs::remove_dir_all(&worktree_path) {
            log::warn!("parallel group: failed to clean up worktree for {child_name}: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::gremlin::validate_gremlin_id;
    use crate::executor::state::StateData;
    use crate::schemas::bootstrap::Bootstrap;
    use crate::schemas::gremlin_definition::GremlinDefinition;
    use crate::stages::composite::StageAttrs;
    use crate::stages::parallel::ErrorPolicy;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn parse_stages(yaml: &str) -> Vec<RunnableStage> {
        let mut value: serde_yaml::Value = serde_yaml::from_str(yaml).expect("valid YAML");
        let list = value.as_sequence_mut().expect("a stage list");
        RunnableStage::parse_stages(list, 0).expect("valid stages")
    }

    fn test_gremlin(
        stages: Vec<RunnableStage>,
        default_client: &str,
    ) -> (tempfile::TempDir, Gremlin) {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("state").join("gr-test");
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

        let mut state_data = StateData::new(Some("gr-test".to_string()));
        state_data.state_file = Some(state_dir.join("state.json"));

        let gremlin = Gremlin {
            id: validate_gremlin_id("gr-test").unwrap(),
            state_dir,
            artifact_dir: artifact_dir.clone(),
            definition_path: None,
            client_override: None,
            definition: GremlinDefinition {
                name: "test".to_string(),
                path: PathBuf::from("test.yaml"),
                default_client: default_client.to_string(),
                base_ref: "main".to_string(),
                bootstrap: Bootstrap::default(),
                stages: stages.clone(),
                land: None,
                expanded_yaml: serde_yaml::Value::Null,
            },
            registry: Box::new(crate::artifacts::registry::FileSystemArtifactRegistry::new(
                artifact_dir,
            )),
            worktree: None,
            worktree_parent: None,
            project_root: tmp.path().to_path_buf(),
            base_ref_sha: String::new(),
            base_ref: "main".to_string(),
            resume_from: None,
            state: state_data,
            env: HashMap::new(),
            client: crate::clients::client::Client::parse(default_client).unwrap(),
            loop_stack: Vec::new(),
            stage_inputs: HashMap::new(),
            dry_run: false,
            definition_is_expanded: false,
        };
        (tmp, gremlin)
    }

    // --- Empty group ---

    #[tokio::test]
    async fn empty_parallel_group_succeeds() {
        let stage = RunnableStage::Parallel {
            attrs: StageAttrs::new("empty".to_string()),
            max_concurrent: None,
            cancel_on_error: false,
            error_policy: ErrorPolicy::Any,
            client: None,
            body: vec![],
        };
        let (_tmp, mut gremlin) = test_gremlin(vec![], "cmd:true");
        let result = run_parallel(&stage, &mut gremlin, None).await;
        assert!(result.is_ok());
    }

    // --- Single child success ---

    #[tokio::test]
    async fn single_child_succeeds() {
        let yaml = r#"
- name: group
  parallel:
    - name: a
      type: exec
      options:
        cmds: ["true"]
"#;
        let stages = parse_stages(yaml);
        let (_tmp, mut gremlin) = test_gremlin(stages.clone(), "cmd:true");
        let stage = stages[0].clone();
        let result = run_parallel(&stage, &mut gremlin, None).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }

    // --- ErrorPolicy::Any ---

    #[tokio::test]
    async fn error_policy_any_bails_on_first_child_failure() {
        let yaml = r#"
- name: group
  error_policy: any
  parallel:
    - name: bad
      type: exec
      options:
        cmds: ["gremlins-nonexistent-cmd-xyz"]
    - name: good
      type: exec
      options:
        cmds: ["true"]
"#;
        let stages = parse_stages(yaml);
        let (_tmp, mut gremlin) = test_gremlin(stages.clone(), "cmd:true");
        let stage = stages[0].clone();
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
        let yaml = r#"
- name: group
  error_policy: all
  parallel:
    - name: bad
      type: exec
      options:
        cmds: ["gremlins-nonexistent-cmd-xyz"]
    - name: good
      type: exec
      options:
        cmds: ["true"]
"#;
        let stages = parse_stages(yaml);
        let (_tmp, mut gremlin) = test_gremlin(stages.clone(), "cmd:true");
        let stage = stages[0].clone();
        let result = run_parallel(&stage, &mut gremlin, None).await;
        // One success is enough with All policy.
        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }

    #[tokio::test]
    async fn error_policy_all_bails_when_every_child_fails() {
        let yaml = r#"
- name: group
  error_policy: all
  parallel:
    - name: bad1
      type: exec
      options:
        cmds: ["gremlins-nonexistent-cmd-xyz"]
    - name: bad2
      type: exec
      options:
        cmds: ["gremlins-nonexistent-cmd-xyz"]
"#;
        let stages = parse_stages(yaml);
        let (_tmp, mut gremlin) = test_gremlin(stages.clone(), "cmd:true");
        let stage = stages[0].clone();
        let result = run_parallel(&stage, &mut gremlin, None).await;
        match result {
            Err(RunError::StageFailed { .. }) => {}
            other => panic!("expected StageFailed, got {other:?}"),
        }
    }

    // --- cancel_on_error ---

    #[tokio::test]
    async fn cancel_on_error_aborts_siblings_on_first_failure() {
        // With max_concurrent=1, children run one at a time. The "bad"
        // child is listed first and fails quickly. The "slow" child is
        // either still waiting on the semaphore (in which case the cancel
        // flag makes it exit immediately) or has already started (in which
        // case it runs to completion). Either way, the group returns the
        // correct error and all threads are joined.
        let yaml = r#"
- name: group
  cancel_on_error: true
  error_policy: any
  max_concurrent: 1
  parallel:
    - name: bad
      type: exec
      options:
        cmds: ["gremlins-nonexistent-cmd-xyz"]
    - name: slow
      type: exec
      options:
        cmds: ["sleep 1"]
"#;
        let stages = parse_stages(yaml);
        let (_tmp, mut gremlin) = test_gremlin(stages.clone(), "cmd:true");
        let stage = stages[0].clone();
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
        // With max_concurrent=1, children run sequentially. We use `sleep 0.1`
        // commands and check that total time is at least N * 0.1s.
        let yaml = r#"
- name: group
  max_concurrent: 1
  parallel:
    - name: a
      type: exec
      options:
        cmds: ["sleep 0.1"]
    - name: b
      type: exec
      options:
        cmds: ["sleep 0.1"]
    - name: c
      type: exec
      options:
        cmds: ["sleep 0.1"]
"#;
        let stages = parse_stages(yaml);
        let (_tmp, mut gremlin) = test_gremlin(stages.clone(), "cmd:true");
        let stage = stages[0].clone();
        let start = std::time::Instant::now();
        let result = run_parallel(&stage, &mut gremlin, None).await;
        let elapsed = start.elapsed();
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        // With max_concurrent=1, 3 × 0.1s ≈ 0.3s minimum.
        assert!(
            elapsed >= std::time::Duration::from_millis(250),
            "max_concurrent=1 should serialize, but took only {elapsed:?}"
        );
    }

    // --- Resumption after partial completion ---

    #[tokio::test]
    async fn resumption_skips_already_done_children() {
        let yaml = r#"
- name: group
  parallel:
    - name: a
      type: exec
      options:
        cmds: ["true"]
    - name: b
      type: exec
      options:
        cmds: ["true"]
"#;
        let stages = parse_stages(yaml);
        let (_tmp, mut gremlin) = test_gremlin(stages.clone(), "cmd:true");

        // Mark child "a" as already done.
        gremlin.state.mark_done("group", "a");

        let stage = stages[0].clone();
        let result = run_parallel(&stage, &mut gremlin, None).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");

        // After success, done tracking is cleared.
        assert!(gremlin.state.done_for("group").is_empty());
    }

    #[tokio::test]
    async fn fully_done_group_is_skipped_entirely() {
        let yaml = r#"
- name: group
  parallel:
    - name: a
      type: exec
      options:
        cmds: ["gremlins-nonexistent-cmd-xyz"]
    - name: b
      type: exec
      options:
        cmds: ["gremlins-nonexistent-cmd-xyz"]
"#;
        let stages = parse_stages(yaml);
        let (_tmp, mut gremlin) = test_gremlin(stages.clone(), "cmd:true");

        // Mark both children done — the group is fully complete.
        gremlin.state.mark_done("group", "a");
        gremlin.state.mark_done("group", "b");

        let stage = stages[0].clone();
        let result = run_parallel(&stage, &mut gremlin, None).await;
        // Should succeed without running any child (which would fail).
        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }

    // --- Worktree lifecycle ---

    #[tokio::test]
    async fn child_worktrees_are_cleaned_up() {
        // Without a git repo, children won't have worktrees — the test
        // verifies that cleanup is best-effort and does not crash.
        let yaml = r#"
- name: group
  parallel:
    - name: a
      type: exec
      options:
        cmds: ["true"]
"#;
        let stages = parse_stages(yaml);
        let (_tmp, mut gremlin) = test_gremlin(stages.clone(), "cmd:true");
        let stage = stages[0].clone();
        let result = run_parallel(&stage, &mut gremlin, None).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }

    // --- Artifact merge ---

    #[tokio::test]
    async fn child_artifacts_are_merged_into_parent() {
        use crate::test_support::EnvGuard;

        // The child runs an exec stage that produces an artifact via
        // `bind`. After the parallel group succeeds, the parent registry
        // must contain the merged artifact, prefixed with the child name.
        let yaml = r#"
- name: group
  parallel:
    - name: writer
      type: exec
      bind:
        output: "artifact://out.txt"
      options:
        cmds: ["echo hello > {output}"]
"#;
        let stages = parse_stages(yaml);
        let (_tmp, mut gremlin) = test_gremlin(stages.clone(), "cmd:true");

        // Point config::state_root() at the temp dir so the fallback
        // path in merge_child_artifacts finds the child's artifact
        // directory. Without this, config::state_root() returns the
        // system default and the merge silently skips every child.
        let mut env = EnvGuard::lock();
        env.set("GREMLINS_SANDBOX_ROOT", _tmp.path());

        let stage = stages[0].clone();
        let result = run_parallel(&stage, &mut gremlin, None).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");

        // The parent registry must contain the merged artifact.
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
        let yaml = r#"
- name: group
  parallel:
    - name: a
      type: exec
      options:
        cmds: ["true"]
"#;
        let stages = parse_stages(yaml);
        let (_tmp, mut gremlin) = test_gremlin(stages.clone(), "cmd:true");
        let stage = stages[0].clone();
        let result = run_parallel(&stage, &mut gremlin, None).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        // Cost aggregation is best-effort; the test just verifies no panic.
    }

    // --- Client inheritance ---

    #[tokio::test]
    async fn parallel_with_explicit_client_succeeds() {
        // Smoke test: a parallel with an explicit `client:` propagates it
        // to child definitions so resolve_client_spec's step-4 fallback
        // picks it up. An exec child doesn't call resolve_client, so this
        // just proves the new code path doesn't crash.
        let yaml = r#"
- name: group
  client: "cmd:true"
  parallel:
    - name: a
      type: exec
      options:
        cmds: ["true"]
"#;
        let stages = parse_stages(yaml);
        let (_tmp, mut gremlin) = test_gremlin(stages.clone(), "cmd:true");
        let stage = stages[0].clone();
        let result = run_parallel(&stage, &mut gremlin, None).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }
}
