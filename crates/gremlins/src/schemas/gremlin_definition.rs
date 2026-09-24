//! The native `GremlinDefinition` — an expanded gremlin definition YAML resolved to typed data.
//!
//! [`GremlinDefinition::from_yaml`] mirrors `pyext::schemas::GremlinDefinition::from_yaml` step
//! for step: canonicalise and locate the file, walk up to the `.gremlins`
//! project root, expand the YAML, then resolve the definition identity (`name`,
//! `default_client`, `base_ref`, `bootstrap`), the typed stage tree, the
//! optional `land` stage, and the producer/consumer validators that guard
//! artifact wiring.
//!
//! It is synchronous and free of PyO3. The runtime layer — client
//! construction, worktrees, bootstrap execution — is a later concern;
//! `default_client` is kept as the declared string so this layer never has to
//! know how a client is built.

use std::path::{Path, PathBuf};

use serde_yaml::{Mapping, Value};

use crate::config;
use crate::schemas::bootstrap::Bootstrap;
use crate::schemas::error::SchemaError;
use crate::schemas::expand;
use crate::schemas::loader::{self, StageNode};
use crate::stages::node::RunnableStage;

/// The message emitted when no layer supplied a default client.
const MISSING_DEFAULT_CLIENT: &str = "gremlin definition is missing 'default_client' — set a \
     'default_client' in the definition YAML, pass --client on the command line, or set \
     'default-client' in config.json";

/// A gremlin definition resolved from an expanded YAML file.
#[derive(Debug, Clone)]
pub struct GremlinDefinition {
    /// The definition's identity: the YAML file stem.
    pub name: String,
    /// Where the definition was loaded from, canonicalised where possible.
    pub path: PathBuf,
    /// Every stage runs with this client unless it declares its own.
    pub default_client: String,
    /// The git ref the worktree branches from.
    pub base_ref: String,
    /// Bootstrap commands and input sources.
    pub bootstrap: Bootstrap,
    /// The stage tree, in declaration order.
    pub stages: Vec<RunnableStage>,
    /// The optional `land` stage — always an exec stage named `land`.
    pub land: Option<RunnableStage>,
    /// The fully expanded YAML tree, kept so that [`GremlinDefinition::validate`]
    /// can run every validator without re-parsing.
    pub expanded_yaml: Value,
}

/// The name a not-yet-loaded definition carries. [`Gremlin::init_runtime`]
/// treats it as "nothing loaded yet", so it must never be a real definition's
/// name — `from_yaml` derives that from the YAML file stem.
pub const UNLOADED_NAME: &str = "unknown";

impl GremlinDefinition {
    /// A placeholder carrying no identity: the value a [`Gremlin`] holds until
    /// [`Gremlin::init_runtime`] reads the real YAML.
    ///
    /// [`Gremlin`]: crate::executor::gremlin::Gremlin
    /// [`Gremlin::init_runtime`]: crate::executor::gremlin::Gremlin::init_runtime
    pub fn stub() -> GremlinDefinition {
        GremlinDefinition {
            name: UNLOADED_NAME.to_string(),
            path: PathBuf::from("."),
            default_client: String::new(),
            base_ref: String::new(),
            bootstrap: Bootstrap::default(),
            stages: Vec::new(),
            land: None,
            expanded_yaml: Value::Null,
        }
    }

    /// Whether this definition is the [`GremlinDefinition::stub`] rather than a loaded one.
    pub fn is_stub(&self) -> bool {
        self.name.is_empty() || self.name == UNLOADED_NAME
    }

    /// Clone this definition, replacing its stage list with `stages`.
    ///
    /// Used by the parallel executor to give each child a definition that
    /// contains only the child's own stage(s), while inheriting every other
    /// field (name, path, default_client, base_ref, bootstrap, land) from
    /// the parent.
    pub fn clone_with_stages(&self, stages: Vec<RunnableStage>) -> Self {
        GremlinDefinition {
            stages,
            ..self.clone()
        }
    }

    /// Load and resolve a gremlin definition. `default_client_override` is the CLI
    /// `--client` value; it is consulted only when the YAML declares none.
    ///
    /// Does not validate — call [`GremlinDefinition::validate`] afterward to
    /// check duplicate producers, unresolved consumers, and unused stage keys.
    pub fn from_yaml(
        path: impl AsRef<Path>,
        default_client_override: Option<&str>,
    ) -> Result<GremlinDefinition, SchemaError> {
        let path = path.as_ref();
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if !path.exists() {
            return Err(SchemaError::DefinitionFileNotFound {
                path: path.display().to_string(),
            });
        }

        let project_root = project_root_for(&path);
        let expanded = expand::parse_definition_file(&path, &project_root)?;

        Self::from_expanded_value(expanded, path, default_client_override)
    }

    /// Load an already-expanded YAML file directly — no expansion, no project-root
    /// walk. Used by [`Gremlin::from`] when a hermetic `definition.yaml` exists
    /// alongside the state directory.
    ///
    /// Strips the `__gremlins_expanded__` sentinel if present, but tolerates its
    /// absence.
    ///
    /// [`Gremlin::from`]: crate::executor::gremlin::Gremlin::from
    pub fn from_expanded_yaml(
        path: impl AsRef<Path>,
        default_client_override: Option<&str>,
    ) -> Result<GremlinDefinition, SchemaError> {
        let path = path.as_ref();
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if !path.exists() {
            return Err(SchemaError::DefinitionFileNotFound {
                path: path.display().to_string(),
            });
        }

        let mut expanded = expand::load_yaml_file(&path)?;
        // Strip the sentinel if present — the file may lack it but still be
        // fully expanded.
        if let Some(mapping) = expanded.as_mapping_mut() {
            mapping.remove(Value::from("__gremlins_expanded__"));
        }

        Self::from_expanded_value(expanded, path, default_client_override)
    }

    /// Shared extraction: turn an already-expanded YAML [`Value`] into a typed
    /// [`GremlinDefinition`]. Both [`from_yaml`] and [`from_expanded_yaml`]
    /// funnel through here once they have the expanded tree.
    fn from_expanded_value(
        expanded: Value,
        path: PathBuf,
        default_client_override: Option<&str>,
    ) -> Result<GremlinDefinition, SchemaError> {
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("")
            .to_string();

        let root = expanded
            .as_mapping()
            .ok_or_else(|| SchemaError::YamlNotMapping {
                label: path.display().to_string(),
                got: format!("{expanded:?}"),
            })?;

        let yaml_default_client = default_client_from_yaml(root)?;
        let base_ref = base_ref_from_yaml(root)?;

        let mut raw_stages = stages_from_yaml(root)?;
        let stages = RunnableStage::parse_stages(&mut raw_stages, 0)?;

        if root.contains_key("inputs") {
            return Err(SchemaError::Generic(
                "'inputs' is not a valid definition key; declare CLI arguments under bootstrap.source"
                    .to_string(),
            ));
        }

        let bootstrap = match root.get("bootstrap") {
            None | Some(Value::Null) => Bootstrap::default(),
            Some(value) => Bootstrap::from_yaml(Some(value))?,
        };

        let land = land_from_yaml(root)?;

        let default_client = resolve_default_client(yaml_default_client, default_client_override)?;

        Ok(GremlinDefinition {
            name,
            path,
            default_client,
            base_ref,
            bootstrap,
            stages,
            land,
            expanded_yaml: expanded,
        })
    }

    /// Run all three semantic validators against the already-loaded definition.
    ///
    /// This lets a caller load without validation then validate later without
    /// re-parsing the YAML. Uses the stored expanded YAML tree to run
    /// `validate_stage_keys` in addition to the typed-tree validators.
    pub fn validate(&self) -> Result<(), SchemaError> {
        if let Err(errors) = expand::validate_stage_keys(&self.expanded_yaml) {
            return Err(errors.into_iter().next().unwrap());
        }
        let nodes: Vec<StageNode> = self
            .stages
            .iter()
            .map(RunnableStage::to_stage_node)
            .collect();
        loader::check_duplicate_producers(&nodes, &self.bootstrap.cli_out)?;
        loader::check_unresolved_consumers(
            &nodes,
            &self.bootstrap.launch_cmds,
            &self.bootstrap.cli_out,
        )?;
        Ok(())
    }

    /// Serialize this definition to a [`serde_yaml::Value`] matching the
    /// canonical expanded-YAML shape that [`from_expanded_yaml`] reads.
    ///
    /// The output always includes `__gremlins_expanded__: true` so
    /// `from_expanded_yaml` recognizes it.
    ///
    /// [`from_expanded_yaml`]: GremlinDefinition::from_expanded_yaml
    pub fn to_expanded_yaml(&self) -> Value {
        let mut root = Mapping::new();

        // Sentinel — always emitted.
        root.insert(
            Value::String("__gremlins_expanded__".to_string()),
            Value::Bool(true),
        );

        // default_client — always present.
        root.insert(
            Value::String("default_client".to_string()),
            Value::String(self.default_client.clone()),
        );

        // base_ref — omit if "current".
        if self.base_ref != "current" {
            root.insert(
                Value::String("base_ref".to_string()),
                Value::String(self.base_ref.clone()),
            );
        }

        // bootstrap — omit entirely if all fields are default/empty.
        let bootstrap_yaml = bootstrap_to_yaml(&self.bootstrap);
        if !is_empty_mapping(&bootstrap_yaml) {
            root.insert(Value::String("bootstrap".to_string()), bootstrap_yaml);
        }

        // land — omit if None.
        if let Some(ref land) = self.land {
            let mut land_val = land.to_yaml();
            // Ensure name and type are forced to "land"/"exec" for round-trip
            // safety, matching what land_from_yaml does on parse.
            if let Value::Mapping(ref mut land_map) = land_val {
                land_map.insert(
                    Value::String("name".to_string()),
                    Value::String("land".to_string()),
                );
                land_map.insert(
                    Value::String("type".to_string()),
                    Value::String("exec".to_string()),
                );
            }
            root.insert(Value::String("land".to_string()), land_val);
        }

        // stages
        let stages: Vec<Value> = self.stages.iter().map(RunnableStage::to_yaml).collect();
        root.insert(Value::String("stages".to_string()), Value::Sequence(stages));

        Value::Mapping(root)
    }
}

/// The project root: the parent of the nearest ancestor `.gremlins` directory,
/// falling back to the definition's own directory.
fn project_root_for(path: &Path) -> PathBuf {
    let mut current = path.parent();
    while let Some(directory) = current {
        if directory
            .file_name()
            .is_some_and(|name| name == config::overlay_dirname())
        {
            if let Some(parent) = directory.parent() {
                return parent.to_path_buf();
            }
            break;
        }
        current = directory.parent();
    }
    path.parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn default_client_from_yaml(root: &Mapping) -> Result<Option<String>, SchemaError> {
    let Some(value) = root.get("default_client").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let client = value
        .as_str()
        .ok_or_else(|| SchemaError::Generic("default_client must be a string".to_string()))?;
    if client.trim().is_empty() {
        return Err(SchemaError::Generic(
            "default_client must be a non-empty string".to_string(),
        ));
    }
    Ok(Some(client.to_string()))
}

fn base_ref_from_yaml(root: &Mapping) -> Result<String, SchemaError> {
    let value = match root.get("base_ref") {
        None | Some(Value::Null) => return Ok("current".to_string()),
        Some(value) => value,
    };
    let base_ref = value
        .as_str()
        .ok_or_else(|| SchemaError::Generic("base_ref must be a string".to_string()))?;
    let trimmed = base_ref.trim();
    if trimmed.is_empty() {
        return Err(SchemaError::Generic(
            "base_ref must be a non-empty string".to_string(),
        ));
    }
    Ok(trimmed.to_string())
}

fn stages_from_yaml(root: &Mapping) -> Result<Vec<Value>, SchemaError> {
    match root.get("stages") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Sequence(stages)) => Ok(stages.clone()),
        Some(_) => Err(SchemaError::Generic("'stages' must be a list".to_string())),
    }
}

/// Build the `land` stage: the `land` mapping parsed as an exec stage, with the
/// declared `name` and `type` both forced — it is always an exec stage named
/// `land`, whatever the mapping declared, which is what the pyext path built.
fn land_from_yaml(root: &Mapping) -> Result<Option<RunnableStage>, SchemaError> {
    let Some(value) = root.get("land").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let declared = value
        .as_mapping()
        .ok_or_else(|| SchemaError::Generic("'land' must be a mapping".to_string()))?;

    // The land stage's identity and shape are fixed: whatever the mapping
    // declares, it is an exec stage named `land`.
    let mut stage = Mapping::new();
    for (key, value) in declared {
        stage.insert(key.clone(), value.clone());
    }
    stage.insert(
        Value::String("name".to_string()),
        Value::String("land".to_string()),
    );
    stage.insert(
        Value::String("type".to_string()),
        Value::String("exec".to_string()),
    );

    let parsed = RunnableStage::parse(&Value::Mapping(stage), 0)?;
    Ok(Some(parsed))
}

fn resolve_default_client(
    yaml_default: Option<String>,
    override_client: Option<&str>,
) -> Result<String, SchemaError> {
    if let Some(client) = yaml_default {
        return Ok(client);
    }
    // A blank `--client` is no client at all: skip it rather than resolve to an
    // unusable empty spec.
    if let Some(client) = override_client
        .map(str::trim)
        .filter(|client| !client.is_empty())
    {
        return Ok(client.to_string());
    }
    config::global_config()
        .ok()
        .and_then(|cfg| cfg.default_client().map(String::from))
        .ok_or_else(|| SchemaError::Generic(MISSING_DEFAULT_CLIENT.to_string()))
}

// ---------------------------------------------------------------------------
// Serialization helpers for to_expanded_yaml()
// ---------------------------------------------------------------------------

fn is_empty_mapping(value: &Value) -> bool {
    match value {
        Value::Mapping(m) => m.is_empty(),
        _ => false,
    }
}

fn bootstrap_to_yaml(bootstrap: &Bootstrap) -> Value {
    let mut m = Mapping::new();

    if let Some(ref source) = bootstrap.source {
        let mut src_map = Mapping::with_capacity(source.sources.len());
        for (name, input_src) in &source.sources {
            let mut entry = Mapping::new();
            if input_src.types.len() == 1 {
                entry.insert(
                    Value::String("type".to_string()),
                    Value::String(input_src.types[0].clone()),
                );
            } else {
                let types: Vec<Value> = input_src
                    .types
                    .iter()
                    .map(|t| Value::String(t.clone()))
                    .collect();
                entry.insert(Value::String("type".to_string()), Value::Sequence(types));
            }
            if input_src.optional {
                entry.insert(Value::String("optional".to_string()), Value::Bool(true));
            }
            src_map.insert(Value::String(name.clone()), Value::Mapping(entry));
        }
        m.insert(Value::String("source".to_string()), Value::Mapping(src_map));
    }

    if !bootstrap.launch_cmds.is_empty() {
        let cmds: Vec<Value> = bootstrap
            .launch_cmds
            .iter()
            .map(|c| Value::String(c.clone()))
            .collect();
        m.insert(
            Value::String("launch_cmds".to_string()),
            Value::Sequence(cmds),
        );
    }

    if !bootstrap.cmds.is_empty() {
        let cmds: Vec<Value> = bootstrap
            .cmds
            .iter()
            .map(|c| Value::String(c.clone()))
            .collect();
        m.insert(Value::String("cmds".to_string()), Value::Sequence(cmds));
    }

    if !bootstrap.cli_out.is_empty() {
        let mut cli = Mapping::with_capacity(bootstrap.cli_out.len());
        for (k, v) in &bootstrap.cli_out {
            cli.insert(Value::String(k.clone()), Value::String(v.clone()));
        }
        m.insert(Value::String("cli_out".to_string()), Value::Mapping(cli));
    }

    if !bootstrap.env.is_empty() {
        m.insert(
            Value::String("env".to_string()),
            Value::String(bootstrap.env.clone()),
        );
    }

    Value::Mapping(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stages::composite::ClientSpec;
    use crate::test_support::Sandbox;

    const WITH_CLIENT: &str = r#"
default_client: 'xai:grok-4'

stages:
  - name: plan
    type: agent
    prompt:
      - "write the plan to {plan}\n"
    bind:
      plan: artifact://plan.md
  - name: run
    type: exec
    interpolation:
      plan: content("artifact://plan.md")
    options:
      cmds:
        - "cat {plan}"
"#;

    const WITHOUT_CLIENT: &str = r#"
stages:
  - name: plan
    type: agent
    prompt:
      - "write the plan to {plan}\n"
    bind:
      plan: artifact://plan.md
  - name: run
    type: exec
    interpolation:
      plan: content("artifact://plan.md")
    options:
      cmds:
        - "cat {plan}"
"#;

    fn write_fixture(dir: &Path, stem: &str, body: &str) -> PathBuf {
        let overlay = dir.join(".gremlins");
        std::fs::create_dir_all(&overlay).unwrap();
        let path = overlay.join(format!("{stem}.yaml"));
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn resolves_definition_identity_and_stage_tree() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "demo", WITH_CLIENT);

        let definition = GremlinDefinition::from_yaml(&path, None).unwrap();

        assert_eq!(definition.name, "demo");
        assert!(definition.path.is_absolute());
        assert_eq!(definition.default_client, "xai:grok-4");
        assert_eq!(definition.base_ref, "current");
        assert!(definition.land.is_none());
        assert!(definition.bootstrap.launch_cmds.is_empty());

        let names: Vec<&str> = definition.stages.iter().map(RunnableStage::name).collect();
        assert_eq!(names, vec!["plan", "run"]);
        assert_eq!(definition.stages[0].stage_type(), "agent");
        assert_eq!(definition.stages[1].stage_type(), "exec");
    }

    #[test]
    fn default_client_reaches_every_stage() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "demo", WITH_CLIENT);

        let definition = GremlinDefinition::from_yaml(&path, None).unwrap();
        // Stages without an explicit `client:` carry None — the definition
        // default_client is resolved at runtime by the executor.
        assert_eq!(definition.stages[0].client(), None);
        assert_eq!(definition.stages[1].client(), None);
        assert_eq!(definition.default_client, "xai:grok-4");
    }

    #[test]
    fn a_stage_client_beats_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(
            dir.path(),
            "demo",
            r#"
default_client: 'xai:grok-4'

stages:
  - name: plan
    type: agent
    client: 'local:model'
    prompt:
      - "write {plan}\n"
    bind:
      plan: artifact://plan.md
  - name: run
    type: exec
    interpolation:
      plan: content("artifact://plan.md")
    options:
      cmds:
        - "cat {plan}"
"#,
        );

        let definition = GremlinDefinition::from_yaml(&path, None).unwrap();
        assert_eq!(
            definition.stages[0].client(),
            Some(&ClientSpec("local:model".into()))
        );
        // Stage without explicit client carries None — resolved at runtime.
        assert_eq!(definition.stages[1].client(), None);
        assert_eq!(definition.default_client, "xai:grok-4");
    }

    #[test]
    fn yaml_client_wins_over_the_override() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "demo", WITH_CLIENT);

        let definition = GremlinDefinition::from_yaml(&path, Some("cli:model")).unwrap();
        assert_eq!(definition.default_client, "xai:grok-4");
    }

    #[test]
    fn override_is_used_when_the_yaml_omits_a_client() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "demo", WITHOUT_CLIENT);

        let definition = GremlinDefinition::from_yaml(&path, Some("cli:model")).unwrap();
        assert_eq!(definition.default_client, "cli:model");
        // Stages without explicit client carry None — resolved at runtime.
        assert_eq!(definition.stages[0].client(), None);
    }

    /// A definition whose client has to come from the sandbox's config, plus
    /// the sandbox that supplies (or withholds) it and the project directory
    /// the definition file lives in.
    fn definition_needing_a_client(
        config_json: Option<&str>,
    ) -> (Sandbox, tempfile::TempDir, PathBuf) {
        let sandbox = Sandbox::with_config(config_json);
        let project = tempfile::tempdir().unwrap();
        let path = write_fixture(project.path(), "demo", WITHOUT_CLIENT);
        (sandbox, project, path)
    }

    #[test]
    fn config_supplies_the_client_when_nothing_else_does() {
        let (_sandbox, _project, path) =
            definition_needing_a_client(Some(r#"{"default-client": "cfg:model"}"#));
        let definition = GremlinDefinition::from_yaml(&path, None).unwrap();
        assert_eq!(definition.default_client, "cfg:model");
    }

    #[test]
    fn a_client_is_required_from_somewhere() {
        let (_sandbox, _project, path) = definition_needing_a_client(None);
        let err = GremlinDefinition::from_yaml(&path, None).unwrap_err();
        assert!(
            err.to_string().contains("missing 'default_client'"),
            "{err}"
        );
    }

    #[test]
    fn base_ref_and_land_are_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(
            dir.path(),
            "demo",
            r#"
default_client: 'xai:grok-4'
base_ref: main

bootstrap:
  launch_cmds:
    - gremlins:bind_artifact("artifact://plan.md", plan)

land:
  interpolation:
    PR_URL: content("artifact://pr-url")
  options:
    cmds:
      - gh pr merge --squash --delete-branch "{PR_URL}"

stages:
  - name: plan
    type: agent
    prompt:
      - "write {plan}\n"
    bind:
      plan: artifact://plan.md
"#,
        );

        let definition = GremlinDefinition::from_yaml(&path, None).unwrap();
        assert_eq!(definition.base_ref, "main");
        assert_eq!(definition.bootstrap.launch_cmds.len(), 1);

        let land = definition.land.expect("land stage");
        assert_eq!(land.name(), "land");
        assert_eq!(land.stage_type(), "exec");
    }

    #[test]
    fn empty_default_client_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "demo", "default_client: ''\nstages: []\n");

        let err = GremlinDefinition::from_yaml(&path, None).unwrap_err();
        assert!(
            err.to_string()
                .contains("default_client must be a non-empty string"),
            "{err}"
        );
    }

    #[test]
    fn empty_base_ref_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(
            dir.path(),
            "demo",
            "default_client: 'xai:grok-4'\nbase_ref: '   '\nstages: []\n",
        );

        let err = GremlinDefinition::from_yaml(&path, None).unwrap_err();
        assert!(
            err.to_string()
                .contains("base_ref must be a non-empty string"),
            "{err}"
        );
    }

    #[test]
    fn missing_file_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let err = GremlinDefinition::from_yaml(dir.path().join("absent.yaml"), None).unwrap_err();
        assert!(
            err.to_string().contains("definition file not found"),
            "{err}"
        );
    }

    #[test]
    fn duplicate_producers_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(
            dir.path(),
            "demo",
            r#"
default_client: 'xai:grok-4'

stages:
  - name: first
    type: agent
    prompt:
      - "one {out}\n"
    bind:
      out: artifact://shared.md
  - name: second
    type: agent
    prompt:
      - "two {out}\n"
    bind:
      out: artifact://shared.md
"#,
        );

        let err = GremlinDefinition::from_yaml(&path, None)
            .unwrap()
            .validate()
            .unwrap_err();
        assert!(
            err.to_string().contains("duplicate artifact producer"),
            "{err}"
        );
    }

    #[test]
    fn unresolved_consumers_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(
            dir.path(),
            "demo",
            r#"
default_client: 'xai:grok-4'

stages:
  - name: consumer
    type: exec
    interpolation:
      missing: content("artifact://never-produced.md")
    options:
      cmds:
        - "cat {missing}"
"#,
        );

        let err = GremlinDefinition::from_yaml(&path, None)
            .unwrap()
            .validate()
            .unwrap_err();
        assert!(
            err.to_string().contains("artifact://never-produced.md"),
            "{err}"
        );
    }

    #[test]
    fn land_name_is_forced() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(
            dir.path(),
            "demo",
            r#"
default_client: 'xai:grok-4'
stages: []
land:
  name: not-land
  options:
    cmds:
      - "true"
"#,
        );

        let definition = GremlinDefinition::from_yaml(&path, None).unwrap();
        assert_eq!(definition.land.as_ref().unwrap().name(), "land");
    }

    #[test]
    fn land_inherits_the_default_client() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(
            dir.path(),
            "demo",
            r#"
default_client: 'xai:grok-4'
stages: []
land:
  options:
    cmds:
      - "true"
"#,
        );

        let definition = GremlinDefinition::from_yaml(&path, None).unwrap();
        // Land without explicit client carries None — resolved at runtime.
        assert_eq!(definition.land.as_ref().unwrap().client(), None);
        assert_eq!(definition.default_client, "xai:grok-4");
    }

    #[test]
    fn land_keeps_its_explicit_client() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(
            dir.path(),
            "demo",
            r#"
default_client: 'xai:grok-4'
stages: []
land:
  client: 'local:model'
  options:
    cmds:
      - "true"
"#,
        );

        let definition = GremlinDefinition::from_yaml(&path, None).unwrap();
        assert_eq!(
            definition.land.as_ref().unwrap().client(),
            Some(&ClientSpec("local:model".into()))
        );
    }

    #[test]
    fn a_blank_override_never_becomes_the_client() {
        let (_sandbox, _project, path) = definition_needing_a_client(None);
        let err = GremlinDefinition::from_yaml(&path, Some("   ")).unwrap_err();
        assert!(
            err.to_string().contains("missing 'default_client'"),
            "{err}"
        );
    }

    #[test]
    fn a_blank_override_falls_through_to_config() {
        let (_sandbox, _project, path) =
            definition_needing_a_client(Some(r#"{"default-client": "cfg:model"}"#));
        let definition = GremlinDefinition::from_yaml(&path, Some("")).unwrap();
        assert_eq!(definition.default_client, "cfg:model");
    }

    #[test]
    fn blank_default_client_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "demo", "default_client: '   '\nstages: []\n");

        let err = GremlinDefinition::from_yaml(&path, None).unwrap_err();
        assert!(
            err.to_string()
                .contains("default_client must be a non-empty string"),
            "{err}"
        );
    }

    #[test]
    fn nested_stage_names_are_visible_to_validation() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(
            dir.path(),
            "demo",
            r#"
default_client: 'xai:grok-4'

stages:
  - type: sequence
    body:
      - type: exec
        interpolation:
          missing: content("artifact://never-produced.md")
        options:
          cmds:
            - "cat {missing}"
"#,
        );

        let err = GremlinDefinition::from_yaml(&path, None)
            .unwrap()
            .validate()
            .unwrap_err();
        // The unnamed nested stage is auto-named `exec` before validation.
        assert!(err.to_string().contains("stage exec:"), "{err}");
    }

    /// [`GremlinDefinition::validate`] catches issues discovered by
    /// `validate_stage_keys` — an unused bind key — even when loaded without
    /// validation.
    #[test]
    fn validate_method_catches_unused_bind_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(
            dir.path(),
            "demo",
            r#"
default_client: 'xai:grok-4'

stages:
  - name: orphan
    type: agent
    bind:
      ghost: artifact://z
    prompt:
      - "hello\n"
"#,
        );

        let def = GremlinDefinition::from_yaml(&path, None).unwrap();
        let err = def.validate().unwrap_err();
        assert!(
            err.to_string().contains("ghost"),
            "validate() should catch unused bind key, got: {err}"
        );
    }

    /// [`GremlinDefinition::validate`] catches duplicate producers.
    #[test]
    fn validate_method_catches_duplicate_producers() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(
            dir.path(),
            "demo",
            r#"
default_client: 'xai:grok-4'

stages:
  - name: first
    type: agent
    prompt:
      - "one {out}\n"
    bind:
      out: artifact://shared.md
  - name: second
    type: agent
    prompt:
      - "two {out}\n"
    bind:
      out: artifact://shared.md
"#,
        );

        let def = GremlinDefinition::from_yaml(&path, None).unwrap();
        let err = def.validate().unwrap_err();
        assert!(
            err.to_string().contains("duplicate artifact producer"),
            "{err}"
        );
    }

    /// [`GremlinDefinition::validate`] catches unresolved consumers.
    #[test]
    fn validate_method_catches_unresolved_consumers() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(
            dir.path(),
            "demo",
            r#"
default_client: 'xai:grok-4'

stages:
  - name: consumer
    type: exec
    interpolation:
      missing: content("artifact://never-produced.md")
    options:
      cmds:
        - "cat {missing}"
"#,
        );

        let def = GremlinDefinition::from_yaml(&path, None).unwrap();
        let err = def.validate().unwrap_err();
        assert!(
            err.to_string().contains("artifact://never-produced.md"),
            "{err}"
        );
    }

    /// [`GremlinDefinition::validate`] is a no-op on a valid definition.
    #[test]
    fn validate_method_passes_on_valid_definition() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "demo", WITH_CLIENT);

        let def = GremlinDefinition::from_yaml(&path, None).unwrap();
        def.validate().unwrap();
    }

    /// A plain file written directly to disk (no `.gremlins` overlay), for the
    /// direct-load path, which never walks to a project root.
    fn write_plain_file(dir: &Path, stem: &str, body: &str) -> PathBuf {
        let path = dir.join(format!("{stem}.yaml"));
        std::fs::write(&path, body).unwrap();
        path
    }

    const EXPANDED_BODY: &str = r#"
default_client: 'cmd:true'
base_ref: main
stages:
  - name: run
    type: exec
    options:
      cmds:
        - "echo hello"
"#;

    const EXPANDED_BODY_WITH_SENTINEL: &str = r#"
__gremlins_expanded__: true
default_client: 'cmd:true'
base_ref: main
stages:
  - name: run
    type: exec
    options:
      cmds:
        - "echo hello"
"#;

    #[test]
    fn from_expanded_yaml_loads_with_sentinel() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_plain_file(dir.path(), "expanded", EXPANDED_BODY_WITH_SENTINEL);

        let definition = GremlinDefinition::from_expanded_yaml(&path, None).unwrap();

        assert_eq!(definition.name, "expanded");
        assert_eq!(definition.default_client, "cmd:true");
        assert_eq!(definition.base_ref, "main");
        assert_eq!(definition.stages.len(), 1);
        assert_eq!(definition.stages[0].name(), "run");
        assert_eq!(definition.stages[0].stage_type(), "exec");
        assert!(
            definition
                .expanded_yaml
                .get("__gremlins_expanded__")
                .is_none(),
            "sentinel must be stripped from the stored YAML"
        );
    }

    #[test]
    fn from_expanded_yaml_loads_without_sentinel() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_plain_file(dir.path(), "expanded", EXPANDED_BODY);

        let definition = GremlinDefinition::from_expanded_yaml(&path, None).unwrap();

        assert_eq!(definition.name, "expanded");
        assert_eq!(definition.default_client, "cmd:true");
        assert_eq!(definition.base_ref, "main");
        assert_eq!(definition.stages.len(), 1);
        assert_eq!(definition.stages[0].name(), "run");
        assert_eq!(definition.stages[0].stage_type(), "exec");
        assert!(
            definition
                .expanded_yaml
                .get("__gremlins_expanded__")
                .is_none(),
            "sentinel must be stripped from the stored YAML"
        );
    }

    #[test]
    fn from_expanded_yaml_and_from_yaml_produce_equivalent_definitions() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_fixture(dir.path(), "demo", WITH_CLIENT);
        let from_yaml = GremlinDefinition::from_yaml(&source, None).unwrap();

        // The expanded tree from the full path is the direct path's input:
        // serialize it to a fresh file (same stem so the identity matches) and
        // load it back without expansion.
        let expanded_path = dir.path().join("reload").join("demo.yaml");
        std::fs::create_dir_all(expanded_path.parent().unwrap()).unwrap();
        let serialized = serde_yaml::to_string(&from_yaml.expanded_yaml).unwrap();
        std::fs::write(&expanded_path, serialized).unwrap();

        let from_expanded = GremlinDefinition::from_expanded_yaml(&expanded_path, None).unwrap();

        assert_eq!(from_expanded.name, from_yaml.name);
        assert_eq!(from_expanded.default_client, from_yaml.default_client);
        assert_eq!(from_expanded.base_ref, from_yaml.base_ref);
        assert_eq!(from_expanded.stages.len(), from_yaml.stages.len());

        let yaml_names: Vec<&str> = from_yaml.stages.iter().map(RunnableStage::name).collect();
        let expanded_names: Vec<&str> = from_expanded
            .stages
            .iter()
            .map(RunnableStage::name)
            .collect();
        assert_eq!(expanded_names, yaml_names);

        let yaml_types: Vec<&str> = from_yaml
            .stages
            .iter()
            .map(RunnableStage::stage_type)
            .collect();
        let expanded_types: Vec<&str> = from_expanded
            .stages
            .iter()
            .map(RunnableStage::stage_type)
            .collect();
        assert_eq!(expanded_types, yaml_types);
    }

    // ------------------------------------------------------------------
    // to_expanded_yaml() round-trip tests
    // ------------------------------------------------------------------

    /// Helper: parse a YAML string via `from_yaml`, serialize it back
    /// with `to_expanded_yaml()`, then re-parse via `from_expanded_value`
    /// and assert the two definitions are equivalent.
    fn round_trip(yaml_body: &str) {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "roundtrip", yaml_body);

        let original = GremlinDefinition::from_yaml(&path, None).unwrap();
        let serialized = original.to_expanded_yaml();

        // Re-parse via from_expanded_value (private, but accessible in-module).
        let roundtripped =
            GremlinDefinition::from_expanded_value(serialized, path.clone(), None).unwrap();

        assert_eq!(roundtripped.name, original.name);
        assert_eq!(roundtripped.default_client, original.default_client);
        assert_eq!(roundtripped.base_ref, original.base_ref);
        assert_eq!(roundtripped.stages.len(), original.stages.len());

        for (a, b) in roundtripped.stages.iter().zip(original.stages.iter()) {
            assert_eq!(a.name(), b.name());
            assert_eq!(a.stage_type(), b.stage_type());
        }

        // Land round-trips.
        match (&roundtripped.land, &original.land) {
            (Some(a), Some(b)) => {
                assert_eq!(a.name(), b.name());
                assert_eq!(a.stage_type(), b.stage_type());
            }
            (None, None) => {}
            _ => panic!("land mismatch"),
        }

        // Bootstrap round-trips.
        assert_eq!(
            roundtripped.bootstrap.launch_cmds,
            original.bootstrap.launch_cmds
        );
        assert_eq!(roundtripped.bootstrap.cmds, original.bootstrap.cmds);
        assert_eq!(roundtripped.bootstrap.env, original.bootstrap.env);
        assert_eq!(roundtripped.bootstrap.cli_out, original.bootstrap.cli_out);
    }

    #[test]
    fn round_trip_minimal_agent_and_exec() {
        round_trip(
            r#"
default_client: 'xai:grok-4'

stages:
  - name: plan
    type: agent
    prompt:
      - "write the plan to {plan}\n"
    bind:
      plan: artifact://plan.md
  - name: run
    type: exec
    interpolation:
      plan: content("artifact://plan.md")
    options:
      cmds:
        - "cat {plan}"
"#,
        );
    }

    #[test]
    fn round_trip_with_all_composites() {
        round_trip(
            r#"
default_client: 'xai:grok-4'

stages:
  - name: outer
    type: sequence
    body:
      - name: inner-loop
        type: loop
        max-iterations: 3
        stop_when_exists: artifact://done.txt
        options:
          interval: 1.5
        body:
          - name: parallel-stuff
            type: parallel
            max_concurrent: 2
            cancel_on_error: true
            error_policy: all
            parallel:
              - name: a
                type: exec
                options:
                  cmds:
                    - "echo a"
              - name: b
                type: exec
                options:
                  cmds:
                    - "echo b"
"#,
        );
    }

    #[test]
    fn round_trip_with_bootstrap() {
        round_trip(
            r#"
default_client: 'xai:grok-4'

bootstrap:
  source:
    my_input:
      type: string
      optional: true
  launch_cmds:
    - gremlins:bind_artifact("artifact://plan.md", plan)
  cmds:
    - echo ready
  cli_out:
    plan: artifact://plan.md
  env:
    FOO=bar

stages:
  - name: run
    type: exec
    interpolation:
      plan: content("artifact://plan.md")
    options:
      cmds:
        - "cat {plan}"
"#,
        );
    }

    #[test]
    fn round_trip_with_land() {
        round_trip(
            r#"
default_client: 'xai:grok-4'

land:
  interpolation:
    PR_URL: content("artifact://pr-url")
  options:
    cmds:
      - gh pr merge --squash --delete-branch "{PR_URL}"

stages:
  - name: plan
    type: agent
    prompt:
      - "create {pr_url}\n"
    bind:
      pr_url: artifact://pr-url
"#,
        );
    }

    #[test]
    fn to_expanded_yaml_emits_sentinel() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "sentinel", WITH_CLIENT);
        let definition = GremlinDefinition::from_yaml(&path, None).unwrap();
        let yaml = definition.to_expanded_yaml();

        assert_eq!(
            yaml.get("__gremlins_expanded__"),
            Some(&Value::Bool(true)),
            "to_expanded_yaml must emit __gremlins_expanded__: true"
        );
    }

    #[test]
    fn to_expanded_yaml_omits_default_base_ref() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "base", WITH_CLIENT);
        let definition = GremlinDefinition::from_yaml(&path, None).unwrap();
        let yaml = definition.to_expanded_yaml();

        // base_ref defaults to "current" — should be absent from output.
        assert!(
            yaml.get("base_ref").is_none(),
            "base_ref 'current' must be omitted"
        );
    }

    #[test]
    fn to_expanded_yaml_omits_empty_skip_if_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "skip", WITH_CLIENT);
        let definition = GremlinDefinition::from_yaml(&path, None).unwrap();
        let yaml = definition.to_expanded_yaml();

        // None of the stages have skip_if_exists set — the key must be absent.
        let stages = yaml.get("stages").unwrap().as_sequence().unwrap();
        for stage in stages {
            assert!(
                stage.get("skip_if_exists").is_none(),
                "skip_if_exists must be omitted when empty"
            );
        }
    }

    #[test]
    fn to_expanded_yaml_omits_empty_bootstrap() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "noboot", WITH_CLIENT);
        let definition = GremlinDefinition::from_yaml(&path, None).unwrap();
        let yaml = definition.to_expanded_yaml();

        // No bootstrap in the fixture — key must be absent.
        assert!(
            yaml.get("bootstrap").is_none(),
            "bootstrap must be omitted when all fields are default"
        );
    }

    #[test]
    fn to_expanded_yaml_preserves_empty_bootstrap_source() {
        // When bootstrap.source is explicitly Some but sources is empty,
        // we must emit source: {} to preserve the distinction.
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(
            dir.path(),
            "empty-src",
            r#"
default_client: 'xai:grok-4'

bootstrap:
  source: {}

stages:
  - name: run
    type: exec
    options:
      cmds:
        - "echo hi"
"#,
        );
        let definition = GremlinDefinition::from_yaml(&path, None).unwrap();
        let yaml = definition.to_expanded_yaml();

        let bootstrap = yaml.get("bootstrap").expect("bootstrap must be present");
        let source = bootstrap
            .get("source")
            .expect("source must be present when explicitly set");
        assert!(
            source.as_mapping().unwrap().is_empty(),
            "source must be an empty mapping"
        );
    }
}
