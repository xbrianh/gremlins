//! The native `Pipeline` — an expanded pipeline YAML resolved to typed data.
//!
//! [`Pipeline::from_yaml`] mirrors `pyext::schemas::Pipeline::from_yaml` step
//! for step: canonicalise and locate the file, walk up to the `.gremlins`
//! project root, expand the YAML, then resolve the pipeline identity (`name`,
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
use crate::stages::composite::ClientSpec;
use crate::stages::node::RunnableStage;

/// The message emitted when no layer supplied a default client.
const MISSING_DEFAULT_CLIENT: &str = "pipeline is missing 'default_client' — set a \
     'default_client' in the pipeline YAML, pass --client on the command line, or set \
     'default-client' in config.json";

/// A pipeline resolved from an expanded YAML file.
#[derive(Debug, Clone)]
pub struct Pipeline {
    /// The pipeline's identity: the YAML file stem.
    pub name: String,
    /// Where the pipeline was loaded from, canonicalised where possible.
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
}

impl Pipeline {
    /// Clone this pipeline, replacing its stage list with `stages`.
    ///
    /// Used by the parallel executor to give each child a pipeline that
    /// contains only the child's own stage(s), while inheriting every other
    /// field (name, path, default_client, base_ref, bootstrap, land) from
    /// the parent.
    pub fn clone_with_stages(&self, stages: Vec<RunnableStage>) -> Self {
        Pipeline {
            stages,
            ..self.clone()
        }
    }

    /// Load and resolve a pipeline. `default_client_override` is the CLI
    /// `--client` value; it is consulted only when the YAML declares none.
    pub fn from_yaml(
        path: impl AsRef<Path>,
        default_client_override: Option<&str>,
    ) -> Result<Pipeline, SchemaError> {
        let path = path.as_ref();
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if !path.exists() {
            return Err(SchemaError::PipelineFileNotFound {
                path: path.display().to_string(),
            });
        }

        let project_root = project_root_for(&path);
        let expanded = expand::parse_pipeline_file(&path, &project_root)?;
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
        let mut stages = RunnableStage::parse_stages(&mut raw_stages, 0)?;

        if root.contains_key("inputs") {
            return Err(SchemaError::Generic(
                "'inputs' is not a valid pipeline key; declare CLI arguments under bootstrap.source"
                    .to_string(),
            ));
        }

        let bootstrap = match root.get("bootstrap") {
            None | Some(Value::Null) => Bootstrap::default(),
            Some(value) => Bootstrap::from_yaml(Some(value))?,
        };

        let mut land = land_from_yaml(root)?;

        // The validators walk the typed tree: its names are the filled ones,
        // including nested stages the raw YAML never had named.
        let nodes: Vec<StageNode> = stages.iter().map(RunnableStage::to_stage_node).collect();
        loader::check_duplicate_producers(&nodes, &bootstrap.cli_out)?;
        loader::check_unresolved_consumers(&nodes, &bootstrap.launch_cmds, &bootstrap.cli_out)?;

        let default_client = resolve_default_client(yaml_default_client, default_client_override)?;
        let spec = ClientSpec(default_client.clone());
        for stage in &mut stages {
            stage.fill_client(&spec);
        }
        if let Some(land) = land.as_mut() {
            land.fill_client(&spec);
        }

        Ok(Pipeline {
            name,
            path,
            default_client,
            base_ref,
            bootstrap,
            stages,
            land,
        })
    }
}

/// The project root: the parent of the nearest ancestor `.gremlins` directory,
/// falling back to the pipeline's own directory.
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
    fn resolves_pipeline_identity_and_stage_tree() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "demo", WITH_CLIENT);

        let pipeline = Pipeline::from_yaml(&path, None).unwrap();

        assert_eq!(pipeline.name, "demo");
        assert!(pipeline.path.is_absolute());
        assert_eq!(pipeline.default_client, "xai:grok-4");
        assert_eq!(pipeline.base_ref, "current");
        assert!(pipeline.land.is_none());
        assert!(pipeline.bootstrap.launch_cmds.is_empty());

        let names: Vec<&str> = pipeline.stages.iter().map(RunnableStage::name).collect();
        assert_eq!(names, vec!["plan", "run"]);
        assert_eq!(pipeline.stages[0].stage_type(), "agent");
        assert_eq!(pipeline.stages[1].stage_type(), "exec");
    }

    #[test]
    fn default_client_reaches_every_stage() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "demo", WITH_CLIENT);

        let pipeline = Pipeline::from_yaml(&path, None).unwrap();
        let default = Some(&ClientSpec("xai:grok-4".into()));
        assert_eq!(pipeline.stages[0].client(), default);
        assert_eq!(pipeline.stages[1].client(), default);
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

        let pipeline = Pipeline::from_yaml(&path, None).unwrap();
        assert_eq!(
            pipeline.stages[0].client(),
            Some(&ClientSpec("local:model".into()))
        );
        assert_eq!(
            pipeline.stages[1].client(),
            Some(&ClientSpec("xai:grok-4".into()))
        );
    }

    #[test]
    fn yaml_client_wins_over_the_override() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "demo", WITH_CLIENT);

        let pipeline = Pipeline::from_yaml(&path, Some("cli:model")).unwrap();
        assert_eq!(pipeline.default_client, "xai:grok-4");
    }

    #[test]
    fn override_is_used_when_the_yaml_omits_a_client() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "demo", WITHOUT_CLIENT);

        let pipeline = Pipeline::from_yaml(&path, Some("cli:model")).unwrap();
        assert_eq!(pipeline.default_client, "cli:model");
        assert_eq!(
            pipeline.stages[0].client(),
            Some(&ClientSpec("cli:model".into()))
        );
    }

    /// A pipeline whose client has to come from the sandbox's config, plus
    /// the sandbox that supplies (or withholds) it and the project directory
    /// the pipeline file lives in.
    fn pipeline_needing_a_client(
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
            pipeline_needing_a_client(Some(r#"{"default-client": "cfg:model"}"#));
        let pipeline = Pipeline::from_yaml(&path, None).unwrap();
        assert_eq!(pipeline.default_client, "cfg:model");
    }

    #[test]
    fn a_client_is_required_from_somewhere() {
        let (_sandbox, _project, path) = pipeline_needing_a_client(None);
        let err = Pipeline::from_yaml(&path, None).unwrap_err();
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

        let pipeline = Pipeline::from_yaml(&path, None).unwrap();
        assert_eq!(pipeline.base_ref, "main");
        assert_eq!(pipeline.bootstrap.launch_cmds.len(), 1);

        let land = pipeline.land.expect("land stage");
        assert_eq!(land.name(), "land");
        assert_eq!(land.stage_type(), "exec");
    }

    #[test]
    fn empty_default_client_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "demo", "default_client: ''\nstages: []\n");

        let err = Pipeline::from_yaml(&path, None).unwrap_err();
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

        let err = Pipeline::from_yaml(&path, None).unwrap_err();
        assert!(
            err.to_string()
                .contains("base_ref must be a non-empty string"),
            "{err}"
        );
    }

    #[test]
    fn missing_file_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let err = Pipeline::from_yaml(dir.path().join("absent.yaml"), None).unwrap_err();
        assert!(err.to_string().contains("pipeline file not found"), "{err}");
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

        let err = Pipeline::from_yaml(&path, None).unwrap_err();
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

        let err = Pipeline::from_yaml(&path, None).unwrap_err();
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

        let pipeline = Pipeline::from_yaml(&path, None).unwrap();
        assert_eq!(pipeline.land.as_ref().unwrap().name(), "land");
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

        let pipeline = Pipeline::from_yaml(&path, None).unwrap();
        assert_eq!(
            pipeline.land.as_ref().unwrap().client(),
            Some(&ClientSpec("xai:grok-4".into()))
        );
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

        let pipeline = Pipeline::from_yaml(&path, None).unwrap();
        assert_eq!(
            pipeline.land.as_ref().unwrap().client(),
            Some(&ClientSpec("local:model".into()))
        );
    }

    #[test]
    fn a_blank_override_never_becomes_the_client() {
        let (_sandbox, _project, path) = pipeline_needing_a_client(None);
        let err = Pipeline::from_yaml(&path, Some("   ")).unwrap_err();
        assert!(
            err.to_string().contains("missing 'default_client'"),
            "{err}"
        );
    }

    #[test]
    fn a_blank_override_falls_through_to_config() {
        let (_sandbox, _project, path) =
            pipeline_needing_a_client(Some(r#"{"default-client": "cfg:model"}"#));
        let pipeline = Pipeline::from_yaml(&path, Some("")).unwrap();
        assert_eq!(pipeline.default_client, "cfg:model");
    }

    #[test]
    fn blank_default_client_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(dir.path(), "demo", "default_client: '   '\nstages: []\n");

        let err = Pipeline::from_yaml(&path, None).unwrap_err();
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

        let err = Pipeline::from_yaml(&path, None).unwrap_err();
        // The unnamed nested stage is auto-named `exec` before validation.
        assert!(err.to_string().contains("stage exec:"), "{err}");
    }
}
