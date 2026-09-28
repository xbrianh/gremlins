//! The typed stage tree.
//!
//! A parsed stage tree is a [`ParsedStage`] — one variant per stage type,
//! with composites holding their fully parsed children. The tree is pure
//! data: nothing here touches a client, a worktree, or the network.

use std::collections::HashMap;

use serde_yaml::{Mapping, Value};
use thiserror::Error;

use crate::schemas::error::SchemaError;
use crate::schemas::loader::{StageEntry, StageNode};
use crate::stages::agent::Agent;
use crate::stages::composite::{ClientSpec, StageAttrs};
use crate::stages::exec::Exec;
use crate::stages::parallel::ErrorPolicy;

/// Why a stage failed to parse.
#[derive(Error, Debug)]
pub enum StageError {
    /// A stage-level validation failure, already rendered for the operator.
    #[error("{0}")]
    Message(String),

    /// A failure from the shared schema layer (name filling).
    #[error(transparent)]
    Schema(#[from] SchemaError),
}

/// A parse failure is, at the definition level, a schema failure.
impl From<StageError> for SchemaError {
    fn from(err: StageError) -> Self {
        match err {
            StageError::Schema(source) => source,
            StageError::Message(message) => SchemaError::Generic(message),
        }
    }
}

/// One node of the parsed stage tree.
///
/// Leaf variants wrap the stage's own data plus the two things the tree adds:
/// the `skip_if_exists` guard and the stage's `client`. Composite variants
/// reuse the composite stage's parsed data and replace its raw
/// `Vec<serde_json::Value>` body with the fully parsed children.
#[derive(Debug, Clone)]
pub enum ParsedStage {
    Agent {
        stage: Agent,
        client: Option<ClientSpec>,
    },
    Exec {
        stage: Exec,
        client: Option<ClientSpec>,
    },
    Sequence {
        attrs: StageAttrs,
        max_iterations: u32,
        interval: Option<f64>,
        client: Option<ClientSpec>,
        body: Vec<ParsedStage>,
    },
    Parallel {
        attrs: StageAttrs,
        max_concurrent: Option<u32>,
        cancel_on_error: bool,
        error_policy: ErrorPolicy,
        client: Option<ClientSpec>,
        body: Vec<ParsedStage>,
    },
}

impl ParsedStage {
    /// The stage's name — its identity in state, artifacts, and errors.
    pub fn name(&self) -> &str {
        match self {
            ParsedStage::Agent { stage, .. } => &stage.name,
            ParsedStage::Exec { stage, .. } => &stage.name,
            ParsedStage::Sequence { attrs, .. } | ParsedStage::Parallel { attrs, .. } => {
                &attrs.name
            }
        }
    }

    /// The stage's type, as declared (`parallel` for a bare `parallel:` block).
    pub fn stage_type(&self) -> &str {
        match self {
            ParsedStage::Agent { .. } => "agent",
            ParsedStage::Exec { .. } => "exec",
            ParsedStage::Sequence { attrs, .. } | ParsedStage::Parallel { attrs, .. } => {
                &attrs.stage_type
            }
        }
    }

    /// The stage's own client, if it declared one.
    pub fn client(&self) -> Option<&ClientSpec> {
        match self {
            ParsedStage::Agent { client, .. }
            | ParsedStage::Exec { client, .. }
            | ParsedStage::Sequence { client, .. }
            | ParsedStage::Parallel { client, .. } => client.as_ref(),
        }
    }

    /// The artifact guard that makes the stage a conditional producer.
    pub fn skip_if_exists(&self) -> &str {
        match self {
            ParsedStage::Agent { .. } | ParsedStage::Exec { .. } => "",
            ParsedStage::Sequence { attrs, .. } | ParsedStage::Parallel { attrs, .. } => {
                &attrs.skip_if_exists
            }
        }
    }

    /// The stage's children — empty for leaves.
    pub fn body(&self) -> &[ParsedStage] {
        match self {
            ParsedStage::Agent { .. } | ParsedStage::Exec { .. } => &[],
            ParsedStage::Sequence { body, .. } | ParsedStage::Parallel { body, .. } => body,
        }
    }

    /// Build a [`StageEntry`] descriptor for the name-filling pass.
    pub fn to_stage_entry(&self) -> StageEntry {
        let name = self.name();
        StageEntry {
            name: if name.is_empty() {
                None
            } else {
                Some(name.to_string())
            },
            auto_name: None,
            stage_type: Some(self.stage_type().to_string()),
        }
    }

    /// Overwrite the stage's name.
    pub fn set_name(&mut self, name: String) {
        match self {
            ParsedStage::Agent { stage, .. } => stage.name = name,
            ParsedStage::Exec { stage, .. } => stage.name = name,
            ParsedStage::Sequence { attrs, .. } | ParsedStage::Parallel { attrs, .. } => {
                attrs.name = name
            }
        }
    }

    /// Serialize this stage (and its children, recursively) to a
    /// [`serde_yaml::Value`] matching the canonical expanded-YAML shape.
    pub fn to_yaml(&self) -> Value {
        match self {
            ParsedStage::Agent { stage, client } => agent_to_yaml(stage, client),
            ParsedStage::Exec { stage, client } => exec_to_yaml(stage, client),
            ParsedStage::Sequence {
                attrs,
                max_iterations,
                interval,
                client,
                body,
            } => sequence_to_yaml(attrs, *max_iterations, *interval, client, body),
            ParsedStage::Parallel {
                attrs,
                max_concurrent,
                cancel_on_error,
                error_policy,
                client,
                body,
            } => parallel_to_yaml(
                attrs,
                *max_concurrent,
                *cancel_on_error,
                *error_policy,
                client,
                body,
            ),
        }
    }

    /// Flatten this subtree into the schema layer's [`StageNode`] snapshot.
    ///
    /// The producer/consumer validators walk this form. Names are read from the
    /// typed tree, so auto-filled names of nested stages are visible to the
    /// validators — unlike a snapshot taken from the raw YAML, whose nested
    /// mappings never receive their filled names.
    pub fn to_stage_node(&self) -> StageNode {
        let (bind_map, interpolation_map) = match self {
            ParsedStage::Agent { stage, .. } => {
                (stage.bind_map.clone(), stage.interpolation_map.clone())
            }
            ParsedStage::Exec { stage, .. } => {
                (stage.bind_map.clone(), stage.interpolation_map.clone())
            }
            ParsedStage::Sequence { .. } | ParsedStage::Parallel { .. } => {
                (HashMap::new(), HashMap::new())
            }
        };

        StageNode {
            name: self.name().to_string(),
            stage_type: self.stage_type().to_string(),
            bind_map,
            interpolation_map,
            skip_if_exists: self.skip_if_exists().to_string(),
            body: self.body().iter().map(ParsedStage::to_stage_node).collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// Serialization helpers — build serde_yaml::Value trees
// ---------------------------------------------------------------------------

/// Omit keys whose value is an empty mapping, an empty sequence, a null, or
/// an empty string.
fn is_empty_value(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Sequence(seq) => seq.is_empty(),
        Value::Mapping(map) => map.is_empty(),
        _ => false,
    }
}

/// Insert `key` → `value` into `mapping` unless `value` is empty.
fn insert_if_nonempty(mapping: &mut Mapping, key: &str, value: Value) {
    if !is_empty_value(&value) {
        mapping.insert(Value::String(key.to_string()), value);
    }
}

/// Insert `key` → `Value::String(value)` unless `value` is empty.
fn insert_str_if_nonempty(mapping: &mut Mapping, key: &str, value: &str) {
    if !value.is_empty() {
        mapping.insert(
            Value::String(key.to_string()),
            Value::String(value.to_string()),
        );
    }
}

/// Serialize a `HashMap<String, String>` to a YAML mapping, omitting empty.
fn string_map_to_yaml(map: &HashMap<String, String>) -> Value {
    if map.is_empty() {
        return Value::Mapping(Mapping::new());
    }
    let mut out = Mapping::with_capacity(map.len());
    for (k, v) in map {
        out.insert(Value::String(k.clone()), Value::String(v.clone()));
    }
    Value::Mapping(out)
}

/// Convert `serde_json::Value` options to a YAML mapping, filtering out
/// framework-substituted keys (`cwd`, `base_ref`) that the runtime injects.
fn options_to_yaml(options: &HashMap<String, serde_json::Value>) -> Value {
    // Filter out keys that the runtime injects at execution time — they're
    // not part of the user-visible definition. We intentionally do NOT filter
    // all FRAMEWORK_KEYS here: model is a valid user-facing option for agent
    // stages, and name is validated out by the builder.
    // Iterate directly to avoid an intermediate HashMap allocation.
    let mut out = Mapping::new();
    for (k, v) in options
        .iter()
        .filter(|(k, _)| k.as_str() != "cwd" && k.as_str() != "base_ref")
    {
        if let Ok(yaml_val) = serde_yaml::to_value(v) {
            out.insert(Value::String(k.clone()), yaml_val);
        }
    }
    Value::Mapping(out)
}

fn client_to_yaml(client: &Option<ClientSpec>) -> Option<Value> {
    client.as_ref().map(|c| Value::String(c.0.clone()))
}

fn agent_to_yaml(stage: &Agent, client: &Option<ClientSpec>) -> Value {
    let mut m = Mapping::new();
    m.insert(
        Value::String("name".to_string()),
        Value::String(stage.name.clone()),
    );
    m.insert(
        Value::String("type".to_string()),
        Value::String("agent".to_string()),
    );
    // prompt: omit if empty list (same as what the YAML path accepts)
    if !stage.prompts.is_empty() {
        let prompts: Vec<Value> = stage
            .prompts
            .iter()
            .map(|p| Value::String(p.clone()))
            .collect();
        m.insert(
            Value::String("prompt".to_string()),
            Value::Sequence(prompts),
        );
    }
    insert_if_nonempty(&mut m, "options", options_to_yaml(&stage.options));
    insert_if_nonempty(
        &mut m,
        "interpolation",
        string_map_to_yaml(&stage.interpolation_map),
    );
    insert_if_nonempty(&mut m, "bind", string_map_to_yaml(&stage.bind_map));
    if let Some(client_val) = client_to_yaml(client) {
        m.insert(Value::String("client".to_string()), client_val);
    }
    Value::Mapping(m)
}

fn exec_to_yaml(stage: &Exec, client: &Option<ClientSpec>) -> Value {
    let mut m = Mapping::new();
    m.insert(
        Value::String("name".to_string()),
        Value::String(stage.name.clone()),
    );
    m.insert(
        Value::String("type".to_string()),
        Value::String("exec".to_string()),
    );
    insert_if_nonempty(&mut m, "options", options_to_yaml(&stage.options));
    insert_if_nonempty(
        &mut m,
        "interpolation",
        string_map_to_yaml(&stage.interpolation_map),
    );
    insert_if_nonempty(&mut m, "bind", string_map_to_yaml(&stage.bind_map));
    if let Some(client_val) = client_to_yaml(client) {
        m.insert(Value::String("client".to_string()), client_val);
    }
    Value::Mapping(m)
}

fn sequence_to_yaml(
    attrs: &StageAttrs,
    max_iterations: u32,
    interval: Option<f64>,
    client: &Option<ClientSpec>,
    body: &[ParsedStage],
) -> Value {
    let mut m = Mapping::new();
    m.insert(
        Value::String("name".to_string()),
        Value::String(attrs.name.clone()),
    );
    m.insert(
        Value::String("type".to_string()),
        Value::String("sequence".to_string()),
    );
    if max_iterations > 1 {
        m.insert(
            Value::String("max-iterations".to_string()),
            Value::Number((max_iterations as i64).into()),
        );
    }
    if let Some(interval_secs) = interval {
        m.insert(
            Value::String("interval".to_string()),
            serde_yaml::to_value(interval_secs).unwrap_or(Value::Null),
        );
    }
    if let Some(client_val) = client_to_yaml(client) {
        m.insert(Value::String("client".to_string()), client_val);
    }
    insert_str_if_nonempty(&mut m, "skip_if_exists", &attrs.skip_if_exists);
    let children: Vec<Value> = body.iter().map(ParsedStage::to_yaml).collect();
    m.insert(Value::String("body".to_string()), Value::Sequence(children));
    Value::Mapping(m)
}

fn parallel_to_yaml(
    attrs: &StageAttrs,
    max_concurrent: Option<u32>,
    cancel_on_error: bool,
    error_policy: ErrorPolicy,
    client: &Option<ClientSpec>,
    body: &[ParsedStage],
) -> Value {
    let mut m = Mapping::new();
    m.insert(
        Value::String("name".to_string()),
        Value::String(attrs.name.clone()),
    );
    m.insert(
        Value::String("type".to_string()),
        Value::String("parallel".to_string()),
    );
    if let Some(mc) = max_concurrent {
        m.insert(
            Value::String("max_concurrent".to_string()),
            Value::Number((mc as i64).into()),
        );
    }
    if cancel_on_error {
        m.insert(
            Value::String("cancel_on_error".to_string()),
            Value::Bool(true),
        );
    }
    if error_policy != ErrorPolicy::Any {
        m.insert(
            Value::String("error_policy".to_string()),
            Value::String(error_policy.as_str().to_string()),
        );
    }
    if let Some(client_val) = client_to_yaml(client) {
        m.insert(Value::String("client".to_string()), client_val);
    }
    insert_str_if_nonempty(&mut m, "skip_if_exists", &attrs.skip_if_exists);
    let children: Vec<Value> = body.iter().map(ParsedStage::to_yaml).collect();
    m.insert(
        Value::String("parallel".to_string()),
        Value::Sequence(children),
    );
    Value::Mapping(m)
}
