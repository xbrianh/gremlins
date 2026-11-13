//! State view types and helpers shared by the read-only ops.
//!
//! `StateView` presents a uniform, JSON-friendly snapshot of a gremlin's
//! state regardless of whether it is live (present in the run map) or
//! persisted to disk.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::config;
use crate::executor::gremlin::Gremlin;
use crate::executor::state::{self, FileSystemStateStore};
use crate::executor::{null_field, str_field};

use super::{error_response, get_run_map};

pub(crate) struct StateView {
    id: String,
    status: String,
    stage: String,
    definition: String,
    project_root: String,
    workdir: String,
    pub(crate) state_dir: PathBuf,
    started_at: String,
    ended_at: Value,
    exit_code: Value,
    pid: Value,
    model: String,
    attempt: String,
    kind: String,
    pub(crate) bail_info: Value,
    token_usage: Option<Value>,
    total_cost_usd: Option<f64>,
}

impl StateView {
    /// Extract fields from a live store's state tree.
    fn from_live(
        id: &str,
        store: &Arc<dyn state::StateStore + Send + Sync>,
    ) -> Result<Self, Value> {
        let tree = store.state_tree();
        if tree.is_empty() {
            return Err(error_response(&format!("gremlin {id}: state is empty")));
        }
        Ok(Self {
            id: id.to_string(),
            status: "running".to_string(),
            stage: str_field(&tree, "stage"),
            definition: definition_name_from_tree(&tree),
            project_root: str_field(&tree, "project_root"),
            workdir: str_field(&tree, "workdir"),
            state_dir: live_state_dir(store, id),
            started_at: str_field(&tree, "started_at"),
            ended_at: null_field(&tree, "ended_at"),
            exit_code: null_field(&tree, "exit_code"),
            pid: null_field(&tree, "pid"),
            model: str_field(&tree, "model"),
            attempt: str_field(&tree, "attempt"),
            kind: str_field(&tree, "kind"),
            bail_info: store
                .stage_error()
                .map(Value::Object)
                .unwrap_or(Value::Null),
            token_usage: tree.get("token_usage").cloned(),
            total_cost_usd: tree.get("total_cost_usd").and_then(|v| v.as_f64()),
        })
    }

    /// Extract fields from a persisted (non-live) gremlin.
    fn from_persisted(gremlin: &Gremlin, state_dir: PathBuf) -> Self {
        Self {
            id: gremlin.id.as_str().to_string(),
            status: status_or_orphan(false, gremlin),
            stage: gremlin.state.read_str("stage"),
            definition: definition_display_name(gremlin),
            project_root: gremlin.project_root.to_string_lossy().to_string(),
            workdir: gremlin
                .workdir
                .as_ref()
                .map(|w| w.path().to_string_lossy().to_string())
                .unwrap_or_default(),
            state_dir,
            started_at: gremlin.state.read_str("started_at"),
            ended_at: gremlin.state.read_field("ended_at").unwrap_or(Value::Null),
            exit_code: gremlin.state.read_field("exit_code").unwrap_or(Value::Null),
            pid: gremlin.state.read_field("pid").unwrap_or(Value::Null),
            model: gremlin.state.read_str("model"),
            attempt: gremlin.state.read_str("attempt"),
            kind: gremlin.state.read_str("kind"),
            bail_info: gremlin
                .state
                .stage_error()
                .map(Value::Object)
                .unwrap_or(Value::Null),
            token_usage: gremlin.state.read_field("token_usage"),
            total_cost_usd: gremlin
                .state
                .read_field("total_cost_usd")
                .and_then(|v| v.as_f64()),
        }
    }

    /// The fields common to `status` and `info` responses.
    pub(crate) fn common_json(&self) -> Map<String, Value> {
        let value = serde_json::json!({
            "id": self.id,
            "status": self.status,
            "stage": self.stage,
            "definition": self.definition,
            "project_root": self.project_root,
            "workdir": self.workdir,
            "state_dir": self.state_dir.to_string_lossy(),
            "artifact_dir": self.state_dir.join("artifacts").to_string_lossy(),
            "started_at": self.started_at,
            "ended_at": self.ended_at,
            "exit_code": self.exit_code,
            "pid": self.pid,
            "model": self.model,
            "attempt": self.attempt,
            "kind": self.kind,
            "token_usage": self.token_usage,
            "total_cost_usd": self.total_cost_usd,
        });
        value.as_object().cloned().unwrap_or_default()
    }
}

/// Open a persisted gremlin by id, returning the gremlin and its state dir.
pub(crate) fn open_persisted_gremlin(id: &str) -> Result<(Gremlin, PathBuf), Value> {
    let gremlin = Gremlin::from(id).map_err(|e| error_response(&format!("gremlin {id}: {e}")))?;
    let state_dir = gremlin.state.state_dir();
    Ok((gremlin, state_dir))
}

pub(crate) fn state_view_for_id(id: &str) -> Result<StateView, Value> {
    let live = {
        let map = get_run_map().lock().unwrap();
        map.get(id).map(|h| h.store.clone())
    };
    if let Some(store) = live {
        return StateView::from_live(id, &store);
    }
    let (gremlin, state_dir) = open_persisted_gremlin(id)?;
    Ok(StateView::from_persisted(&gremlin, state_dir))
}

pub(crate) fn live_state_dir(
    store: &Arc<dyn state::StateStore + Send + Sync>,
    id: &str,
) -> PathBuf {
    store
        .as_any()
        .and_then(|a| a.downcast_ref::<FileSystemStateStore>())
        .map(|fs| fs.root_dir().to_path_buf())
        .unwrap_or_else(|| config::state_root().join(format!("{id}-0001")))
}

pub(crate) fn status_or_orphan(is_live: bool, gremlin: &Gremlin) -> String {
    if is_live {
        return "running".to_string();
    }
    let status = gremlin.state.read_str("status");
    if status == "running" {
        return "orphan".to_string();
    }
    status
}

pub(crate) fn definition_display_name(gremlin: &Gremlin) -> String {
    let recorded = gremlin.state.read_str("definition_path");
    if let Some(stem) = std::path::Path::new(&recorded)
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
    {
        return stem.to_string();
    }
    gremlin.state.read_str("kind")
}

pub(crate) fn definition_name_from_tree(tree: &Map<String, Value>) -> String {
    tree.get("definition_path")
        .and_then(|v| v.as_str())
        .and_then(|s| {
            std::path::Path::new(s)
                .file_stem()
                .and_then(|s| s.to_str())
                .map(String::from)
        })
        .unwrap_or_else(|| str_field(tree, "kind"))
}
