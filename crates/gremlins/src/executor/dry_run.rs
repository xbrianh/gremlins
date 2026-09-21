//! Dry-run validation: walk the stage tree with a [`DryRunArtifactRegistry`],
//! calling `prepare_agent` / `prepare_exec` for each stage without executing
//! model or shell commands.
//!
//! Errors are batched — every failure across the entire tree is collected and
//! reported in one pass rather than short-circuiting on the first problem.

use std::collections::HashMap;
use std::fmt;

use crate::artifacts::registry::{ArtifactRegistry, DryRunArtifactRegistry};
use crate::schemas::gremlin_definition::GremlinDefinition;
use crate::stages::agent::{prepare_agent, AgentPrepared};
use crate::stages::exec::{prepare_exec, ExecPrepared};
use crate::stages::node::RunnableStage;

use super::bootstrap::parse_gremlins_command;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// One failure found during dry-run validation.
#[derive(Debug)]
pub struct DryRunError {
    pub stage: String,
    pub message: String,
}

impl fmt::Display for DryRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "stage {}: {}", self.stage, self.message)
    }
}

/// Batch of dry-run errors.
#[derive(Debug, Default)]
pub struct DryRunErrors {
    errors: Vec<DryRunError>,
}

impl DryRunErrors {
    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn push(&mut self, stage: String, message: String) {
        self.errors.push(DryRunError { stage, message });
    }

    pub fn into_errors(self) -> Vec<DryRunError> {
        self.errors
    }
}

impl fmt::Display for DryRunErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for err in &self.errors {
            writeln!(f, "{err}")?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Walk the entire stage tree of `definition` in dispatch order, calling
/// `prepare_agent` / `prepare_exec` for every leaf stage with a
/// `DryRunArtifactRegistry`.  Returns every error found — an empty vec means
/// the definition is valid.
pub async fn validate_definition(definition: &GremlinDefinition) -> Vec<DryRunError> {
    let mut errors = DryRunErrors::default();

    // Framework substitutions: the dry run has no live client or worktree, so
    // we derive {model} from default_client and use "." for {cwd}.
    let model = &definition.default_client;
    let framework_subs = HashMap::from([
        ("model".to_string(), model.clone()),
        ("cwd".to_string(), ".".to_string()),
        ("base_ref".to_string(), definition.base_ref.clone()),
    ]);

    let registry = DryRunArtifactRegistry::seeded(bootstrap_artifact_keys(definition));

    walk_stages(
        &definition.stages,
        &registry,
        &framework_subs,
        "",
        &mut errors,
    )
    .await;

    errors.into_errors()
}

// ---------------------------------------------------------------------------
// Tree walk
// ---------------------------------------------------------------------------

/// Walk a list of stages in order. Boxed to break the recursive async cycle.
fn walk_stages<'a>(
    stages: &'a [RunnableStage],
    registry: &'a DryRunArtifactRegistry,
    framework_subs: &'a HashMap<String, String>,
    scope: &'a str,
    errors: &'a mut DryRunErrors,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
    Box::pin(async move {
        for stage in stages {
            walk_stage(stage, registry, framework_subs, scope, errors).await;
        }
    })
}

/// Walk a single stage. Boxed to break the recursive async cycle.
fn walk_stage<'a>(
    stage: &'a RunnableStage,
    registry: &'a DryRunArtifactRegistry,
    framework_subs: &'a HashMap<String, String>,
    scope: &'a str,
    errors: &'a mut DryRunErrors,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
    Box::pin(async move {
        match stage {
            RunnableStage::Agent { stage: agent, .. } => {
                let mut fsubs = framework_subs.clone();
                fsubs.insert("name".to_string(), agent.name.clone());
                let loop_iter = fsubs.get("loop_iter").map(String::as_str).unwrap_or("1");

                match prepare_agent(agent, registry, loop_iter, &fsubs).await {
                    Ok(prepared) => commit_prepared_agent(registry, &prepared, errors).await,
                    Err(e) => {
                        errors.push(agent.name.clone(), e.to_string());
                    }
                }
            }
            RunnableStage::Exec { stage: exec, .. } => {
                let mut fsubs = framework_subs.clone();
                fsubs.insert("name".to_string(), exec.name.clone());
                let loop_iter = fsubs.get("loop_iter").map(String::as_str).unwrap_or("1");

                match prepare_exec(exec, registry, loop_iter, &fsubs).await {
                    Ok(prepared) => commit_prepared_exec(registry, &prepared, errors).await,
                    Err(e) => {
                        errors.push(exec.name.clone(), e.to_string());
                    }
                }
            }
            RunnableStage::Sequence { attrs, body, .. } => {
                let key = if scope.is_empty() {
                    attrs.name.clone()
                } else {
                    format!("{}/{}", scope, attrs.name)
                };
                walk_stages(body, registry, framework_subs, &key, errors).await;
            }
            RunnableStage::Loop { attrs, body, .. } => {
                // One loop iteration: iteration 1.
                let loop_iter = format!("{}~1", attrs.name);
                let mut fsubs = framework_subs.clone();
                fsubs.insert("loop_iter".to_string(), loop_iter.clone());
                fsubs.insert("name".to_string(), attrs.name.clone());

                walk_stages(body, registry, &fsubs, &loop_iter, errors).await;
            }
            RunnableStage::Parallel { attrs: _, body, .. } => {
                // Each child gets its own isolated registry clone.
                for child in body {
                    let child_registry = registry.clone();
                    let mut fsubs = framework_subs.clone();
                    fsubs.insert("name".to_string(), child.name().to_string());

                    walk_stage(child, &child_registry, &fsubs, scope, errors).await;
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Commit helpers — register bind URIs so downstream stages see them
// ---------------------------------------------------------------------------

fn commit_prepared_agent<'a>(
    registry: &'a DryRunArtifactRegistry,
    prepared: &'a AgentPrepared,
    errors: &'a mut DryRunErrors,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
    Box::pin(async move {
        for (key, uri_str, _optional) in &prepared.bind_uris {
            let uri = match crate::artifacts::uri::Uri::parse(uri_str) {
                Ok(u) => u,
                Err(e) => {
                    errors.push(prepared.name.clone(), format!("invalid URI for {key}: {e}"));
                    continue;
                }
            };
            let path = match registry.path_for_uri(&uri).await {
                Ok(p) => p,
                Err(e) => {
                    errors.push(
                        prepared.name.clone(),
                        format!("failed to resolve path for {key}: {e}"),
                    );
                    continue;
                }
            };
            if let Err(e) = registry.commit(uri_str, &path).await {
                errors.push(
                    prepared.name.clone(),
                    format!("failed to commit {key}: {e}"),
                );
            }
        }
    })
}

fn commit_prepared_exec<'a>(
    registry: &'a DryRunArtifactRegistry,
    prepared: &'a ExecPrepared,
    errors: &'a mut DryRunErrors,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
    Box::pin(async move {
        for (key, uri_str, _optional) in &prepared.bind_uris {
            let uri = match crate::artifacts::uri::Uri::parse(uri_str) {
                Ok(u) => u,
                Err(e) => {
                    errors.push(prepared.name.clone(), format!("invalid URI for {key}: {e}"));
                    continue;
                }
            };
            let path = match registry.path_for_uri(&uri).await {
                Ok(p) => p,
                Err(e) => {
                    errors.push(
                        prepared.name.clone(),
                        format!("failed to resolve path for {key}: {e}"),
                    );
                    continue;
                }
            };
            if let Err(e) = registry.commit(uri_str, &path).await {
                errors.push(
                    prepared.name.clone(),
                    format!("failed to commit {key}: {e}"),
                );
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Bootstrap artifacts
// ---------------------------------------------------------------------------

/// Collect artifact keys the bootstrap declares so the dry-run registry can
/// seed them — stages that consume them will find them.
fn bootstrap_artifact_keys(definition: &GremlinDefinition) -> Vec<String> {
    let mut keys: Vec<String> = definition
        .bootstrap
        .cli_out
        .keys()
        .map(|name| {
            if name.starts_with("artifact://") {
                name.clone()
            } else {
                format!("artifact://{name}")
            }
        })
        .collect();

    // `launch_cmds` can contain `gremlins:bind_artifact` DSL calls — use the
    // same parser as the real bootstrap runner so validate agrees with runtime.
    for cmd in &definition.bootstrap.launch_cmds {
        if let Some((cmd_name, args)) = parse_gremlins_command(cmd) {
            if cmd_name == "bind_artifact" && !args.is_empty() {
                let uri = &args[0];
                if !uri.is_empty() {
                    let normalized = if uri.starts_with("artifact://") {
                        uri.clone()
                    } else {
                        format!("artifact://{uri}")
                    };
                    keys.push(normalized);
                }
            }
        }
    }

    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schemas::gremlin_definition::GremlinDefinition;
    use std::path::Path;

    fn write_fixture(dir: &Path, stem: &str, body: &str) -> std::path::PathBuf {
        let overlay = dir.join(".gremlins");
        std::fs::create_dir_all(&overlay).unwrap();
        let path = overlay.join(format!("{stem}.yaml"));
        std::fs::write(&path, body).unwrap();
        path
    }

    #[tokio::test]
    async fn valid_definition_passes() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(
            dir.path(),
            "demo",
            r#"
default_client: 'cmd:true'

stages:
  - name: plan
    type: agent
    prompt:
      - "hi\n"
  - name: run
    type: exec
    options:
      cmds: ["true"]
"#,
        );

        let definition = GremlinDefinition::from_yaml(&path, None).unwrap();
        let errors = validate_definition(&definition).await;
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    }

    #[tokio::test]
    async fn producer_consumer_chain_passes() {
        // Stage 1 produces artifact://data.txt, stage 2 consumes it.
        // The dry-run commits bind URIs so downstream stages find them.
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(
            dir.path(),
            "demo",
            r#"
default_client: 'cmd:true'

stages:
  - name: writer
    type: exec
    bind:
      out: "artifact://data.txt"
    options:
      cmds: ["echo hi > {out}"]
  - name: reader
    type: exec
    interpolation:
      src: content("artifact://data.txt")
    options:
      cmds: ["cat {src}"]
"#,
        );

        let definition = GremlinDefinition::from_yaml(&path, None).unwrap();
        let errors = validate_definition(&definition).await;
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    }

    #[tokio::test]
    async fn loop_body_duplicate_producer_caught_by_dry_run() {
        // Two children in a loop body both bind artifact://{loop_iter}/out.md.
        // The static validators check each child independently and don't see
        // the conflict. The dry-run resolves {loop_iter} → poll~1 and the
        // second child's prepare_exec detects the duplicate.
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(
            dir.path(),
            "demo",
            r#"
default_client: 'cmd:true'

stages:
  - name: poll
    type: loop
    max-iterations: 3
    body:
      - name: first
        type: exec
        bind:
          out: "artifact://{loop_iter}/out.md"
        options:
          cmds: ["true"]
      - name: second
        type: exec
        bind:
          out: "artifact://{loop_iter}/out.md"
        options:
          cmds: ["true"]
"#,
        );

        let definition = GremlinDefinition::from_yaml(&path, None).unwrap();
        let errors = validate_definition(&definition).await;
        assert!(
            !errors.is_empty(),
            "expected at least one error from dry-run"
        );
        let messages: Vec<String> = errors.iter().map(|e| e.message.clone()).collect();
        let has_duplicate = messages
            .iter()
            .any(|m| m.contains("duplicate producer") || m.contains("already produced"));
        assert!(
            has_duplicate,
            "expected duplicate producer error, got: {messages:?}"
        );
    }

    #[tokio::test]
    async fn loop_with_stop_when_exists_using_loop_iter_passes() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(
            dir.path(),
            "demo",
            r#"
default_client: 'cmd:true'

stages:
  - name: poll
    type: loop
    max-iterations: 3
    stop_when_exists: "artifact://{loop_iter}/done"
    body:
      - name: tick
        type: exec
        options:
          cmds: ["true"]
"#,
        );

        let definition = GremlinDefinition::from_yaml(&path, None).unwrap();
        let errors = validate_definition(&definition).await;
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    }

    #[tokio::test]
    async fn sequence_nested_passes() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(
            dir.path(),
            "demo",
            r#"
default_client: 'cmd:true'

stages:
  - type: sequence
    body:
      - name: one
        type: exec
        options:
          cmds: ["true"]
      - name: two
        type: exec
        options:
          cmds: ["true"]
"#,
        );

        let definition = GremlinDefinition::from_yaml(&path, None).unwrap();
        let errors = validate_definition(&definition).await;
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    }

    #[tokio::test]
    async fn parallel_children_get_isolated_registries() {
        // Two parallel children both bind the same artifact URI.
        // Because each gets its own registry clone, they don't conflict.
        let dir = tempfile::tempdir().unwrap();
        let path = write_fixture(
            dir.path(),
            "demo",
            r#"
default_client: 'cmd:true'

stages:
  - name: group
    parallel:
      - name: a
        type: exec
        bind:
          out: "artifact://out.md"
        options:
          cmds: ["true"]
      - name: b
        type: exec
        bind:
          out: "artifact://out.md"
        options:
          cmds: ["true"]
"#,
        );

        let definition = GremlinDefinition::from_yaml(&path, None).unwrap();
        let errors = validate_definition(&definition).await;
        // Parallel children have isolated registries, so no conflict.
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    }
}
