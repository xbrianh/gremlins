//! The gremlin handle: identity, environment, worktrees, and state.
//!
//! A [`Gremlin`] is the runtime handle for one run — the thing the run loop
//! drives a stage tree through. It owns the resolved [`Pipeline`], the
//! [`ArtifactRegistry`], and the [`StateData`] handle, and it is the single
//! place where a gremlin's environment is assembled.
//!
//! Three constructors cover the lifecycles the Python executor had:
//! [`Gremlin::launch`] starts a fresh run in its own detached worktree,
//! [`Gremlin::open`] reconstructs a handle from a persisted state directory
//! (for status checks and recovery), and [`Gremlin::fork`] spins off a child
//! that inherits the parent's artifacts, environment, and — optionally — a
//! fresh worktree at the parent's commit.
//!
//! The environment rules are the subtle part and live in [`resolve_env`]: the
//! eight `GREMLIN*` system variables are injected *after* any `bootstrap.env`
//! script has been sourced, so a script can shape the environment but can
//! never redirect the harness's own paths.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::artifacts::registry::ArtifactRegistry;
use crate::artifacts::uri::Uri;
use crate::clients::client::Client;
use crate::config;
use crate::core::{discovery, env_file, git};
use crate::executor::state::{self, StateData};
use crate::executor::RunError;
use crate::schemas::bootstrap::Bootstrap;
use crate::schemas::pipeline::Pipeline;
use crate::stages::node::RunnableStage;

/// State keys that describe *this* run's live execution and must never leak
/// into a forked child, which starts its own from scratch.
pub(crate) const FORK_TRANSIENT: [&str; 14] = [
    "parallel_worktrees",
    "done_children",
    "parallel_attempts",
    "active_children",
    "token_usage",
    "subprocess_cost_usd",
    "total_cost_usd",
    "stage",
    "stage_updated_at",
    "sub_stage",
    "ended_at",
    "status",
    "pid",
    "exit_code",
];

/// A validated gremlin id.
///
/// The only way to obtain one is [`validate_gremlin_id`], so a `GremlinId` in
/// hand is proof that the value is safe to use as a single path component:
/// `[A-Za-z0-9_-]+`, never empty, never containing `..`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GremlinId(String);

impl GremlinId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::ops::Deref for GremlinId {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for GremlinId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Validate `id` as a gremlin identifier and return it typed.
///
/// Mirrors the Python `validate_gremlin_id`: `..` anywhere is rejected (so the
/// id can never traverse out of the state or scratch root) and anything
/// outside `[A-Za-z0-9_-]` is rejected. The empty string is rejected too — the
/// Python regex could not match it either.
pub fn validate_gremlin_id(id: &str) -> Result<GremlinId, String> {
    let illegal = || format!("gremlin_id contains illegal characters: {id:?}");
    if id.is_empty() {
        return Err(illegal());
    }
    if id.contains("..") {
        return Err(illegal());
    }
    if !id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(illegal());
    }
    Ok(GremlinId(id.to_string()))
}

/// One gremlin run, reconstructed from its state directory.
pub struct Gremlin {
    pub id: GremlinId,
    pub state_dir: PathBuf,
    pub artifact_dir: PathBuf,
    pub pipeline: Pipeline,
    pub registry: ArtifactRegistry,
    pub worktree: Option<PathBuf>,
    pub worktree_parent: Option<PathBuf>,
    pub project_root: PathBuf,
    pub base_ref_sha: String,
    pub base_ref: String,
    pub resume_from: Option<String>,
    pub state: StateData,
    pub env: HashMap<String, String>,
    pub client: Client,
    pub loop_stack: Vec<(String, u32)>,
    /// The CLI/source values this run was launched with, keyed by source name.
    ///
    /// Bootstrap's `bind_artifact` DSL reads from here: a source key that is
    /// absent or empty is an optional source with nothing to bind.
    pub stage_inputs: HashMap<String, String>,
}

impl Gremlin {
    /// Start a fresh run of `pipeline_path` under the id `id`.
    ///
    /// The worktree is created before anything is persisted, so a launch that
    /// fails part-way removes the checkout it made rather than leaving it
    /// registered behind a state directory nobody can use. The checkout is
    /// branched from `HEAD`, and the commit it lands on is recorded as
    /// `worktree_base` in `state.json` and returned through
    /// [`Gremlin::base_ref_sha`]; resolving a *named* base ref (which may need
    /// a fetch, or a tag) is a launcher concern that feeds a later layer.
    #[allow(clippy::too_many_arguments)]
    pub fn launch(
        id: &str,
        pipeline_path: &Path,
        client_override: Option<&str>,
        worktree_parent: Option<&Path>,
        resume_from: Option<&str>,
        stage_inputs: &HashMap<String, String>,
        fetch_worktree: bool,
        worktree_dir: Option<&Path>,
    ) -> Result<Gremlin, RunError> {
        let gremlin_id = validate_gremlin_id(id).map_err(RunError::Message)?;

        let state_dir = config::state_root().join(gremlin_id.as_str());
        let artifact_dir = config::scratch_root(Some(gremlin_id.as_str())).join("artifacts");
        std::fs::create_dir_all(&state_dir)?;
        std::fs::create_dir_all(&artifact_dir)?;

        let pipeline_path = pipeline_path
            .canonicalize()
            .unwrap_or_else(|_| pipeline_path.to_path_buf());
        let pipeline = Pipeline::from_yaml(&pipeline_path, client_override)
            .map_err(|error| RunError::Message(error.to_string()))?;

        // An unusable client must not abort a launch: the state directory, the
        // worktree and the artifacts all have to exist before any stage can
        // run, and failing here would leave the operator with no run at all to
        // debug. `cmd:true` is the harness's own no-op client — it is what
        // `config::inject_sentinals` installs so a missing client never fails
        // structural validation — so a bad spec degrades to a run that does
        // nothing rather than one that never starts, while the pipeline's
        // declared `default_client` stays on record in `state.json`.
        let client = Client::parse(&pipeline.default_client).unwrap_or_else(|_| {
            Client::parse("cmd:true").expect("'cmd:true' is always a valid client spec")
        });

        let project_root = project_root_for(&pipeline_path);

        // A pre-set worktree means the caller owns the checkout; otherwise the
        // project has to be a repository before we try to branch one off it.
        if worktree_dir.is_none() && !git::in_git_repo(Some(&project_root)) {
            return Err(RunError::Git {
                message: format!("{project_root:?} is not a git repository"),
            });
        }

        // The launcher layer resolves the base commit before calling us; until
        // it does, "whatever HEAD is" is the only honest ref. The fallback
        // below records what that turned out to be.
        let base_ref_sha = String::new();
        let mut worktree = worktree_dir.map(Path::to_path_buf);
        let mut created_worktree: Option<String> = None;
        if worktree.is_none() && !project_root.as_os_str().is_empty() {
            let reference = if base_ref_sha.is_empty() {
                "HEAD"
            } else {
                base_ref_sha.as_str()
            };
            match git::setup_detached_worktree(
                &project_root,
                reference,
                fetch_worktree,
                worktree_parent,
            ) {
                Ok(path) => {
                    created_worktree = Some(path.clone());
                    worktree = Some(PathBuf::from(path));
                }
                Err(error) => {
                    return Err(RunError::Git {
                        message: error.to_string(),
                    })
                }
            }
        }

        // The checkout is the source of truth: when the launcher did not hand
        // us a SHA, the commit the worktree actually landed on is the base.
        let base_ref_sha = if base_ref_sha.is_empty() {
            worktree
                .as_deref()
                .map(|path| git::head_sha(Some(path)))
                .unwrap_or_default()
        } else {
            base_ref_sha
        };

        let launched = finish_launch(
            gremlin_id,
            &state_dir,
            &artifact_dir,
            &pipeline_path,
            pipeline,
            client,
            &project_root,
            worktree,
            worktree_parent,
            resume_from,
            stage_inputs,
            base_ref_sha,
        );
        match launched {
            Ok(gremlin) => Ok(gremlin),
            Err(error) => {
                // The worktree is ours only once `setup_detached_worktree` gave
                // us its path; if anything after that failed, unregister it.
                if let Some(path) = created_worktree {
                    git::remove_worktree(&project_root, &path);
                }
                Err(error)
            }
        }
    }

    /// Reconstruct a handle from a persisted state directory.
    ///
    /// `open` is deliberately lenient about a missing or unloadable pipeline —
    /// status checks must still work on a run whose pipeline has since been
    /// deleted — but strict about the state directory itself.
    pub fn open(id: &str) -> Result<Gremlin, RunError> {
        let gremlin_id = validate_gremlin_id(id).map_err(RunError::Message)?;
        let state_dir = config::state_root().join(gremlin_id.as_str());
        let state_file = state_dir.join("state.json");

        if !state_dir.is_dir() {
            return Err(RunError::Message(format!(
                "no state at {}",
                state_dir.display()
            )));
        }
        if !state_file.is_file() {
            return Err(RunError::Message(format!(
                "no state.json at {}",
                state_file.display()
            )));
        }

        // `read_state_json` is permissive by design ({} for missing or
        // unparseable); `open` needs the real reason, so an empty result is
        // re-parsed here to say which it was.
        let raw = state::read_state_json(Some(&state_file));
        if raw.is_empty() {
            let text = std::fs::read_to_string(&state_file)?;
            let detail = match serde_json::from_str::<Value>(&text) {
                Err(error) => format!("could not parse state.json: {error}"),
                Ok(Value::Object(_)) => "state.json must be a non-empty JSON object".to_string(),
                Ok(_) => "state.json must be a JSON object".to_string(),
            };
            return Err(RunError::Message(detail));
        }

        let kind = str_field(&raw, "kind");
        let project_root = {
            let from_state = str_field(&raw, "project_root");
            if from_state.is_empty() {
                config::project_root()
            } else {
                PathBuf::from(from_state)
            }
        };
        let pipeline_path = str_field(&raw, "pipeline_path");
        let workdir = str_field(&raw, "workdir");

        // A hermetic `pipeline.yaml` next to the state pins the pipeline the
        // run actually used; otherwise fall back to resolving the kind.
        let hermetic = state_dir.join("pipeline.yaml");
        let resolved: Option<PathBuf> = if hermetic.is_file() {
            Some(hermetic)
        } else if kind.is_empty() {
            None
        } else {
            resolve_pipeline_in_project(&kind, &project_root)
        };

        let candidate = resolved
            .clone()
            .or_else(|| (!pipeline_path.is_empty()).then(|| PathBuf::from(&pipeline_path)))
            .or_else(|| (!kind.is_empty()).then(|| PathBuf::from(&kind)));

        // A missing pipeline must not fail a status check, so a load failure
        // degrades to a stub carrying just the identity.
        let pipeline = match candidate.as_deref() {
            Some(path) => Pipeline::from_yaml(path, None)
                .unwrap_or_else(|_| stub_pipeline(&kind, path.to_path_buf())),
            None => stub_pipeline(&kind, PathBuf::from(".")),
        };

        let client = Client::parse(&pipeline.default_client)
            .or_else(|_| Client::parse("cmd:true"))
            .expect("'cmd:true' is always a valid client spec");

        let artifact_dir = config::scratch_root(Some(gremlin_id.as_str())).join("artifacts");
        let registry = ArtifactRegistry::new(artifact_dir.clone());
        let state = StateData::new(Some(gremlin_id.as_str().to_string()));

        let worktree = (!workdir.is_empty()).then(|| PathBuf::from(&workdir));
        let base_ref_sha = state.read_str("worktree_base");
        let base_ref = {
            let recorded = state.read_str("base_ref");
            if recorded.is_empty() {
                pipeline.base_ref.clone()
            } else {
                recorded
            }
        };

        let overlay_dir = state_dir.join(config::overlay_dirname());
        let env = resolve_env(
            bootstrap_script(&pipeline.bootstrap),
            &artifact_dir,
            &state_dir,
            gremlin_id.as_str(),
            &project_root,
            worktree.as_deref(),
            &overlay_dir,
        )?;

        // `stage_inputs` is what bootstrap's `bind_artifact` resolves against;
        // an absent or null field is an empty map, exactly as the Python
        // `state_json.get("stage_inputs") or {}` read it.
        let stage_inputs: HashMap<String, String> = state
            .read_field("stage_inputs")
            .and_then(|value| match value {
                Value::Object(map) => Some(
                    map.into_iter()
                        .map(|(key, value)| (key, value.as_str().unwrap_or("").to_string()))
                        .collect(),
                ),
                _ => None,
            })
            .unwrap_or_default();

        Ok(Gremlin {
            id: gremlin_id,
            state_dir,
            artifact_dir,
            pipeline,
            registry,
            worktree,
            worktree_parent: None,
            project_root,
            base_ref_sha,
            base_ref,
            resume_from: None,
            state,
            env,
            client,
            loop_stack: Vec::new(),
            stage_inputs,
        })
    }

    /// Fork a child gremlin: copy artifacts, branch a worktree at the parent's
    /// HEAD, and seed a fresh `state.json` carrying the child's identity.
    ///
    /// `child_pipeline_path` is the branch's hermetic `pipeline.yaml`; pass
    /// `None` to inherit the parent's persisted `pipeline_path`.
    pub fn fork(
        &self,
        child_id: &str,
        parent_id: &str,
        group_name: &str,
        child_key: &str,
        child_pipeline_path: Option<&Path>,
    ) -> Result<Gremlin, RunError> {
        let child_gremlin_id = validate_gremlin_id(child_id).map_err(RunError::Message)?;

        let child_state_dir = self
            .state_dir
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(child_gremlin_id.as_str());
        let child_artifact_dir =
            config::scratch_root(Some(child_gremlin_id.as_str())).join("artifacts");
        std::fs::create_dir_all(&child_state_dir)?;
        std::fs::create_dir_all(&child_artifact_dir)?;

        copy_tree(&self.artifact_dir, &child_artifact_dir)?;

        // A child of a run that has no worktree has none either: there is no
        // commit for it to branch from. When it does branch one, the commit it
        // landed on is the child's own base.
        let mut child_worktree: Option<PathBuf> = None;
        let mut child_worktree_base = String::new();
        if let Some(parent_worktree) = &self.worktree {
            let sha = git::head_sha(Some(parent_worktree));
            if sha.is_empty() {
                return Err(RunError::Message(format!(
                    "could not resolve HEAD in {}",
                    parent_worktree.display()
                )));
            }
            let path = git::setup_detached_worktree(
                &self.project_root,
                &sha,
                false,
                self.worktree_parent.as_deref(),
            )
            .map_err(|error| RunError::Git {
                message: error.to_string(),
            })?;
            child_worktree = Some(PathBuf::from(path));
            child_worktree_base = sha;
        }

        let registry = ArtifactRegistry::from_registry_file(
            &self.registry.registry_path,
            child_artifact_dir.clone(),
        )
        .map_err(|error| RunError::Message(error.to_string()))?;

        let parent = state::read_state_json(self.state.state_file.as_deref());
        let mut child = Map::new();
        for name in state::field_names() {
            if FORK_TRANSIENT.contains(&name) {
                continue;
            }
            if let Some(value) = parent.get(name) {
                child.insert(name.to_string(), value.clone());
            } else if let Some(default) = state::default_for(name) {
                child.insert(name.to_string(), default);
            }
        }
        for key in FORK_TRANSIENT {
            child.remove(key);
        }

        child.insert(
            "id".to_string(),
            Value::String(child_gremlin_id.to_string()),
        );
        child.insert(
            "parent_id".to_string(),
            Value::String(inherit(parent_id, &parent, "parent_id")),
        );
        child.insert(
            "group_name".to_string(),
            Value::String(inherit(group_name, &parent, "group_name")),
        );
        child.insert(
            "child_key".to_string(),
            Value::String(inherit(child_key, &parent, "child_key")),
        );
        child.insert(
            "pipeline_path".to_string(),
            Value::String(match child_pipeline_path {
                Some(path) => path.to_string_lossy().into_owned(),
                None => str_field(&parent, "pipeline_path"),
            }),
        );
        // A child that branched its own worktree must say so: leaving the
        // parent's `workdir` on record would point its commands, and any
        // `gremlins rm` cleanup, at the parent's checkout.
        if let Some(path) = &child_worktree {
            child.insert(
                "workdir".to_string(),
                Value::String(path.to_string_lossy().into_owned()),
            );
            child.insert(
                "worktree_base".to_string(),
                Value::String(child_worktree_base.clone()),
            );
        }

        child.insert("status".to_string(), Value::String("running".to_string()));
        child.insert("pid".to_string(), Value::Null);
        child.insert("exit_code".to_string(), Value::Null);

        state::write_state(&child_state_dir, &child)?;
        std::fs::write(child_state_dir.join("log"), "")?;

        let pipeline = match child_pipeline_path {
            Some(path) => Pipeline::from_yaml(path, None).unwrap_or_else(|_| self.pipeline.clone()),
            None => self.pipeline.clone(),
        };

        Ok(Gremlin {
            id: child_gremlin_id,
            state_dir: child_state_dir,
            artifact_dir: child_artifact_dir,
            pipeline,
            registry,
            worktree: child_worktree,
            worktree_parent: self.worktree_parent.clone(),
            project_root: self.project_root.clone(),
            base_ref_sha: child_worktree_base,
            base_ref: self.base_ref.clone(),
            resume_from: None,
            state: StateData::new(Some(child_id.to_string())),
            env: self.env.clone(),
            client: self.client.clone(),
            loop_stack: Vec::new(),
            // A child inherits the parent's source values: its bootstrap binds
            // the same inputs the parent launched with.
            stage_inputs: self.stage_inputs.clone(),
        })
    }

    /// Fork a child gremlin that runs only the given `stages`.
    ///
    /// Like [`Gremlin::fork`], but instead of loading a child pipeline from
    /// disk, the child inherits the parent's pipeline metadata and runs only
    /// the provided stage list. Used by the parallel executor so each child
    /// runs exactly one stage without needing a separate pipeline file.
    pub fn fork_with_stages(
        &self,
        child_id: &str,
        parent_id: &str,
        group_name: &str,
        child_key: &str,
        stages: Vec<RunnableStage>,
    ) -> Result<Gremlin, RunError> {
        let child_gremlin_id = validate_gremlin_id(child_id).map_err(RunError::Message)?;

        let child_state_dir = self
            .state_dir
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(child_gremlin_id.as_str());
        let child_artifact_dir =
            config::scratch_root(Some(child_gremlin_id.as_str())).join("artifacts");
        std::fs::create_dir_all(&child_state_dir)?;
        std::fs::create_dir_all(&child_artifact_dir)?;

        copy_tree(&self.artifact_dir, &child_artifact_dir)?;

        let mut child_worktree: Option<PathBuf> = None;
        let mut child_worktree_base = String::new();
        if let Some(parent_worktree) = &self.worktree {
            let sha = git::head_sha(Some(parent_worktree));
            if sha.is_empty() {
                return Err(RunError::Message(format!(
                    "could not resolve HEAD in {}",
                    parent_worktree.display()
                )));
            }
            let path = git::setup_detached_worktree(
                &self.project_root,
                &sha,
                false,
                self.worktree_parent.as_deref(),
            )
            .map_err(|error| RunError::Git {
                message: error.to_string(),
            })?;
            child_worktree = Some(PathBuf::from(path));
            child_worktree_base = sha;
        }

        let registry = ArtifactRegistry::from_registry_file(
            &self.registry.registry_path,
            child_artifact_dir.clone(),
        )
        .map_err(|error| RunError::Message(error.to_string()))?;

        let parent = state::read_state_json(self.state.state_file.as_deref());
        let mut child = Map::new();
        for name in state::field_names() {
            if FORK_TRANSIENT.contains(&name) {
                continue;
            }
            if let Some(value) = parent.get(name) {
                child.insert(name.to_string(), value.clone());
            } else if let Some(default) = state::default_for(name) {
                child.insert(name.to_string(), default);
            }
        }
        for key in FORK_TRANSIENT {
            child.remove(key);
        }

        child.insert(
            "id".to_string(),
            Value::String(child_gremlin_id.to_string()),
        );
        child.insert(
            "parent_id".to_string(),
            Value::String(inherit(parent_id, &parent, "parent_id")),
        );
        child.insert(
            "group_name".to_string(),
            Value::String(inherit(group_name, &parent, "group_name")),
        );
        child.insert(
            "child_key".to_string(),
            Value::String(inherit(child_key, &parent, "child_key")),
        );
        child.insert(
            "pipeline_path".to_string(),
            Value::String(str_field(&parent, "pipeline_path")),
        );
        if let Some(path) = &child_worktree {
            child.insert(
                "workdir".to_string(),
                Value::String(path.to_string_lossy().into_owned()),
            );
            child.insert(
                "worktree_base".to_string(),
                Value::String(child_worktree_base.clone()),
            );
        }

        child.insert("status".to_string(), Value::String("running".to_string()));
        child.insert("pid".to_string(), Value::Null);
        child.insert("exit_code".to_string(), Value::Null);

        state::write_state(&child_state_dir, &child)?;
        std::fs::write(child_state_dir.join("log"), "")?;

        let pipeline = self.pipeline.clone_with_stages(stages);

        Ok(Gremlin {
            id: child_gremlin_id,
            state_dir: child_state_dir,
            artifact_dir: child_artifact_dir,
            pipeline,
            registry,
            worktree: child_worktree,
            worktree_parent: self.worktree_parent.clone(),
            project_root: self.project_root.clone(),
            base_ref_sha: child_worktree_base,
            base_ref: self.base_ref.clone(),
            resume_from: None,
            state: StateData::new(Some(child_id.to_string())),
            env: self.env.clone(),
            client: self.client.clone(),
            loop_stack: Vec::new(),
            stage_inputs: self.stage_inputs.clone(),
        })
    }

    /// The directory commands run in: the worktree, else the project root, else
    /// the CWD.
    pub fn cwd(&self) -> PathBuf {
        if let Some(worktree) = &self.worktree {
            return worktree.clone();
        }
        if !self.project_root.as_os_str().is_empty() {
            return self.project_root.clone();
        }
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    }

    /// The framework substitution variables for `stage`.
    pub fn framework_subs(&self, stage: &RunnableStage) -> HashMap<String, String> {
        framework_subs(
            stage,
            &self.cwd().to_string_lossy(),
            self.client.model(),
            &self.base_ref,
        )
    }
}

/// The worktree of `pipeline_path`'s project.
///
/// Mirrors the private `project_root_for` in `schemas::pipeline`: the parent of
/// the nearest ancestor `.gremlins` directory, else the pipeline's own parent.
fn project_root_for(pipeline_path: &Path) -> PathBuf {
    let canonical = pipeline_path
        .canonicalize()
        .unwrap_or_else(|_| pipeline_path.to_path_buf());
    let mut current = canonical.parent();
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
    canonical
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The fallible remainder of [`Gremlin::launch`], after the worktree exists.
///
/// Split out so the caller can unregister the worktree if any of it fails.
#[allow(clippy::too_many_arguments)]
fn finish_launch(
    gremlin_id: GremlinId,
    state_dir: &Path,
    artifact_dir: &Path,
    pipeline_path: &Path,
    pipeline: Pipeline,
    client: Client,
    project_root: &Path,
    worktree: Option<PathBuf>,
    worktree_parent: Option<&Path>,
    resume_from: Option<&str>,
    stage_inputs: &HashMap<String, String>,
    base_ref_sha: String,
) -> Result<Gremlin, RunError> {
    let workdir = worktree
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    let pipeline_path_str = pipeline_path.to_string_lossy().into_owned();

    let mut state = StateData::new(Some(gremlin_id.as_str().to_string()));
    let inputs: Map<String, Value> = stage_inputs
        .iter()
        .map(|(key, value)| (key.clone(), Value::String(value.clone())))
        .collect();
    let mut initial = Map::new();
    initial.insert("kind".to_string(), Value::String(String::new()));
    initial.insert(
        "project_root".to_string(),
        Value::String(project_root.to_string_lossy().into_owned()),
    );
    initial.insert("workdir".to_string(), Value::String(workdir.clone()));
    initial.insert(
        "setup_kind".to_string(),
        Value::String("worktree-detached".to_string()),
    );
    initial.insert(
        "worktree_base".to_string(),
        Value::String(base_ref_sha.clone()),
    );
    initial.insert("status".to_string(), Value::String("running".to_string()));
    initial.insert("started_at".to_string(), Value::String(state::now_stamp()));
    initial.insert("description".to_string(), Value::String(String::new()));
    initial.insert("parent_id".to_string(), Value::String(String::new()));
    initial.insert("pipeline_args".to_string(), Value::Array(Vec::new()));
    initial.insert(
        "client".to_string(),
        Value::String(pipeline.default_client.clone()),
    );
    initial.insert(
        "pipeline_path".to_string(),
        Value::String(pipeline_path_str.clone()),
    );
    initial.insert("stage".to_string(), Value::String("starting".to_string()));
    initial.insert("pid".to_string(), Value::Null);
    initial.insert("stage_inputs".to_string(), Value::Object(inputs));
    initial.insert("attempt".to_string(), Value::String(String::new()));
    initial.insert("group_name".to_string(), Value::String(String::new()));
    initial.insert("child_key".to_string(), Value::String(String::new()));
    initial.insert("exit_code".to_string(), Value::Null);
    state.persist(state_dir, &initial)?;

    // Re-assert the worktree fields now that the checkout exists, matching the
    // patch the Python initializer applied after `setup_workdir`.
    let mut patch = Map::new();
    patch.insert("workdir".to_string(), Value::String(workdir));
    patch.insert(
        "worktree_base".to_string(),
        Value::String(base_ref_sha.clone()),
    );
    patch.insert(
        "setup_kind".to_string(),
        Value::String("worktree-detached".to_string()),
    );
    state.patch(&[], &patch);

    stage_overlay(project_root, state_dir);

    let registry = ArtifactRegistry::new(artifact_dir.to_path_buf());
    register_stage_inputs(&registry, &pipeline.bootstrap, stage_inputs);
    register_base_sha(&registry, worktree.as_deref().unwrap_or(project_root));

    let overlay_dir = state_dir.join(config::overlay_dirname());
    let env = resolve_env(
        bootstrap_script(&pipeline.bootstrap),
        artifact_dir,
        state_dir,
        gremlin_id.as_str(),
        project_root,
        worktree.as_deref(),
        &overlay_dir,
    )?;

    let base_ref = pipeline.base_ref.clone();
    Ok(Gremlin {
        id: gremlin_id,
        state_dir: state_dir.to_path_buf(),
        artifact_dir: artifact_dir.to_path_buf(),
        pipeline,
        registry,
        worktree,
        worktree_parent: worktree_parent.map(Path::to_path_buf),
        project_root: project_root.to_path_buf(),
        base_ref_sha,
        base_ref,
        resume_from: resume_from.map(String::from),
        state,
        env,
        client,
        loop_stack: Vec::new(),
        stage_inputs: stage_inputs.clone(),
    })
}

/// Register each stage input that is not a bootstrap source as an artifact.
///
/// An empty value is skipped, matching the Python initializer this replaces: a
/// stage input the operator left blank is indistinguishable from one they never
/// supplied, and registering it would create an empty artifact that a later
/// `skip_if_exists` check would read as satisfied. Failures are logged and
/// skipped too — a stage input that cannot be written is worth complaining
/// about, but it must not abort a launch, because the stage that wanted the
/// artifact will report it precisely.
fn register_stage_inputs(
    registry: &ArtifactRegistry,
    bootstrap: &Bootstrap,
    stage_inputs: &HashMap<String, String>,
) {
    let source_keys: Vec<String> = bootstrap
        .source
        .as_ref()
        .map(|source| source.all_sources())
        .unwrap_or_default();
    for (key, value) in stage_inputs {
        if value.is_empty() || source_keys.iter().any(|source| source == key) {
            continue;
        }
        let uri_str = format!("artifact://{key}");
        if registry.is_live(&uri_str) {
            continue;
        }
        match Uri::parse(&uri_str) {
            Ok(uri) => {
                if let Err(error) = registry.write_into_registry(&uri, value) {
                    log::warn!("launch: could not register stage input {key:?}: {error}");
                }
            }
            Err(error) => log::warn!("launch: invalid stage input URI {uri_str:?}: {error}"),
        }
    }
}

/// Record the commit the run started from, once, as `artifact://base_sha`.
fn register_base_sha(registry: &ArtifactRegistry, cwd: &Path) {
    if registry.is_live("artifact://base_sha") {
        return;
    }
    let sha = git::head_sha(Some(cwd));
    if sha.is_empty() {
        return;
    }
    if let Ok(uri) = Uri::parse("artifact://base_sha") {
        if let Err(error) = registry.write_into_registry(&uri, &sha) {
            log::warn!("launch: could not register artifact://base_sha: {error}");
        }
    }
}

/// Copy the project's `.gremlins` overlay into `state_dir`, best-effort.
///
/// Nothing is copied when the project has no overlay, or when the overlay and
/// the destination are the same directory (a state dir inside the project).
fn stage_overlay(project_root: &Path, state_dir: &Path) {
    let dirname = config::overlay_dirname();
    let source = project_root.join(dirname);
    if !source.is_dir() {
        return;
    }
    let destination = state_dir.join(dirname);
    let same = match (source.canonicalize(), destination.canonicalize()) {
        (Ok(from), Ok(to)) => from == to,
        _ => false,
    };
    if same {
        return;
    }
    if let Err(error) = copy_tree(&source, &destination) {
        log::warn!(
            "launch: could not stage {} into {}: {error}",
            source.display(),
            destination.display()
        );
    }
}

/// Recursively copy `source` into `destination`, creating directories as
/// needed. A missing source is not an error — it simply copies nothing.
pub(crate) fn copy_tree(source: &Path, destination: &Path) -> Result<(), RunError> {
    if !source.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Resolve a pipeline kind against `project_root`, ignoring the process-wide
/// overlay override.
///
/// Discovery threads the project root through its `base_dir` argument, but it
/// would otherwise read `GREMLINS_OVERLAY_DIR` from the process environment,
/// and a running gremlin exports that variable as its own overlay path. Naming
/// the project's own overlay explicitly is what lets `status` look inside the
/// project the state file names rather than the overlay of whoever asked —
/// without the process-global env ever being cleared, and so without a lock.
fn resolve_pipeline_in_project(kind: &str, project_root: &Path) -> Option<PathBuf> {
    let overlay = config::overlay_dir_without_env(project_root);
    discovery::resolve_pipeline_path_in(kind, Some(&overlay), project_root.to_path_buf()).ok()
}

/// A pipeline placeholder carrying only an identity, for a run whose pipeline
/// can no longer be loaded.
fn stub_pipeline(kind: &str, path: PathBuf) -> Pipeline {
    Pipeline {
        name: if kind.is_empty() {
            "unknown".to_string()
        } else {
            kind.to_string()
        },
        path,
        default_client: String::new(),
        base_ref: String::new(),
        bootstrap: Bootstrap::default(),
        stages: Vec::new(),
        land: None,
    }
}

/// Read a string field, treating anything else as `""`.
fn str_field(map: &Map<String, Value>, field: &str) -> String {
    map.get(field)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// A fork's identity field: the caller's value when non-empty, else the
/// parent's.
fn inherit(value: &str, parent: &Map<String, Value>, field: &str) -> String {
    if value.is_empty() {
        str_field(parent, field)
    } else {
        value.to_string()
    }
}

/// The bootstrap environment script, or `None` when it is absent or blank.
fn bootstrap_script(bootstrap: &Bootstrap) -> Option<&str> {
    let script = bootstrap.env.trim();
    if script.is_empty() {
        None
    } else {
        Some(script)
    }
}

/// Runtime-owned substitution vars. Stages must not assemble these themselves.
pub fn framework_subs(
    stage: &RunnableStage,
    cwd: &str,
    model: &str,
    base_ref: &str,
) -> HashMap<String, String> {
    HashMap::from([
        ("name".to_string(), stage.name().to_string()),
        ("model".to_string(), model.to_string()),
        ("cwd".to_string(), cwd.to_string()),
        ("base_ref".to_string(), base_ref.to_string()),
    ])
}

/// Resolve the process environment a gremlin's stages and bootstrap inherit.
///
/// The order is what matters: the bootstrap script (when there is one) runs
/// first, against the current environment, and *replaces* it — that is the
/// script's whole job — after which the eight system variables are written on
/// top. System variables therefore win unconditionally, so no script can
/// redirect the harness's paths.
pub fn resolve_env(
    bootstrap_env: Option<&str>,
    artifact_dir: &Path,
    state_dir: &Path,
    gremlin_id: &str,
    project_root: &Path,
    worktree: Option<&Path>,
    overlay_dir: &Path,
) -> Result<HashMap<String, String>, RunError> {
    let base: HashMap<String, String> = std::env::vars().collect();

    let mut env = match bootstrap_env {
        Some(script) if !script.trim().is_empty() => {
            env_file::source_env_string(script, &base, Some(project_root)).map_err(|error| {
                RunError::BootstrapFailed {
                    exit_code: 1,
                    stderr: error.to_string(),
                }
            })?
        }
        _ => base,
    };

    let worktree_path = worktree
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Without a worktree there is no workspace, so the process CWD stands in.
    let workspace_dir = if worktree_path.is_empty() {
        std::env::current_dir()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    } else {
        worktree_path.clone()
    };
    // `scratch_root(id)` is the artifact dir's parent. When the artifact dir
    // has no usable parent, fall back to resolving the scratch root outright so
    // the variable is never empty.
    let scratch_dir = artifact_dir
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| config::scratch_root(Some(gremlin_id)));

    env.insert("GREMLINS_GREMLIN_ID".to_string(), gremlin_id.to_string());
    env.insert(
        "GREMLINS_PROJECT_ROOT".to_string(),
        project_root.to_string_lossy().into_owned(),
    );
    env.insert(
        "GREMLINS_OVERLAY_DIR".to_string(),
        overlay_dir.to_string_lossy().into_owned(),
    );
    env.insert("GREMLINS_WORKTREE_PATH".to_string(), worktree_path);
    env.insert(
        "GREMLINS_ARTIFACT_DIR".to_string(),
        artifact_dir.to_string_lossy().into_owned(),
    );
    env.insert("GREMLIN_WORKSPACE_DIR".to_string(), workspace_dir);
    env.insert(
        "GREMLIN_STATE_DIR".to_string(),
        state_dir.to_string_lossy().into_owned(),
    );
    env.insert(
        "GREMLINS_SCRATCH_DIR".to_string(),
        scratch_dir.to_string_lossy().into_owned(),
    );
    Ok(env)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::{with_sandbox, EnvGuard};

    #[test]
    fn gremlin_is_send_sync() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        assert_send::<Gremlin>();
        assert_sync::<Gremlin>();
    }

    // --- id validation ---

    #[test]
    fn gremlin_id_accepts_safe_ids() {
        for id in ["abc", "a-b_c9", "A1"] {
            let validated = validate_gremlin_id(id).unwrap_or_else(|e| panic!("{id}: {e}"));
            assert_eq!(validated.as_str(), id);
        }
    }

    #[test]
    fn gremlin_id_rejects_traversal_and_junk() {
        for id in ["", "..", "a..b", "a/b", "a b", "a.b"] {
            let error = validate_gremlin_id(id).expect_err(id);
            assert!(
                error.contains(&format!("{id:?}")),
                "message should name the id: {error}"
            );
        }

        let id = validate_gremlin_id("a-b_c9").unwrap();
        assert_eq!(&*id, "a-b_c9");
        assert_eq!(id.to_string(), "a-b_c9");
        assert_eq!(id.as_str(), "a-b_c9");
        assert_eq!(id.into_string(), "a-b_c9");
    }

    // --- environment resolution ---

    #[test]
    fn resolve_env_injects_system_vars() {
        let dir = tempfile::tempdir().unwrap();
        let artifact_dir = dir.path().join("scratch").join("gr-test").join("artifacts");
        let state_dir = dir.path().join("state").join("gr-test");
        let project_root = dir.path().join("project");
        let overlay_dir = state_dir.join(config::overlay_dirname());
        std::fs::create_dir_all(&artifact_dir).unwrap();
        std::fs::create_dir_all(&overlay_dir).unwrap();

        let env = resolve_env(
            None,
            &artifact_dir,
            &state_dir,
            "gr-test",
            &project_root,
            None,
            &overlay_dir,
        )
        .unwrap();

        assert_eq!(env["GREMLINS_GREMLIN_ID"], "gr-test");
        assert_eq!(env["GREMLINS_PROJECT_ROOT"], project_root.to_string_lossy());
        assert_eq!(env["GREMLINS_OVERLAY_DIR"], overlay_dir.to_string_lossy());
        assert_eq!(env["GREMLINS_WORKTREE_PATH"], "");
        assert_eq!(env["GREMLINS_ARTIFACT_DIR"], artifact_dir.to_string_lossy());
        assert_eq!(
            env["GREMLIN_WORKSPACE_DIR"],
            std::env::current_dir().unwrap().to_string_lossy()
        );
        assert_eq!(env["GREMLIN_STATE_DIR"], state_dir.to_string_lossy());
        assert_eq!(
            env["GREMLINS_SCRATCH_DIR"],
            artifact_dir.parent().unwrap().to_string_lossy()
        );
    }

    /// The absolute path of `env`, so a bootstrap script that replaces `PATH`
    /// can still reach the binary `source_env_string` uses to read the
    /// environment back.
    fn env_binary() -> String {
        let path = std::env::var("PATH").unwrap_or_default();
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("env");
            if candidate.is_file() {
                return candidate.to_string_lossy().into_owned();
            }
        }
        "env".to_string()
    }

    #[test]
    fn resolve_env_sources_and_keeps_system_vars() {
        let dir = tempfile::tempdir().unwrap();
        let artifact_dir = dir.path().join("scratch").join("gr-test").join("artifacts");
        let state_dir = dir.path().join("state").join("gr-test");
        let project_root = dir.path().join("project");
        let overlay_dir = state_dir.join(config::overlay_dirname());
        std::fs::create_dir_all(&artifact_dir).unwrap();
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::create_dir_all(&overlay_dir).unwrap();

        // The script replaces `PATH`, so it pins `env`'s location first: the
        // loader reads the resulting environment back with `env -0`.
        let script = format!(
            "export GREMLINS_TEST_SOURCED=yes\nexport PATH=/custom\nhash -p {} env\n",
            env_binary(),
        );
        let env = resolve_env(
            Some(&script),
            &artifact_dir,
            &state_dir,
            "gr-test",
            &project_root,
            None,
            &overlay_dir,
        )
        .unwrap();

        assert_eq!(env["GREMLINS_TEST_SOURCED"], "yes");
        // The sourced map replaces the base, so the script's PATH is what wins.
        assert_eq!(env["PATH"], "/custom");
        assert_eq!(env["GREMLINS_GREMLIN_ID"], "gr-test");
        assert_eq!(env["GREMLINS_PROJECT_ROOT"], project_root.to_string_lossy());
        assert_eq!(env["GREMLINS_OVERLAY_DIR"], overlay_dir.to_string_lossy());
        assert_eq!(env["GREMLINS_WORKTREE_PATH"], "");
        assert_eq!(env["GREMLINS_ARTIFACT_DIR"], artifact_dir.to_string_lossy());
        assert_eq!(
            env["GREMLIN_WORKSPACE_DIR"],
            std::env::current_dir().unwrap().to_string_lossy()
        );
        assert_eq!(env["GREMLIN_STATE_DIR"], state_dir.to_string_lossy());
        assert_eq!(
            env["GREMLINS_SCRATCH_DIR"],
            artifact_dir.parent().unwrap().to_string_lossy()
        );
    }

    #[test]
    fn resolve_env_reports_bootstrap_failure() {
        let dir = tempfile::tempdir().unwrap();
        let artifact_dir = dir.path().join("scratch").join("gr-test").join("artifacts");
        let state_dir = dir.path().join("state").join("gr-test");
        std::fs::create_dir_all(&artifact_dir).unwrap();
        std::fs::create_dir_all(&state_dir).unwrap();

        let error = resolve_env(
            Some("exit 3"),
            &artifact_dir,
            &state_dir,
            "gr-test",
            dir.path(),
            None,
            &state_dir.join(config::overlay_dirname()),
        )
        .unwrap_err();
        assert!(
            matches!(error, RunError::BootstrapFailed { .. }),
            "unexpected {error:?}"
        );
    }

    // --- framework substitutions ---

    #[test]
    fn framework_subs_carries_the_four_vars() {
        let stage = RunnableStage::Exec {
            stage: crate::stages::exec::Exec {
                name: "plan".to_string(),
                options: HashMap::new(),
                interpolation_map: HashMap::new(),
                bind_map: HashMap::new(),
            },
            skip_if_exists: String::new(),
            client: None,
        };

        let subs = framework_subs(&stage, "/w", "xai:grok-4", "main");
        assert_eq!(subs.len(), 4);
        assert_eq!(subs["name"], "plan");
        assert_eq!(subs["cwd"], "/w");
        assert_eq!(subs["model"], "xai:grok-4");
        assert_eq!(subs["base_ref"], "main");
    }

    // --- git-backed lifecycle ---

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    fn git(root: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .expect("failed to run git")
    }

    /// A repository with one commit and a `.gremlins/demo.yaml` pipeline.
    fn init_repo(root: &Path) -> bool {
        if !git(root, &["init", "-q"]).status.success() {
            return false;
        }
        let overlay = root.join(".gremlins");
        if std::fs::create_dir_all(&overlay).is_err() {
            return false;
        }
        if std::fs::write(
            overlay.join("demo.yaml"),
            "default_client: 'cmd:true'\nstages: []\n",
        )
        .is_err()
        {
            return false;
        }
        if !git(root, &["add", "."]).status.success() {
            return false;
        }
        git(
            root,
            &[
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=Test",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        )
        .status
        .success()
    }

    fn read_state(path: &Path) -> Value {
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path:?}: {e}"))
    }

    #[test]
    fn launch_creates_state_and_worktree() {
        if !git_available() {
            eprintln!("git is unavailable; skipping launch_creates_state_and_worktree");
            return;
        }
        with_sandbox(None, |sandbox| {
            let repo = tempfile::tempdir().unwrap();
            if !init_repo(repo.path()) {
                eprintln!("could not prepare a git fixture; skipping");
                return;
            }
            let pipeline_path = repo.path().join(".gremlins").join("demo.yaml");

            let gremlin = Gremlin::launch(
                "gr-test",
                &pipeline_path,
                None,
                None,
                None,
                &HashMap::new(),
                false,
                None,
            )
            .unwrap();

            let worktree = gremlin.worktree.clone().expect("a worktree");
            assert!(worktree.is_dir(), "{worktree:?}");

            let state_file = sandbox.join("state").join("gr-test").join("state.json");
            assert!(state_file.is_file(), "{state_file:?}");
            let raw = read_state(&state_file);
            assert_eq!(raw["id"], "gr-test");
            assert_eq!(raw["status"], "running");
            assert!(raw["pipeline_path"]
                .as_str()
                .unwrap()
                .ends_with("demo.yaml"));

            // The checkout and the commit it was branched from are on record,
            // and the returned handle agrees with what was persisted.
            assert_eq!(raw["workdir"].as_str().unwrap(), worktree.to_string_lossy());
            let base = raw["worktree_base"].as_str().unwrap();
            assert_eq!(base.len(), 40, "worktree_base should be a SHA: {base:?}");
            assert_eq!(base, gremlin.base_ref_sha);

            assert_eq!(gremlin.env["GREMLINS_GREMLIN_ID"], "gr-test");
            assert!(gremlin.registry.artifact_dir.ends_with("artifacts"));
        });
    }

    #[test]
    fn launch_then_open_roundtrips() {
        if !git_available() {
            eprintln!("git is unavailable; skipping launch_then_open_roundtrips");
            return;
        }
        with_sandbox(None, |_sandbox| {
            let repo = tempfile::tempdir().unwrap();
            if !init_repo(repo.path()) {
                eprintln!("could not prepare a git fixture; skipping");
                return;
            }
            let pipeline_path = repo.path().join(".gremlins").join("demo.yaml");

            let launched = Gremlin::launch(
                "gr-test",
                &pipeline_path,
                None,
                None,
                None,
                &HashMap::new(),
                false,
                None,
            )
            .unwrap();
            let opened = Gremlin::open("gr-test").unwrap();

            assert_eq!(opened.id, launched.id);
            assert_eq!(opened.state_dir, launched.state_dir);
            assert_eq!(opened.project_root, launched.project_root);
            assert_eq!(opened.pipeline.name, launched.pipeline.name);
            assert_eq!(opened.env["GREMLINS_GREMLIN_ID"], "gr-test");
            assert_eq!(opened.worktree, launched.worktree);
        });
    }

    #[test]
    fn fork_copies_artifacts_and_seeds_child_state() {
        if !git_available() {
            eprintln!("git is unavailable; skipping fork_copies_artifacts_and_seeds_child_state");
            return;
        }
        with_sandbox(None, |sandbox| {
            let repo = tempfile::tempdir().unwrap();
            if !init_repo(repo.path()) {
                eprintln!("could not prepare a git fixture; skipping");
                return;
            }
            let pipeline_path = repo.path().join(".gremlins").join("demo.yaml");

            let parent = Gremlin::launch(
                "gr-test",
                &pipeline_path,
                None,
                None,
                None,
                &HashMap::new(),
                false,
                None,
            )
            .unwrap();

            std::fs::write(parent.artifact_dir.join("note.txt"), "hello").unwrap();

            let child = parent.fork("gr-child", "", "", "", None).unwrap();

            assert_ne!(child.artifact_dir, parent.artifact_dir);
            assert!(child.artifact_dir.ends_with("artifacts"));
            assert!(child.artifact_dir.join("note.txt").is_file());

            let child_state = sandbox.join("state").join("gr-child").join("state.json");
            let raw = read_state(&child_state);
            assert_eq!(raw["id"], "gr-child");
            assert_eq!(raw["status"], "running");
            assert!(raw["pid"].is_null());
            assert!(raw.get("token_usage").is_none());

            let parent_worktree = parent.worktree.clone().unwrap();
            let child_worktree = child.worktree.clone().expect("child worktree");
            assert_ne!(child_worktree, parent_worktree);
            assert!(child_worktree.is_dir(), "{child_worktree:?}");

            // The child's own checkout is on record, never the parent's.
            assert_eq!(
                raw["workdir"].as_str().unwrap(),
                child_worktree.to_string_lossy()
            );
            let base = raw["worktree_base"].as_str().unwrap();
            assert_eq!(base.len(), 40, "worktree_base should be a SHA: {base:?}");
            assert_eq!(base, child.base_ref_sha);

            assert_eq!(child.env, parent.env);
        });
    }

    #[test]
    fn fork_keeps_parent_id_unless_one_is_given() {
        if !git_available() {
            eprintln!("git is unavailable; skipping fork_keeps_parent_id_unless_one_is_given");
            return;
        }
        with_sandbox(None, |sandbox| {
            let repo = tempfile::tempdir().unwrap();
            if !init_repo(repo.path()) {
                eprintln!("could not prepare a git fixture; skipping");
                return;
            }
            let pipeline_path = repo.path().join(".gremlins").join("demo.yaml");

            let parent = Gremlin::launch(
                "gr-test",
                &pipeline_path,
                None,
                None,
                None,
                &HashMap::new(),
                false,
                None,
            )
            .unwrap();

            // Mirrors the reference implementation: an empty argument falls
            // back to whatever the parent state carries (usually the parent's
            // own parent, or nothing at all), never to the parent's own id.
            parent.fork("gr-a", "", "", "", None).unwrap();
            parent
                .fork("gr-b", "gr-root", "group", "key", None)
                .unwrap();

            for (child_id, expected) in [("gr-a", ""), ("gr-b", "gr-root")] {
                let raw = read_state(&sandbox.join("state").join(child_id).join("state.json"));
                assert_eq!(raw["parent_id"], expected, "{child_id}");
            }

            let grouped = read_state(&sandbox.join("state").join("gr-b").join("state.json"));
            assert_eq!(grouped["group_name"], "group");
            assert_eq!(grouped["child_key"], "key");
        });
    }

    #[test]
    fn resolve_pipeline_in_project_ignores_the_overlay_override() {
        let mut env = EnvGuard::lock();
        let project = tempfile::tempdir().unwrap();
        let overlay = project.path().join(config::overlay_dirname());
        std::fs::create_dir_all(&overlay).unwrap();
        std::fs::write(
            overlay.join("demo.yaml"),
            "default_client: 'cmd:true'\nstages: []\n",
        )
        .unwrap();

        // A running gremlin exports its own overlay path; `open` still has to
        // find the pipeline inside the project the state file names.
        env.set("GREMLINS_OVERLAY_DIR", "/nonexistent/overlay");
        let found = resolve_pipeline_in_project("demo", project.path());
        let missing = resolve_pipeline_in_project("nope", project.path());

        assert!(found.is_some_and(|path| path.ends_with("demo.yaml")));
        assert!(missing.is_none());
    }
}
