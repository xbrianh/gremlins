//! Exec stage runner.
//!
//! [`run_exec`] drives a single exec stage from preparation through shell
//! execution to artifact commit.

use indexmap::IndexMap;

use crate::clients::client::ModelURI;
use crate::definition::ExecutorStage;
use crate::executor::exec_runner::{commit_exec, prepare_exec, run_shell};
use crate::executor::gremlin::Gremlin;
use crate::executor::state::{Checkout, Collision};
use crate::executor::supervisor::emit_event;
use crate::executor::EventKind;
use crate::executor::RunError;

use crate::executor::run::{
    collect_checkout_keys, exec_commit_err, exec_prepare_err, stage_failed,
};

// ---------------------------------------------------------------------------
// Exec
// ---------------------------------------------------------------------------

pub(crate) async fn run_exec(
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

    let msg = format!("[{}] exec: preparing", exec.name);
    emit_event(EventKind::LogLine {
        id: gremlin.id.to_string(),
        line: msg,
    });

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
    )
    .await
    .map_err(|e| exec_prepare_err(e, &exec.name))?;

    prepared.cwd = gremlin.cwd();
    prepared.artifact_dir = artifact_dir;
    prepared.env = gremlin.env.clone();
    prepared.base_env = gremlin.runtime_config.base_process_env.clone();
    prepared.gremlin_id = Some(gremlin.id.to_string());

    if !prepared.cmds.is_empty() {
        log::info!(
            "[{}] exec: running {} command(s)",
            prepared.name,
            prepared.cmds.len()
        );
        let exec_id = gremlin.id.to_string();
        emit_event(EventKind::LogLine {
            id: exec_id.clone(),
            line: format!("exec {}: streaming output started", prepared.name),
        });
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
