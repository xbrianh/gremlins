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
        let root = expanded
            .as_mapping()
            .ok_or_else(|| SchemaError::YamlNotMapping {
                label: path.display().to_string(),
                got: format!("{expanded:?}"),
            })?;

        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("")
            .to_string();

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
}
