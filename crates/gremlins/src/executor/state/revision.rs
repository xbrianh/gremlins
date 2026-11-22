//! Backend-independent revision preparation and opaque history publication.
use serde_json::{Map, Value};

pub(super) fn prepare(
    state: &Map<String, Value>,
    bytes: &[u8],
    metadata: &Map<String, Value>,
) -> Map<String, Value> {
    let mut entry = metadata.clone();
    let snapshot = crate::definition::r#static::yaml::parse_expanded_json(bytes).ok();
    let selection = snapshot.as_ref().and_then(|value| {
        value
            .get("resolved_model_selection")
            .or_else(|| value.get("launch_model_selection"))
    });
    entry.entry("requested_selection").or_insert_with(|| {
        selection
            .and_then(|value| value.get("requested"))
            .cloned()
            .or_else(|| state.get("model_override").cloned())
            .unwrap_or(Value::Null)
    });
    if let Some(resolved) = selection.cloned().or_else(|| {
        snapshot
            .as_ref()
            .and_then(|value| value.get("default_model"))
            .cloned()
    }) {
        entry.entry("resolved_selection").or_insert(resolved);
    }
    entry
}

pub(super) fn append(
    state: &mut Map<String, Value>,
    id: String,
    metadata: &Map<String, Value>,
) -> Result<(), String> {
    let mut entry = metadata.clone();
    entry.insert("id".into(), Value::String(id.clone()));
    entry.insert(
        "predecessor".into(),
        state
            .get("definition_revision")
            .cloned()
            .unwrap_or(Value::Null),
    );
    entry.insert(
        "attempt".into(),
        state.get("attempt").cloned().unwrap_or(Value::Null),
    );
    entry.insert("timestamp".into(), Value::String(super::now_iso()));
    state
        .entry("definition_history")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or("invalid definition history")?
        .push(Value::Object(entry));
    state.insert("definition_revision".into(), Value::String(id));
    Ok(())
}
