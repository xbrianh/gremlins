//! Stage boundary events produced by the executor.

use gremlins_magic_bus::{emit_event, EventKind};

/// Emit a StageBoundary event on the bus.
///
/// The bus is the single transport for stage boundaries and stage content:
/// subscribers receive this event in emission order, before the stage content
/// it opens.
pub(crate) fn send_stage_boundary(gremlin_id: &str, stage_name: &str, stage_type: &str) {
    let active = !stage_name.is_empty();
    emit_event(EventKind::StageBoundary {
        id: gremlin_id.to_string(),
        stage: stage_name.to_string(),
        stage_type: stage_type.to_string(),
        active,
    });
}
