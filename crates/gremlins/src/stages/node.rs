//! The typed stage tree.
//!
//! An expanded definition YAML is a list of mappings: a `type` (or a bare
//! `parallel:` block), a name that may be absent, and optionally a body of
//! child stages. [`ParsedStage::parse_stages`] turns that list into a
//! [`ParsedStage`] tree — names filled first so every stage has a stable
//! identity, then one variant per stage type, recursing through composite
//! bodies. The tree is pure data: nothing here touches a client, a worktree,
//! or the network.
//!
//! [`ParsedStage::parse`] mirrors `pyext::schemas::loader::parse_stage`
//! rule for rule so the two parse paths agree while both exist: `parallel`
//! sugar, `max_concurrent` only on parallel groups, the nested-parallel
//! rejection, and per-composite child-name validation.

use std::collections::HashMap;

use serde_yaml::{Mapping, Value};
use thiserror::Error;

use crate::schemas::error::SchemaError;
use crate::schemas::loader::{self as schema_loader, StageEntry, StageNode};
use crate::stages::agent::Agent;
use crate::stages::composite::{get_client_from_dict, ClientSpec, StageAttrs};
use crate::stages::exec::Exec;
use crate::stages::parallel::{validate_child_names, ErrorPolicy, ParallelGroup};
use crate::stages::sequence::Sequence;

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
        skip_if_exists: String,
        client: Option<ClientSpec>,
    },
    Exec {
        stage: Exec,
        skip_if_exists: String,
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
    /// Parse a stage list: fill names across the siblings first, then parse
    /// each entry. `depth` is the composite nesting level of the list.
    pub fn parse_stages(
        stages: &mut [Value],
        depth: usize,
    ) -> Result<Vec<ParsedStage>, StageError> {
        fill_names(stages)?;
        stages
            .iter()
            .map(|value| ParsedStage::parse(value, depth))
            .collect()
    }

    /// Parse a single stage mapping. `parallel` groups may not nest, so a
    /// `depth > 0` parallel is rejected by the group parser.
    pub fn parse(value: &Value, depth: usize) -> Result<ParsedStage, StageError> {
        let mapping = value
            .as_mapping()
            .ok_or_else(|| StageError::Message("each stage must be a mapping".to_string()))?;

        // A bare `parallel:` block is sugar for `type: parallel`; both spellings
        // converge on the same branch below.
        let is_parallel = mapping.contains_key("parallel");
        let name = match mapping.get("name").and_then(Value::as_str) {
            Some(name) => name.to_string(),
            None if is_parallel => "<parallel>".to_string(),
            None => String::new(),
        };

        if !is_parallel && mapping.contains_key("max_concurrent") {
            return Err(StageError::Message(format!(
                "stage {name:?}: 'max_concurrent' is only valid on parallel groups"
            )));
        }

        let stage_type = if is_parallel {
            "parallel"
        } else {
            match mapping.get("type").and_then(Value::as_str) {
                Some(kind) if !kind.is_empty() => kind,
                _ => {
                    return Err(StageError::Message(format!(
                        "stage {name:?}: must have a 'type' field"
                    )))
                }
            }
        };

        // Composites take their children out of the mapping as they descend, so
        // parse from an owned copy rather than the shared expanded tree.
        let mut mapping = mapping.clone();
        match stage_type {
            "parallel" => parse_parallel(&mut mapping, name, depth),
            "loop" => Err(StageError::Message(
                "the 'loop' stage type has been removed; use 'sequence' with max_iterations > 1"
                    .to_string(),
            )),
            "sequence" => parse_sequence(&mut mapping, name, depth),
            "exec" => parse_exec(&mapping, &name),
            "agent" => parse_agent(&mapping, &name),
            other => Err(StageError::Message(format!(
                "stage {name:?}: unknown type {other:?}"
            ))),
        }
    }

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
            ParsedStage::Agent { skip_if_exists, .. }
            | ParsedStage::Exec { skip_if_exists, .. } => skip_if_exists,
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
            is_parallel: self.stage_type() == "parallel",
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
            ParsedStage::Agent {
                stage,
                skip_if_exists,
                client,
            } => agent_to_yaml(stage, skip_if_exists, client),
            ParsedStage::Exec {
                stage,
                skip_if_exists,
                client,
            } => exec_to_yaml(stage, skip_if_exists, client),
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

fn parse_agent(mapping: &Mapping, name: &str) -> Result<ParsedStage, StageError> {
    let dict = json_map(mapping, name)?;
    let stage = Agent::from_dict(&dict).map_err(StageError::Message)?;
    let client = get_client_from_dict(&dict, name).map_err(StageError::Message)?;
    let skip_if_exists = parse_skip_if_exists(mapping, name)?;
    if !skip_if_exists.is_empty() {
        return Err(StageError::Message(
            "'skip_if_exists' is only valid on sequence and parallel stages".to_string(),
        ));
    }
    Ok(ParsedStage::Agent {
        stage,
        skip_if_exists,
        client,
    })
}

fn parse_exec(mapping: &Mapping, name: &str) -> Result<ParsedStage, StageError> {
    let dict = json_map(mapping, name)?;
    let stage = Exec::from_dict(&dict).map_err(StageError::Message)?;
    let client = get_client_from_dict(&dict, name).map_err(StageError::Message)?;
    let skip_if_exists = parse_skip_if_exists(mapping, name)?;
    if !skip_if_exists.is_empty() {
        return Err(StageError::Message(
            "'skip_if_exists' is only valid on sequence and parallel stages".to_string(),
        ));
    }
    Ok(ParsedStage::Exec {
        stage,
        skip_if_exists,
        client,
    })
}

fn parse_sequence(
    mapping: &mut Mapping,
    name: String,
    depth: usize,
) -> Result<ParsedStage, StageError> {
    let dict = json_map(mapping, &name)?;
    let mut parsed = Sequence::with_dict(&dict).map_err(StageError::Message)?;
    parsed.attrs.skip_if_exists = parse_skip_if_exists(mapping, &name)?;
    let body = parse_body(mapping, "body", depth)?;
    Ok(ParsedStage::Sequence {
        attrs: parsed.attrs,
        max_iterations: parsed.max_iterations,
        interval: parsed.interval,
        client: parsed.client,
        body,
    })
}

fn parse_parallel(
    mapping: &mut Mapping,
    name: String,
    depth: usize,
) -> Result<ParsedStage, StageError> {
    let dict = json_map(mapping, &name)?;
    let mut parsed = ParallelGroup::with_dict(&dict, depth).map_err(StageError::Message)?;
    parsed.attrs.skip_if_exists = parse_skip_if_exists(mapping, &name)?;
    let body = parse_body(mapping, "parallel", depth + 1)?;
    // Child names are only knowable once the children have been parsed.
    let child_names: Vec<String> = body.iter().map(|child| child.name().to_string()).collect();
    validate_child_names(&parsed.attrs.name, &child_names).map_err(StageError::Message)?;
    Ok(ParsedStage::Parallel {
        attrs: parsed.attrs,
        max_concurrent: parsed.max_concurrent,
        cancel_on_error: parsed.cancel_on_error,
        error_policy: parsed.error_policy,
        client: parsed.client,
        body,
    })
}

/// Take a stage list out of `key` and parse it. The entry's shape was already
/// validated by the composite's own parser, so a non-sequence yields nothing.
fn parse_body(
    mapping: &mut Mapping,
    key: &str,
    depth: usize,
) -> Result<Vec<ParsedStage>, StageError> {
    let mut children = match mapping.remove(key) {
        Some(Value::Sequence(children)) => children,
        _ => Vec::new(),
    };
    ParsedStage::parse_stages(&mut children, depth)
}

/// Fill a stage list's names in place, then strip the internal `_auto_name`
/// key the recipe expander leaves behind. Mirrors the pyext loader: the shared
/// [`schema_loader::fill_names`] does the work, and the result is written back
/// onto each mapping so later passes (and the `land` builder) see the names.
fn fill_names(stages: &mut [Value]) -> Result<(), StageError> {
    let mut entries: Vec<StageEntry> = stages.iter().map(stage_entry).collect();
    schema_loader::fill_names(&mut entries)?;

    for (value, entry) in stages.iter_mut().zip(&entries) {
        let Some(mapping) = value.as_mapping_mut() else {
            continue;
        };
        if let Some(name) = &entry.name {
            mapping.insert(
                Value::String("name".to_string()),
                Value::String(name.clone()),
            );
        }
        mapping.remove("_auto_name");
    }
    Ok(())
}

/// Project one raw stage mapping onto the descriptor `fill_names` consumes.
fn stage_entry(value: &Value) -> StageEntry {
    let mapping = value.as_mapping();
    let name = mapping
        .and_then(|entry| entry.get("name"))
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .map(String::from);
    let auto_name = mapping
        .and_then(|entry| entry.get("_auto_name"))
        .map(python_str)
        .filter(|name| !name.is_empty());
    let stage_type = mapping
        .and_then(|entry| entry.get("type"))
        .and_then(Value::as_str)
        .filter(|kind| !kind.is_empty())
        .map(String::from);
    StageEntry {
        name,
        auto_name,
        stage_type,
        is_parallel: mapping.is_some_and(|entry| entry.contains_key("parallel")),
    }
}

/// `str(value)` as Python renders it — how the pyext loader read an inherited
/// `_auto_name`.
fn python_str(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.clone(),
        other => format!("{other:?}"),
    }
}

fn parse_skip_if_exists(mapping: &Mapping, name: &str) -> Result<String, StageError> {
    match mapping.get("skip_if_exists") {
        None => Ok(String::new()),
        Some(value) => value.as_str().map(String::from).ok_or_else(|| {
            StageError::Message(format!(
                "stage {name:?}: 'skip_if_exists' must be a string, got {} type",
                yaml_type_name(value)
            ))
        }),
    }
}

/// A mapping projected onto the JSON-shaped dict the stage parsers take.
fn json_map(
    mapping: &Mapping,
    name: &str,
) -> Result<HashMap<String, serde_json::Value>, StageError> {
    let mut out = HashMap::with_capacity(mapping.len());
    for (key, value) in mapping {
        let key = key
            .as_str()
            .ok_or_else(|| StageError::Message(format!("stage {name:?}: keys must be strings")))?;
        let value = serde_json::to_value(value)
            .map_err(|e| StageError::Message(format!("stage {name:?}: {e}")))?;
        out.insert(key.to_string(), value);
    }
    Ok(out)
}

/// The Python type name of a YAML value, for type-mismatch messages.
fn yaml_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(number) if number.is_i64() || number.is_u64() => "int",
        Value::Number(_) => "float",
        Value::String(_) => "str",
        Value::Sequence(_) => "list",
        Value::Mapping(_) => "dict",
        Value::Tagged(_) => "object",
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

fn agent_to_yaml(stage: &Agent, skip_if_exists: &str, client: &Option<ClientSpec>) -> Value {
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
    insert_str_if_nonempty(&mut m, "skip_if_exists", skip_if_exists);
    Value::Mapping(m)
}

fn exec_to_yaml(stage: &Exec, skip_if_exists: &str, client: &Option<ClientSpec>) -> Value {
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
    insert_str_if_nonempty(&mut m, "skip_if_exists", skip_if_exists);
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
        let mut opts = Mapping::new();
        opts.insert(
            Value::String("interval".to_string()),
            serde_yaml::to_value(interval_secs).unwrap_or(Value::Null),
        );
        m.insert(Value::String("options".to_string()), Value::Mapping(opts));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(yaml: &str) -> Result<Vec<ParsedStage>, StageError> {
        let mut value: Value = serde_yaml::from_str(yaml).expect("valid YAML");
        let list = value.as_sequence_mut().expect("a stage list");
        ParsedStage::parse_stages(list, 0)
    }

    fn parse_one(yaml: &str) -> Result<ParsedStage, StageError> {
        let value: Value = serde_yaml::from_str(yaml).expect("valid YAML");
        ParsedStage::parse(&value, 0)
    }

    #[test]
    fn parses_agent_and_exec_leaves() {
        let stages = parse_all(
            r#"
- type: agent
  prompt:
    - "hello\n"
- type: exec
  options:
    cmds: ["echo hi"]
"#,
        )
        .unwrap();

        assert_eq!(stages.len(), 2);
        assert_eq!(stages[0].name(), "agent");
        assert_eq!(stages[0].stage_type(), "agent");
        assert_eq!(stages[1].name(), "exec");
        assert_eq!(stages[1].stage_type(), "exec");

        match &stages[0] {
            ParsedStage::Agent { stage, .. } => {
                assert_eq!(stage.prompts, vec!["hello\n".to_string()]);
            }
            other => panic!("expected agent, got {other:?}"),
        }
        match &stages[1] {
            ParsedStage::Exec { stage, .. } => {
                assert_eq!(
                    stage.options.get("cmds").unwrap(),
                    &serde_json::json!(["echo hi"])
                );
            }
            other => panic!("expected exec, got {other:?}"),
        }
    }

    #[test]
    fn unnamed_siblings_get_type_suffixed_names() {
        let stages = parse_all(
            r#"
- type: agent
- type: agent
- type: agent
"#,
        )
        .unwrap();
        let names: Vec<&str> = stages.iter().map(ParsedStage::name).collect();
        assert_eq!(names, vec!["agent", "agent-2", "agent-3"]);
    }

    #[test]
    fn explicit_names_survive() {
        let stages = parse_all(
            r#"
- name: plan
  type: agent
- type: agent
"#,
        )
        .unwrap();
        let names: Vec<&str> = stages.iter().map(ParsedStage::name).collect();
        assert_eq!(names, vec!["plan", "agent"]);
    }

    #[test]
    fn parallel_sugar_parses_children() {
        let stages = parse_all(
            r#"
- parallel:
    - type: exec
      options:
        cmds: ["true"]
    - type: exec
      options:
        cmds: ["true"]
"#,
        )
        .unwrap();

        assert_eq!(stages.len(), 1);
        assert_eq!(stages[0].name(), "parallel");
        assert_eq!(stages[0].stage_type(), "parallel");
        let body = stages[0].body();
        assert_eq!(body.len(), 2);
        assert_eq!(body[0].name(), "exec");
        assert_eq!(body[1].name(), "exec-2");
    }

    #[test]
    fn parallel_options_are_preserved() {
        let stages = parse_all(
            r#"
- name: group
  parallel:
    - type: exec
      options:
        cmds: ["true"]
  max_concurrent: 3
  cancel_on_error: true
  error_policy: all
"#,
        )
        .unwrap();

        match &stages[0] {
            ParsedStage::Parallel {
                max_concurrent,
                cancel_on_error,
                error_policy,
                ..
            } => {
                assert_eq!(*max_concurrent, Some(3));
                assert!(cancel_on_error);
                assert_eq!(*error_policy, ErrorPolicy::All);
            }
            other => panic!("expected parallel, got {other:?}"),
        }
    }

    #[test]
    fn nested_parallel_is_rejected() {
        let err = parse_all(
            r#"
- parallel:
    - type: exec
      options:
        cmds: ["true"]
    - parallel:
        - type: exec
          options:
            cmds: ["true"]
"#,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("nested parallel groups are not allowed"),
            "{err}"
        );
    }

    #[test]
    fn loop_type_is_rejected_with_migration_hint() {
        let err = parse_one("type: loop\nmax-iterations: 2\nbody: []\n").unwrap_err();
        assert!(err.to_string().contains("has been removed"), "{err}");
    }

    #[test]
    fn sequence_body_is_a_typed_tree() {
        let stages = parse_all(
            r#"
- type: sequence
  body:
    - type: exec
      options:
        cmds: ["one"]
    - type: sequence
      body:
        - type: exec
          options:
            cmds: ["two"]
"#,
        )
        .unwrap();

        assert_eq!(stages[0].stage_type(), "sequence");
        assert_eq!(stages[0].body().len(), 2);
        assert_eq!(stages[0].body()[1].stage_type(), "sequence");
        assert_eq!(stages[0].body()[1].body().len(), 1);
    }

    #[test]
    fn skip_if_exists_is_carried() {
        let stages = parse_all(
            r#"
- type: sequence
  skip_if_exists: "artifact://done"
  body:
    - type: exec
      options:
        cmds: ["true"]
"#,
        )
        .unwrap();
        assert_eq!(stages[0].skip_if_exists(), "artifact://done");
    }

    #[test]
    fn skip_if_exists_must_be_a_string() {
        let err = parse_one("type: agent\nskip_if_exists: 3\n").unwrap_err();
        assert!(
            err.to_string()
                .contains("'skip_if_exists' must be a string"),
            "{err}"
        );
    }

    #[test]
    fn stage_client_is_parsed() {
        let stages = parse_all(
            r#"
- type: agent
  client: "xai:grok-5"
  prompt:
    - "hi\n"
"#,
        )
        .unwrap();
        assert_eq!(stages[0].client(), Some(&ClientSpec("xai:grok-5".into())));
    }

    #[test]
    fn stage_client_must_be_a_string() {
        let err = parse_one("type: agent\nclient: 42\n").unwrap_err();
        assert!(
            err.to_string().contains("'client' must be a string"),
            "{err}"
        );
    }

    #[test]
    fn missing_type_is_rejected() {
        let err = parse_one("name: orphan\n").unwrap_err();
        assert!(
            err.to_string().contains("must have a 'type' field"),
            "{err}"
        );
    }

    #[test]
    fn unknown_type_is_rejected() {
        let err = parse_one("type: banana\n").unwrap_err();
        assert!(err.to_string().contains("unknown type \"banana\""), "{err}");
    }

    #[test]
    fn max_concurrent_is_only_valid_on_parallel() {
        let err = parse_one("type: exec\nmax_concurrent: 2\n").unwrap_err();
        assert!(
            err.to_string()
                .contains("'max_concurrent' is only valid on parallel groups"),
            "{err}"
        );
    }

    #[test]
    fn in_and_out_keys_are_rejected() {
        let err = parse_one("type: agent\nin:\n  a: b\n").unwrap_err();
        assert!(
            err.to_string()
                .contains("'in'/'out' keys are no longer supported"),
            "{err}"
        );
    }

    #[test]
    fn duplicate_parallel_child_names_are_disambiguated() {
        // Name filling runs before child-name validation, so a repeated
        // explicit name is renamed rather than rejected — the pyext path
        // behaves identically.
        let stages = parse_all(
            r#"
- name: group
  parallel:
    - name: shard
      type: exec
      options:
        cmds: ["true"]
    - name: shard
      type: exec
      options:
        cmds: ["true"]
"#,
        )
        .unwrap();
        let names: Vec<&str> = stages[0].body().iter().map(ParsedStage::name).collect();
        assert_eq!(names, vec!["shard", "shard-2"]);
    }
}
