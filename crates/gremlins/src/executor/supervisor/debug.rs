//! Helpers shared by the attachment handler and its tests.

use crate::executor::active_leaf::{ActiveStage, LeafKind};

use super::run_registry::StoreHandle;
use gremlins_magic_bus::connection::SharedWriter;

pub(crate) async fn send_debug_status(writer: &SharedWriter, stage: &str) {
    let payload = serde_json::json!({"type": "status", "stage": stage});
    let _ = writer.write_json_line(&payload).await;
}

pub(crate) fn check_agent_stage(store: &StoreHandle) -> Result<(), String> {
    let active = ActiveStage::read(store.as_ref())?;
    if active.leaf_kind == Some(LeafKind::Agent) {
        Ok(())
    } else {
        Err(format!(
            "not in an agent stage (current: {})",
            active.display_name
        ))
    }
}
