//! Invocation-scoped stage boundaries produced by the executor.

use gremlins_magic_bus::{emit_corr_event, CorrId, EventKind};

/// Captured before dispatch; dropping the guard also closes aborted futures.
///
/// The process-global bus must outlive every invocation (it is never shut down).
/// Publication retains the bus's intentional panic on poisoned state; it must not
/// be used after a panic has poisoned the bus.
pub(crate) struct StageInvocation {
    pub(crate) stage_id: String,
    corr_id: CorrId,
    run_id: String,
    name: String,
    stage_type: String,
}

impl StageInvocation {
    pub(crate) fn enter(run_id: &str, name: &str, stage_type: &str) -> Self {
        let stage_id = super::state::token_hex(16);
        let invocation = Self {
            corr_id: CorrId::new()
                .with_run(run_id)
                .with_stage(name)
                .with_execution_id(&stage_id),
            stage_id,
            run_id: run_id.to_owned(),
            name: name.to_owned(),
            stage_type: stage_type.to_owned(),
        };
        invocation.publish(true);
        invocation
    }

    pub(crate) fn corr_id(&self) -> CorrId {
        self.corr_id.clone()
    }

    fn publish(&self, active: bool) {
        emit_corr_event(
            &self.corr_id,
            EventKind::StageBoundary {
                id: self.run_id.clone(),
                stage: self.name.clone(),
                stage_id: self.stage_id.clone(),
                stage_type: self.stage_type.clone(),
                active,
            },
        );
    }
}

impl Drop for StageInvocation {
    fn drop(&mut self) {
        self.publish(false);
    }
}
