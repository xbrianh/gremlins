//! Resolve the active leaf from a single run-state snapshot. A sequence's
//! display name alone cannot distinguish same-named leaves in different scopes.

use std::io::Read;

use serde_json::{Map, Value};

use super::run_agent::chat_turn_from_stage_name;
use super::state::{BlobMode, StateStore};
use crate::config;
use crate::definition::{StageSpec, StaticDefinition};

#[derive(Debug, PartialEq, Eq)]
pub(super) enum LeafKind {
    Agent,
    Exec,
}

pub(super) struct ActiveStage {
    pub display_name: String,
    pub leaf_kind: Option<LeafKind>,
}

impl ActiveStage {
    pub(super) fn read(store: &dyn StateStore) -> Result<Self, String> {
        let tree = store.state_tree();
        let display = tree
            .get("stage")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or("no recorded stage")?;
        let leaf_kind = if tree.get("kind").and_then(Value::as_str) == Some("chat") {
            // Chat stages are generated per turn and have no definition snapshot.
            chat_turn_from_stage_name(display).map(|_| LeafKind::Agent)
        } else if let Some(path) = LeafPath::from_snapshot(display, &tree) {
            path.resolve(&read_definition(store, &tree)?)
        } else {
            None
        };
        Ok(Self {
            display_name: display.to_owned(),
            leaf_kind,
        })
    }
}

struct LeafPath<'a> {
    sequences: Vec<&'a str>,
    name: &'a str,
}

impl<'a> LeafPath<'a> {
    fn from_snapshot(display: &'a str, tree: &'a Map<String, Value>) -> Option<Self> {
        let Some(scope) = tree.get("active_sequence") else {
            return Some(Self {
                sequences: Vec::new(),
                name: display,
            });
        };
        let sequences: Vec<_> = scope
            .as_array()?
            .iter()
            .map(Value::as_str)
            .collect::<Option<_>>()?;
        if sequences.first().copied() != Some(display) {
            return None;
        }
        Some(Self {
            sequences,
            name: tree.get("sub_stage")?.as_str()?,
        })
    }

    fn resolve(&self, definition: &StaticDefinition) -> Option<LeafKind> {
        let mut stages = definition.stages.as_slice();
        for &sequence in &self.sequences {
            let StageSpec::Sequence { body, .. } = stages.iter().find(|s| s.name() == sequence)?
            else {
                return None;
            };
            stages = body;
        }
        match stages.iter().find(|s| s.name() == self.name)? {
            StageSpec::Agent { .. } => Some(LeafKind::Agent),
            StageSpec::Exec { .. } => Some(LeafKind::Exec),
            _ => None,
        }
    }
}

fn read_definition(
    store: &dyn StateStore,
    tree: &Map<String, Value>,
) -> Result<StaticDefinition, String> {
    let bytes = match tree.get("definition_revision") {
        Some(Value::String(id)) => store
            .read_definition_revision(id)
            .map_err(|e| format!("failed to read definition revision: {e}"))?,
        Some(_) => return Err("invalid definition_revision".into()),
        None => {
            let mut bytes = Vec::new();
            let mut blob = store
                .open("definition.json", BlobMode::Read)
                .map_err(|e| format!("failed to read definition snapshot: {e}"))?;
            blob.read_to_end(&mut bytes)
                .map_err(|e| format!("failed to read definition snapshot: {e}"))?;
            bytes
        }
    };
    let default_model = config::global_config()
        .ok()
        .and_then(|c| c.default_model().map(String::from));
    StaticDefinition::from_expanded_bytes(&bytes, None, default_model.as_deref())
        .map_err(|e| format!("failed to load definition: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::state::FileSystemStateStore;
    use serde_json::json;
    use std::io::Write;

    fn definition(name: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({"default_model": "cmd/true", "stages": [
            {"name": name, "type": "exec", "options": {"cmds": ["true"]}}
        ]}))
        .unwrap()
    }

    fn fixture() -> FileSystemStateStore {
        FileSystemStateStore::create(
            "gr-active-definition",
            json!({"stage": "committed"}).as_object().unwrap(),
            true,
        )
        .unwrap()
    }

    fn write_blob(store: &dyn StateStore, path: &str, bytes: &[u8]) {
        store
            .open(path, BlobMode::Write)
            .unwrap()
            .write_all(bytes)
            .unwrap();
    }

    #[test]
    fn committed_revision_ignores_stale_and_missing_alias() {
        let store = fixture();
        store
            .initialize_definition(&definition("committed"))
            .unwrap();
        write_blob(&store, "definition.json", &definition("stale"));
        assert_eq!(
            ActiveStage::read(&store).unwrap().leaf_kind,
            Some(LeafKind::Exec)
        );
        std::fs::remove_file(store.root_dir().join("definition.json")).unwrap();
        assert_eq!(
            ActiveStage::read(&store).unwrap().leaf_kind,
            Some(LeafKind::Exec)
        );
    }

    #[test]
    fn legacy_alias_is_used_only_without_revision() {
        let store = fixture();
        write_blob(&store, "definition.json", &definition("committed"));
        assert_eq!(
            ActiveStage::read(&store).unwrap().leaf_kind,
            Some(LeafKind::Exec)
        );
        for revision in [
            Value::Null,
            json!(42),
            json!(""),
            json!("../bad"),
            json!("missing"),
        ] {
            let mut tree = store.state_tree();
            tree.insert("definition_revision".into(), revision);
            store.write_state(&tree).unwrap();
            assert!(ActiveStage::read(&store).is_err());
        }
        write_blob(&store, "definition-revisions/malformed.json", b"not json");
        let mut tree = store.state_tree();
        tree.insert("definition_revision".into(), json!("malformed"));
        store.write_state(&tree).unwrap();
        assert!(ActiveStage::read(&store).is_err());
    }

    #[test]
    fn definition_uses_revision_from_same_stage_snapshot() {
        let store = fixture();
        store
            .initialize_definition(&definition("committed"))
            .unwrap();
        let snapshot = store.state_tree();
        store
            .publish_snapshot_conditional(&snapshot, &definition("replacement"))
            .unwrap();
        let definition = read_definition(&store, &snapshot).unwrap();
        assert_eq!(
            LeafPath::from_snapshot("committed", &snapshot)
                .unwrap()
                .resolve(&definition),
            Some(LeafKind::Exec)
        );
    }
}
