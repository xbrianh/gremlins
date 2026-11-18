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
            path.resolve(&read_definition(store)?)
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

fn read_definition(store: &dyn StateStore) -> Result<StaticDefinition, String> {
    let mut bytes = Vec::new();
    let mut blob = store
        .open("definition.json", BlobMode::Read)
        .map_err(|e| format!("failed to read definition snapshot: {e}"))?;
    blob.read_to_end(&mut bytes)
        .map_err(|e| format!("failed to read definition snapshot: {e}"))?;
    let default_model = config::global_config()
        .ok()
        .and_then(|c| c.default_model().map(String::from));
    StaticDefinition::from_expanded_bytes(&bytes, None, default_model.as_deref())
        .map_err(|e| format!("failed to load definition: {e}"))
}
