use rig_core::completion::ToolDefinition;

use super::tool_specs::{
    always_available, ToolSpec, MAX_CONCURRENT_TOOLS, TASK_MAX_PER_TURN, TOOL_SPECS,
};

/// Build the JSON-schema `parameters` object for a tool from its [`ToolSpec`].
/// Field names, descriptions and the `required` set all come from the table;
/// only Edit's nested `edits` item schema and Task's `kind` enum are bespoke.
fn tool_parameters(spec: &ToolSpec) -> serde_json::Value {
    let mut properties = serde_json::Map::new();
    for field in spec.fields {
        let value = match (spec.name, field.name) {
            ("Edit", "edits") => serde_json::json!({
                "type": "array",
                "minItems": 1,
                "items": {
                    "type": "object",
                    "properties": {
                        "old_string": {
                            "type": "string",
                            "description": "Exact text to replace — must be unique in the file (or unique within start_line/end_line scope)"
                        },
                        "new_string": {
                            "type": "string",
                            "description": "Replacement text"
                        },
                        "start_line": {
                            "type": "integer",
                            "description": "When old_string appears multiple times, scope the search to start at this 1-based line (inclusive; clamps to EOF if beyond last line)"
                        },
                        "end_line": {
                            "type": "integer",
                            "description": "When old_string appears multiple times, scope the search to end at this 1-based line (inclusive; clamps to EOF if beyond last line)"
                        }
                    },
                    "additionalProperties": false,
                    "required": ["old_string", "new_string"]
                }            }),
            ("Task", "kind") => serde_json::json!({
                "type": "string",
                "enum": crate::task_kind::TaskKind::ALL
                    .iter()
                    .map(|kind| serde_json::Value::String(kind.as_str().to_string()))
                    .collect::<Vec<_>>(),
                "description": field.description
            }),
            _ => serde_json::json!({
                "type": field.ty,
                "description": field.description
            }),
        };
        properties.insert(field.name.to_string(), value);
    }
    let required: Vec<&str> = spec
        .fields
        .iter()
        .filter(|f| f.required)
        .map(|f| f.name)
        .collect();
    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

pub(crate) fn tool_definitions(
    filter: Option<&[String]>,
    is_chat: bool,
    mode: super::tool_specs::ToolMode,
) -> Vec<ToolDefinition> {
    if mode == super::tool_specs::ToolMode::NoTools {
        return Vec::new();
    }
    let mut all: Vec<ToolDefinition> = Vec::new();
    for spec in TOOL_SPECS {
        // Chat-only tools are omitted from tool definitions in headless
        // mode. The `chat_only` flag on ToolSpec is the single source of
        // truth for this — it matches the `chat_only()` trait hook on the
        // corresponding handler.
        if spec.chat_only && !is_chat {
            continue;
        }
        let description = spec
            .description
            .replace("{TASK_MAX_PER_TURN}", &TASK_MAX_PER_TURN.to_string())
            .replace("{MAX_CONCURRENT_TOOLS}", &MAX_CONCURRENT_TOOLS.to_string());
        all.push(ToolDefinition {
            name: spec.name.into(),
            description,
            parameters: tool_parameters(spec),
        });
    }
    match filter {
        Some(names) => all
            .into_iter()
            .filter(|t| {
                always_available(&t.name)
                    || (is_chat && {
                        // Chat-only tools bypass the filter in chat mode.
                        // We look up the original spec to check `chat_only`
                        // so the flag stays the single source of truth.
                        TOOL_SPECS
                            .iter()
                            .find(|s| s.name == t.name)
                            .map(|s| s.chat_only)
                            .unwrap_or(false)
                    })
                    || names.iter().any(|n| n == &t.name)
            })
            .collect(),
        None => all,
    }
}
