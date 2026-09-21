//! The gremlin handle: identity, environment, worktrees, and state.
//!
//! A [`Gremlin`] is the runtime handle for one run — the thing the run loop
//! drives a stage tree through. It owns the resolved [`GremlinDefinition`], the
//! [`ArtifactRegistry`], and the [`StateData`] handle, and it is the single
//! place where a gremlin's environment is assembled.
//!
//! Three constructors cover the lifecycles the Python executor had:
//! [`Gremlin::create`] starts a fresh run in its own detached worktree,
//! [`Gremlin::from`] reconstructs a handle from a persisted state directory
//! (for status checks, cleanup, and recovery), and [`Gremlin::fork`] spins off
//! a child that inherits the parent's artifacts, environment, and — optionally
//! — a fresh worktree at the parent's commit.
//!
//! Construction is cheap on purpose. `create` and `from` populate only the
//! path and identity fields; the definition YAML, the artifact registry, the
//! client, and the resolved environment are deferred to
//! [`Gremlin::init_runtime`], which [`Gremlin::run`] calls before it drives a
//! single stage. A caller that only wants a handle — to read metadata, or to
//! `clean` — never pays for the runtime.
//!
//! The environment rules are the subtle part and live in [`resolve_env`]: the
//! eight `GREMLIN*` system variables are seeded into the base environment
//! *before* any `bootstrap.env` script is sourced (so the script can read
//! them, e.g. to point `VIRTUAL_ENV` at the worktree's venv), then re-asserted
//! on top afterwards — a script can shape the environment but can never
//! redirect the harness's own paths.

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
use crate::schemas::gremlin_definition::GremlinDefinition;
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
    /// Where the definition YAML lives, resolved at construction time but not
    /// read until [`Gremlin::init_runtime`]. `None` means no definition could be
    /// located — a handle that reaches `init_runtime` without one is an error.
    pub definition_path: Option<PathBuf>,
    /// The CLI `--client` value this run was launched with, replayed when the
    /// definition is finally loaded.
    pub client_override: Option<String>,
    pub definition: GremlinDefinition,
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
    /// Start a fresh run of `definition_path` under the id `id`.
    ///
    /// The worktree is created before anything is persisted, so a launch that
    /// fails part-way removes the checkout it made rather than leaving it
    /// registered behind a state directory nobody can use. The checkout is
    /// branched from `HEAD`, and the commit it lands on is recorded as
    /// `worktree_base` in `state.json` and returned through
    /// [`Gremlin::base_ref_sha`].
    ///
    /// The definition YAML is *not* read here: the handle carries the path and
    /// the `--client` override, and [`Gremlin::init_runtime`] loads them the
    /// first time [`Gremlin::run`] is called.
    ///
    /// `base_ref_sha`, when non-empty, is the commit the worktree branches
    /// from. Omitting it (or passing an empty string) falls back to`HEAD`.
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        id: &str,
        definition_path: &Path,
        client_override: Option<&str>,
        worktree_parent: Option<&Path>,
        resume_from: Option<&str>,
        stage_inputs: &HashMap<String, String>,
        fetch_worktree: bool,
        worktree_dir: Option<&Path>,
        base_ref: Option<&str>,
        base_ref_sha: Option<&str>,
    ) -> Result<Gremlin, RunError> {
        let gremlin_id = validate_gremlin_id(id).map_err(RunError::Message)?;

        let state_dir = config::state_root().join(gremlin_id.as_str());
        let artifact_dir = state_dir.join("artifacts");
        std::fs::create_dir_all(&state_dir)?;
        std::fs::create_dir_all(&artifact_dir)?;

        let definition_path = definition_path
            .canonicalize()
            .unwrap_or_else(|_| definition_path.to_path_buf());

        let project_root = project_root_for(&definition_path);

        // A pre-set worktree means the caller owns the checkout; otherwise the
        // project has to be a repository before we try to branch one off it.
        if worktree_dir.is_none() && !git::in_git_repo(Some(&project_root)) {
            return Err(RunError::Git {
                message: format!("{project_root:?} is not a git repository"),
            });
        }

        // The checkout is branched from the provided base ref when given,
        // otherwise from `HEAD`; the commit it lands on is the base recorded
        // below.
        let mut worktree = worktree_dir.map(Path::to_path_buf);
        let mut created_worktree: Option<String> = None;
        let branch_ref = base_ref_sha.filter(|sha| !sha.is_empty()).unwrap_or("HEAD");
        if worktree.is_none() && !project_root.as_os_str().is_empty() {
            match git::setup_detached_worktree(
                &project_root,
                branch_ref,
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

        // The checkout is the source of truth: the commit the worktree landed
        // on is the base, or empty when there is no worktree.
        let base_ref_sha = worktree
            .as_deref()
            .map(|path| git::head_sha(Some(path)))
            .unwrap_or_default();

        let created = write_launch_state(
            gremlin_id,
            &state_dir,
            &artifact_dir,
            &definition_path,
            client_override,
            &project_root,
            worktree,
            worktree_parent,
            resume_from,
            stage_inputs,
            base_ref_sha,
            base_ref.unwrap_or(""),
        );
        match created {
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

    /// Reconstruct a handle from a persisted state directory, without touching
    /// the definition, the client, or the environment.
    ///
    /// This is the cheap read: it resolves the paths a caller needs to inspect
    /// a run — its worktree, its project, its definition — and nothing more. A
    /// caller that only wants to read metadata, or to `clean` the run up,
    /// never pays for a YAML parse, a client build, or a bootstrap `source`.
    ///
    /// `from` is deliberately lenient about a missing or unloadable definition —
    /// status checks must still work on a run whose definition has since been
    /// deleted — but strict about the state directory itself. The definition is
    /// loaded later, by [`Gremlin::init_runtime`], which is also where a
    /// missing definition becomes a hard error.
    pub fn from(id: &str) -> Result<Gremlin, RunError> {
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
        // unparseable); `from` needs the real reason, so an empty result is
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
        let recorded_path = str_field(&raw, "definition_path");
        let workdir = str_field(&raw, "workdir");

        // A hermetic `definition.yaml` next to the state pins the definition the
        // run actually used; otherwise fall back to resolving the kind. Either
        // way the path is only recorded here — `init_runtime` reads it.
        let hermetic = state_dir.join("definition.yaml");
        let definition_path = if hermetic.is_file() {
            Some(hermetic)
        } else if !kind.is_empty() {
            resolve_definition_in_project(&kind, &project_root)
        } else {
            None
        }
        .or_else(|| (!recorded_path.is_empty()).then(|| PathBuf::from(&recorded_path)))
        // Canonicalise so a caller comparing the handle's path to a resolved
        // one (the way `definition.path` is read) sees the same string.
        .map(|path| path.canonicalize().unwrap_or(path));

        let artifact_dir = state_dir.join("artifacts");
        let state = StateData::new(Some(gremlin_id.as_str().to_string()));

        let worktree = (!workdir.is_empty()).then(|| PathBuf::from(&workdir));
        let base_ref_sha = state.read_str("worktree_base");
        let base_ref = state.read_str("base_ref");

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

        // The stub keeps the resolved path so `definition.path` still names
        // the definition the run used, even though its stages are not loaded.
        let mut definition = GremlinDefinition::stub();
        if let Some(path) = &definition_path {
            definition.path = path.clone();
        }

        Ok(Gremlin {
            id: gremlin_id,
            state_dir,
            artifact_dir: artifact_dir.clone(),
            definition_path,
            client_override: None,
            definition,
            registry: ArtifactRegistry::new(artifact_dir),
            worktree,
            worktree_parent: None,
            project_root,
            base_ref_sha,
            base_ref,
            resume_from: None,
            state,
            env: HashMap::new(),
            client: Client::parse("cmd:true").expect("'cmd:true' is always a valid client spec"),
            loop_stack: Vec::new(),
            stage_inputs,
        })
    }

    /// Load the definition, build the registry, create the client, and resolve the
    /// environment — everything a handle needs before it can run a stage.
    ///
    /// Deferred out of the constructors so that a handle which is only read (or
    /// only cleaned up) never pays for it. Idempotent: a run loop that is
    /// re-entered, or a forked child whose parent already did the work, finds a
    /// loaded definition and returns immediately.
    ///
    /// The stub definition is the "not initialized yet" marker, so every
    /// fallible step runs into locals first and `self` is only mutated once
    /// they have all succeeded. An initialization that fails partway leaves the
    /// handle a stub, and a retry — `resume` re-entering the run loop after a
    /// transient bootstrap failure — starts the whole sequence over instead of
    /// finding a half-built runtime it believes is finished.
    pub(crate) fn init_runtime(&mut self) -> Result<(), RunError> {
        if !self.definition.is_stub() {
            return Ok(());
        }
        let Some(definition_path) = self.definition_path.clone() else {
            return Err(RunError::Message(format!(
                "gremlin {}: no definition path to load",
                self.id
            )));
        };

        let definition =
            GremlinDefinition::from_yaml(&definition_path, self.client_override.as_deref())
                .map_err(|error| RunError::Message(error.to_string()))?;

        // An unusable client must not abort a run: the state directory, the
        // worktree and the artifacts all have to exist before any stage can
        // run, and failing here would leave the operator with no run at all to
        // debug. `cmd:true` is the harness's own no-op client — it is what
        // `config::inject_sentinals` installs so a missing client never fails
        // structural validation — so a bad spec degrades to a run that does
        // nothing rather than one that never starts, while the definition's
        // declared `default_client` stays on record in `state.json`.
        let client = Client::parse(&definition.default_client).unwrap_or_else(|_| {
            Client::parse("cmd:true").expect("'cmd:true' is always a valid client spec")
        });

        let base_ref = if self.base_ref.is_empty() {
            definition.base_ref.clone()
        } else {
            self.base_ref.clone()
        };

        let registry = ArtifactRegistry::new(self.artifact_dir.clone());
        register_stage_inputs(&registry, &definition.bootstrap, &self.stage_inputs);
        register_base_sha(
            &registry,
            self.worktree.as_deref().unwrap_or(&self.project_root),
        );

        let overlay_dir = self.state_dir.join(config::overlay_dirname());
        let env = resolve_env(
            bootstrap_script(&definition.bootstrap),
            &self.artifact_dir,
            &self.state_dir,
            self.id.as_str(),
            &self.project_root,
            self.worktree.as_deref(),
            &overlay_dir,
        )?;

        // Nothing below this line can fail, so this is the commit point.
        let default_client = definition.default_client.clone();
        self.definition = definition;
        self.client = client;
        self.base_ref = base_ref;
        self.registry = registry;

        // The Python launcher sources bootstrap.env before the worktree
        // exists, so GREMLINS_WORKTREE_PATH (and any variable the script
        // derives from it) is absent. resolve_env produces the correct map now
        // that the worktree is real — write it back to the process so agent
        // stages and their child processes inherit the correct PATH,
        // VIRTUAL_ENV, etc.
        for (key, value) in &env {
            std::env::set_var(key, value);
        }
        self.env = env;

        // The client label is only knowable once the definition is loaded; the
        // initial write left it blank and this is the patch that fills it in.
        let mut fields = Map::new();
        fields.insert("client".to_string(), Value::String(default_client));
        self.state.patch(&[], &fields);

        Ok(())
    }

    /// Fork a child gremlin: copy artifacts, branch a worktree at the parent's
    /// HEAD, and seed a fresh `state.json` carrying the child's identity.
    ///
    /// `child_definition_path` is the branch's hermetic `definition.yaml`; pass
    /// `None` to inherit the parent's persisted `definition_path`.
    pub fn fork(
        &self,
        child_id: &str,
        parent_id: &str,
        group_name: &str,
        child_key: &str,
        child_definition_path: Option<&Path>,
    ) -> Result<Gremlin, RunError> {
        let definition = match child_definition_path {
            Some(path) => {
                GremlinDefinition::from_yaml(path, None).unwrap_or_else(|_| self.definition.clone())
            }
            None => self.definition.clone(),
        };
        self.fork_child(
            child_id,
            parent_id,
            group_name,
            child_key,
            child_definition_path,
            definition,
        )
    }

    /// Fork a child gremlin that runs only the given `stages`.
    ///
    /// Like [`Gremlin::fork`], but instead of loading a child definition from
    /// disk, the child inherits the parent's definition metadata and runs only
    /// the provided stage list. Used by the parallel executor so each child
    /// runs exactly one stage without needing a separate definition file.
    pub fn fork_with_stages(
        &self,
        child_id: &str,
        parent_id: &str,
        group_name: &str,
        child_key: &str,
        stages: Vec<RunnableStage>,
    ) -> Result<Gremlin, RunError> {
        log::debug!(
            "fork_with_stages: child_id={child_id}, parent_id={parent_id}, group_name={group_name}, child_key={child_key}, stage_count={}",
            stages.len()
        );
        let mut definition = self.definition.clone_with_stages(stages);
        // launch_cmds and cli_out belong to the parent's initial launch;
        // children inherit the artifacts via the registry copy in `fork_child`.
        // cmds (worktree setup) still runs — each child has its own worktree.
        definition.bootstrap.launch_cmds.clear();
        definition.bootstrap.cli_out.clear();
        self.fork_child(child_id, parent_id, group_name, child_key, None, definition)
    }

    /// The shared body of [`Gremlin::fork`] and [`Gremlin::fork_with_stages`].
    ///
    /// Owns the whole fork sequence — id validation, child directory
    /// derivation and creation, artifact copy, worktree branching, registry
    /// rebuild, child-state seeding, and the state/log write — for a child
    /// whose `definition` the caller has already resolved. `child_definition_path`
    /// is the branch's hermetic `definition.yaml`, recorded in the child state
    /// when present and otherwise inherited from the parent's persisted path.
    fn fork_child(
        &self,
        child_id: &str,
        parent_id: &str,
        group_name: &str,
        child_key: &str,
        child_definition_path: Option<&Path>,
        definition: GremlinDefinition,
    ) -> Result<Gremlin, RunError> {
        log::debug!(
            "fork: child_id={child_id}, parent_id={parent_id}, group_name={group_name}, child_key={child_key}"
        );
        let child_gremlin_id = validate_gremlin_id(child_id).map_err(RunError::Message)?;

        let child_state_dir = self
            .state_dir
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(child_gremlin_id.as_str());
        let child_artifact_dir = child_state_dir.join("artifacts");
        std::fs::create_dir_all(&child_state_dir)?;
        std::fs::create_dir_all(&child_artifact_dir)?;

        log::debug!(
            "fork: child_state_dir={}, child_artifact_dir={}",
            child_state_dir.display(),
            child_artifact_dir.display()
        );

        copy_tree(&self.artifact_dir, &child_artifact_dir)?;
        log::debug!("fork: copied parent artifacts to child artifact dir");

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
            log::debug!(
                "fork: created worktree for child at {}",
                child_worktree.as_ref().unwrap().display()
            );
        } else {
            log::debug!("fork: no parent worktree — child inherits no worktree");
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
            "definition_path".to_string(),
            Value::String(match child_definition_path {
                Some(path) => path.to_string_lossy().into_owned(),
                None => str_field(&parent, "definition_path"),
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

        log::debug!(
            "fork: child {child_id} ready (provider={}, model={} — shares client with parent {parent_id})",
            self.client.provider(),
            self.client.model(),
        );

        Ok(Gremlin {
            id: child_gremlin_id,
            state_dir: child_state_dir,
            artifact_dir: child_artifact_dir,
            definition_path: child_definition_path
                .map(Path::to_path_buf)
                .or_else(|| self.definition_path.clone()),
            client_override: self.client_override.clone(),
            definition,
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

    /// Remove every filesystem asset this gremlin owns: its worktree, its
    /// scratch directory, and — when `remove_state_dir` is set — its state
    /// directory.
    ///
    /// Best-effort and never-raising: a removal that fails is logged at `warn`
    /// and the rest of the cleanup proceeds. Consuming `self` is the point —
    /// the handle points at paths that no longer exist, so any use after a
    /// `clean` is a compile error rather than a silent read of a dead run.
    ///
    /// Two ordering rules carry the meaning. The `closed` marker is touched
    /// *first*, so fleet viewers (`liveness_of_state_file`) see the gremlin as
    /// closed even if filesystem removal fails part-way. The state directory
    /// is removed *last*, so a partial cleanup never strands a worktree or
    /// scratch directory that nothing can trace back to a gremlin: if
    /// `state.json` is gone, every other asset is already gone.
    pub fn clean(self, remove_state_dir: bool) {
        // Mark closed before touching anything: a run that vanished without a
        // marker reads as a crash, not an intentional cleanup.
        let closed = self.state_dir.join("closed");
        if let Err(error) = std::fs::write(&closed, "") {
            log::warn!("clean: could not touch {}: {error}", closed.display());
        }

        self.clean_worktree();
        self.clean_scratch();

        if remove_state_dir {
            if let Err(error) = std::fs::remove_dir_all(&self.state_dir) {
                log::warn!(
                    "clean: could not remove state dir {}: {error}",
                    self.state_dir.display()
                );
            }
        }
    }

    /// Remove the worktree, best-effort.
    ///
    /// `git worktree remove` is the clean path — it drops the administrative
    /// record under `.git/worktrees` as well as the checkout. It is run
    /// whenever a `project_root` is available, *even if the checkout is already
    /// gone*: the administrative record outlives the directory, and skipping
    /// git would leave stale metadata behind forever. When the directory is
    /// still present afterwards (or there is no `project_root` to run against)
    /// an `rmtree` fallback finishes the job.
    fn clean_worktree(&self) {
        let Some(worktree) = &self.worktree else {
            return;
        };
        if !self.project_root.as_os_str().is_empty() {
            git::remove_worktree(&self.project_root, &worktree.to_string_lossy());
        }
        if worktree.exists() {
            if let Err(error) = std::fs::remove_dir_all(worktree) {
                log::warn!(
                    "clean: could not remove worktree {}: {error}",
                    worktree.display()
                );
            }
        }
    }

    /// Remove the scratch directory — the parent of `artifact_dir` — best-effort.
    fn clean_scratch(&self) {
        let scratch = config::scratch_root(Some(self.id.as_str()));
        if !scratch.is_dir() {
            return;
        }
        if let Err(error) = std::fs::remove_dir_all(&scratch) {
            log::warn!(
                "clean: could not remove scratch dir {}: {error}",
                scratch.display()
            );
        }
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

/// The worktree of `definition_path`'s project.
///
/// Mirrors the private `project_root_for` in `schemas::gremlin_definition`: the parent of
/// the nearest ancestor `.gremlins` directory, else the definition's own parent.
/// Falls back to `GREMLINS_PROJECT_ROOT` when the env var is set, so a running
/// gremlin whose state dir has a staged overlay still resolves the real project.
fn project_root_for(definition_path: &Path) -> PathBuf {
    if let Ok(env_root) = std::env::var("GREMLINS_PROJECT_ROOT") {
        let path = PathBuf::from(&env_root);
        if path.is_dir() {
            return path;
        }
    }
    let canonical = definition_path
        .canonicalize()
        .unwrap_or_else(|_| definition_path.to_path_buf());
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

/// The fallible remainder of [`Gremlin::create`], after the worktree exists.
///
/// Writes the initial `state.json`, stages the overlay, and returns a handle
/// whose runtime fields are stubs. Split out so the caller can unregister the
/// worktree if any of it fails.
///
/// The `client` field is written blank: the label comes from the definition's
/// `default_client`, which is only known once [`Gremlin::init_runtime`] has
/// loaded the YAML, and that is the patch which fills it in.
#[allow(clippy::too_many_arguments)]
fn write_launch_state(
    gremlin_id: GremlinId,
    state_dir: &Path,
    artifact_dir: &Path,
    definition_path: &Path,
    client_override: Option<&str>,
    project_root: &Path,
    worktree: Option<PathBuf>,
    worktree_parent: Option<&Path>,
    resume_from: Option<&str>,
    stage_inputs: &HashMap<String, String>,
    base_ref_sha: String,
    base_ref: &str,
) -> Result<Gremlin, RunError> {
    let workdir = worktree
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    let definition_path_str = definition_path.to_string_lossy().into_owned();

    let mut state = StateData::new(Some(gremlin_id.as_str().to_string()));
    let inputs: Map<String, Value> = stage_inputs
        .iter()
        .map(|(key, value)| (key.clone(), Value::String(value.clone())))
        .collect();

    // Merge with existing state.json if present (the Python launcher may have
    // already written initial state before spawning the subprocess).
    let existing = state::read_state_json(Some(&state_dir.join("state.json")));
    let mut initial = existing;
    // Always set these fields from the Rust launch context.
    initial.insert("id".to_string(), Value::String(gremlin_id.to_string()));
    if !initial.contains_key("kind") {
        initial.insert("kind".to_string(), Value::String(String::new()));
    }
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
    if !base_ref.is_empty() {
        initial.insert("base_ref".to_string(), Value::String(base_ref.to_string()));
    }
    initial.insert("status".to_string(), Value::String("running".to_string()));
    if !initial.contains_key("started_at") {
        initial.insert("started_at".to_string(), Value::String(state::now_stamp()));
    }
    if !initial.contains_key("description") {
        initial.insert("description".to_string(), Value::String(String::new()));
    }
    if !initial.contains_key("parent_id") {
        initial.insert("parent_id".to_string(), Value::String(String::new()));
    }
    if !initial.contains_key("definition_args") {
        initial.insert("definition_args".to_string(), Value::Array(Vec::new()));
    }
    // Left blank until `init_runtime` loads the definition and patches it.
    initial.insert("client".to_string(), Value::String(String::new()));
    initial.insert(
        "definition_path".to_string(),
        Value::String(definition_path_str.clone()),
    );
    initial.insert("stage".to_string(), Value::String("starting".to_string()));
    // The process running the definition is the one a later `gremlins stop`
    // must signal. Record our own pid rather than clobbering the launcher's
    // value with null (which left live gremlins unstoppable).
    initial.insert("pid".to_string(), Value::from(std::process::id() as i64));
    initial.insert("stage_inputs".to_string(), Value::Object(inputs));
    if !initial.contains_key("attempt") {
        initial.insert("attempt".to_string(), Value::String(String::new()));
    }
    if !initial.contains_key("group_name") {
        initial.insert("group_name".to_string(), Value::String(String::new()));
    }
    if !initial.contains_key("child_key") {
        initial.insert("child_key".to_string(), Value::String(String::new()));
    }
    initial.insert("exit_code".to_string(), Value::Null);
    if !initial.contains_key("metadata") {
        initial.insert("metadata".to_string(), Value::Object(Map::new()));
    }
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

    // The stub is not empty: it carries the resolved definition path so that a
    // pre-init handle reports the same `definition.path` as one built by
    // [`Gremlin::from`]. `init_runtime` replaces the stub wholesale.
    let definition = GremlinDefinition {
        path: definition_path.to_path_buf(),
        ..GremlinDefinition::stub()
    };

    Ok(Gremlin {
        id: gremlin_id,
        state_dir: state_dir.to_path_buf(),
        artifact_dir: artifact_dir.to_path_buf(),
        definition_path: Some(definition_path.to_path_buf()),
        client_override: client_override.map(String::from),
        definition,
        registry: ArtifactRegistry::new(artifact_dir.to_path_buf()),
        worktree,
        worktree_parent: worktree_parent.map(Path::to_path_buf),
        project_root: project_root.to_path_buf(),
        base_ref_sha,
        base_ref: base_ref.to_string(),
        resume_from: resume_from.map(String::from),
        state,
        env: HashMap::new(),
        client: Client::parse("cmd:true").expect("'cmd:true' is always a valid client spec"),
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

/// Resolve a definition kind against `project_root`, ignoring the process-wide
/// overlay override.
///
/// Discovery threads the project root through its `base_dir` argument, but it
/// would otherwise read `GREMLINS_OVERLAY_DIR` from the process environment,
/// and a running gremlin exports that variable as its own overlay path. Naming
/// the project's own overlay explicitly is what lets `status` look inside the
/// project the state file names rather than the overlay of whoever asked —
/// without the process-global env ever being cleared, and so without a lock.
fn resolve_definition_in_project(kind: &str, project_root: &Path) -> Option<PathBuf> {
    let overlay = config::overlay_dir_without_env(project_root);
    discovery::resolve_definition_path_in(kind, Some(&overlay), project_root.to_path_buf()).ok()
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

/// The eight harness-owned system variables, built from a gremlin's paths.
///
/// These are both *seeded into* the base the bootstrap script is sourced
/// against and *re-asserted* on top of the result, so the script can read
/// them but can never override them.
pub fn system_env(
    artifact_dir: &Path,
    state_dir: &Path,
    gremlin_id: &str,
    project_root: &Path,
    worktree: Option<&Path>,
    overlay_dir: &Path,
) -> HashMap<String, String> {
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

    let mut vars = HashMap::new();
    vars.insert("GREMLINS_GREMLIN_ID".to_string(), gremlin_id.to_string());
    vars.insert(
        "GREMLINS_PROJECT_ROOT".to_string(),
        project_root.to_string_lossy().into_owned(),
    );
    vars.insert(
        "GREMLINS_OVERLAY_DIR".to_string(),
        overlay_dir.to_string_lossy().into_owned(),
    );
    vars.insert("GREMLINS_WORKTREE_PATH".to_string(), worktree_path);
    vars.insert(
        "GREMLINS_ARTIFACT_DIR".to_string(),
        artifact_dir.to_string_lossy().into_owned(),
    );
    vars.insert("GREMLIN_WORKSPACE_DIR".to_string(), workspace_dir);
    vars.insert(
        "GREMLIN_STATE_DIR".to_string(),
        state_dir.to_string_lossy().into_owned(),
    );
    vars.insert(
        "GREMLINS_SCRATCH_DIR".to_string(),
        scratch_dir.to_string_lossy().into_owned(),
    );
    vars
}

/// Resolve the process environment a gremlin's stages and bootstrap inherit.
///
/// The system variables are seeded into the base *before* the bootstrap script
/// is sourced, so the script can read them (e.g. to point `VIRTUAL_ENV` at the
/// worktree's venv), then re-asserted on top afterwards. The script can shape
/// the environment but can never redirect the harness's paths.
pub fn resolve_env(
    bootstrap_env: Option<&str>,
    artifact_dir: &Path,
    state_dir: &Path,
    gremlin_id: &str,
    project_root: &Path,
    worktree: Option<&Path>,
    overlay_dir: &Path,
) -> Result<HashMap<String, String>, RunError> {
    let system = system_env(
        artifact_dir,
        state_dir,
        gremlin_id,
        project_root,
        worktree,
        overlay_dir,
    );

    let mut base: HashMap<String, String> = std::env::vars().collect();
    base.extend(system.clone());

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

    // Re-assert the system variables on top of whatever the script produced.
    env.extend(system);
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
    fn resolve_env_lets_the_script_read_system_vars() {
        let dir = tempfile::tempdir().unwrap();
        let artifact_dir = dir.path().join("scratch").join("gr-test").join("artifacts");
        let state_dir = dir.path().join("state").join("gr-test");
        let project_root = dir.path().join("project");
        let worktree = dir.path().join("wt");
        let overlay_dir = state_dir.join(config::overlay_dirname());
        std::fs::create_dir_all(&artifact_dir).unwrap();
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::create_dir_all(&overlay_dir).unwrap();

        // The script derives VIRTUAL_ENV from the worktree path — the shape the
        // bundled definitions use — then tries to redirect a system variable.
        let script = "export VIRTUAL_ENV=\"${GREMLINS_WORKTREE_PATH}/.venv\"\n\
                       export GREMLINS_WORKTREE_PATH=/hijacked\n";
        let env = resolve_env(
            Some(script),
            &artifact_dir,
            &state_dir,
            "gr-test",
            &project_root,
            Some(&worktree),
            &overlay_dir,
        )
        .unwrap();

        assert_eq!(env["VIRTUAL_ENV"], format!("{}/.venv", worktree.display()));
        // The script can read the system variables but cannot override them.
        assert_eq!(env["GREMLINS_WORKTREE_PATH"], worktree.to_string_lossy());
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

    /// A repository with one commit and a `.gremlins/demo.yaml` gremlin definition.
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
    fn create_creates_state_and_worktree() {
        if !git_available() {
            eprintln!("git is unavailable; skipping create_creates_state_and_worktree");
            return;
        }
        with_sandbox(None, |sandbox| {
            let repo = tempfile::tempdir().unwrap();
            if !init_repo(repo.path()) {
                eprintln!("could not prepare a git fixture; skipping");
                return;
            }
            let definition_path = repo.path().join(".gremlins").join("demo.yaml");

            let mut gremlin = Gremlin::create(
                "gr-test",
                &definition_path,
                None,
                None,
                None,
                &HashMap::new(),
                false,
                None,
                None,
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
            assert_eq!(raw["pid"].as_i64().unwrap(), std::process::id() as i64);
            assert!(raw["definition_path"]
                .as_str()
                .unwrap()
                .ends_with("demo.yaml"));
            // Construction is cheap: the client label is blank until the run
            // loads the definition, and nothing has been parsed yet.
            assert_eq!(raw["client"], "");
            assert!(gremlin.definition.is_stub());
            assert!(gremlin.env.is_empty());

            // The checkout and the commit it was branched from are on record,
            // and the returned handle agrees with what was persisted.
            assert_eq!(raw["workdir"].as_str().unwrap(), worktree.to_string_lossy());
            let base = raw["worktree_base"].as_str().unwrap();
            assert_eq!(base.len(), 40, "worktree_base should be a SHA: {base:?}");
            assert_eq!(base, gremlin.base_ref_sha);

            // Lazy init is what fills in the runtime fields.
            gremlin.init_runtime().unwrap();
            assert_eq!(gremlin.definition.name, "demo");
            assert_eq!(gremlin.env["GREMLINS_GREMLIN_ID"], "gr-test");
            assert!(gremlin.registry.artifact_dir.ends_with("artifacts"));
            let raw = read_state(&state_file);
            assert_eq!(raw["client"], "cmd:true");

            // Idempotent: a second call is a no-op, not a re-parse.
            let path_before = gremlin.definition.path.clone();
            gremlin.init_runtime().unwrap();
            assert_eq!(gremlin.definition.path, path_before);
        });
    }

    #[test]
    fn create_then_from_roundtrips() {
        if !git_available() {
            eprintln!("git is unavailable; skipping create_then_from_roundtrips");
            return;
        }
        with_sandbox(None, |_sandbox| {
            let repo = tempfile::tempdir().unwrap();
            if !init_repo(repo.path()) {
                eprintln!("could not prepare a git fixture; skipping");
                return;
            }
            let definition_path = repo.path().join(".gremlins").join("demo.yaml");

            let mut launched = Gremlin::create(
                "gr-test",
                &definition_path,
                None,
                None,
                None,
                &HashMap::new(),
                false,
                None,
                None,
                None,
            )
            .unwrap();
            let mut opened = Gremlin::from("gr-test").unwrap();

            assert_eq!(opened.id, launched.id);
            assert_eq!(opened.state_dir, launched.state_dir);
            assert_eq!(opened.project_root, launched.project_root);
            assert_eq!(opened.worktree, launched.worktree);
            assert_eq!(opened.definition_path, launched.definition_path);

            launched.init_runtime().unwrap();
            opened.init_runtime().unwrap();
            assert_eq!(opened.definition.name, launched.definition.name);
            assert_eq!(opened.env["GREMLINS_GREMLIN_ID"], "gr-test");
        });
    }

    #[test]
    fn from_then_clean_needs_no_run() {
        if !git_available() {
            eprintln!("git is unavailable; skipping from_then_clean_needs_no_run");
            return;
        }
        with_sandbox(None, |sandbox| {
            let repo = tempfile::tempdir().unwrap();
            if !init_repo(repo.path()) {
                eprintln!("could not prepare a git fixture; skipping");
                return;
            }
            let definition_path = repo.path().join(".gremlins").join("demo.yaml");

            let created = Gremlin::create(
                "gr-test",
                &definition_path,
                None,
                None,
                None,
                &HashMap::new(),
                false,
                None,
                None,
                None,
            )
            .unwrap();
            let worktree = created.worktree.clone().unwrap();
            drop(created);

            // A handle reconstructed purely for cleanup: no `run`, so the
            // definition was never loaded, and `clean` must not need it.
            let handle = Gremlin::from("gr-test").unwrap();
            assert!(handle.definition.is_stub());
            handle.clean(true);

            assert!(!sandbox.join("state").join("gr-test").exists());
            assert!(!worktree.exists(), "worktree should be gone");
            assert!(!sandbox.join("scratch").join("gr-test").exists());
        });
    }

    #[tokio::test]
    // The env guard holds a plain mutex across the run's awaits; safe because
    // `#[tokio::test]` drives a current-thread runtime, so no other task can be
    // scheduled on this thread while the lock is held.
    #[allow(clippy::await_holding_lock)]
    async fn from_then_run_triggers_lazy_init() {
        if !git_available() {
            eprintln!("git is unavailable; skipping from_then_run_triggers_lazy_init");
            return;
        }
        let mut env = EnvGuard::lock();
        let sandbox = tempfile::tempdir().unwrap();
        env.set("GREMLINS_SANDBOX_ROOT", sandbox.path());

        let repo = tempfile::tempdir().unwrap();
        if !init_repo(repo.path()) {
            eprintln!("could not prepare a git fixture; skipping");
            return;
        }
        let definition_path = repo.path().join(".gremlins").join("demo.yaml");

        Gremlin::create(
            "gr-test",
            &definition_path,
            None,
            None,
            None,
            &HashMap::new(),
            false,
            None,
            None,
            None,
        )
        .unwrap();

        let mut handle = Gremlin::from("gr-test").unwrap();
        assert!(handle.definition.is_stub());

        // The resume path: reconstruct cheaply, then drive the run. The
        // definition must have been loaded by the time `run` returns.
        assert_eq!(handle.run().await.unwrap(), 0);
        assert_eq!(handle.definition.name, "demo");
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
            let definition_path = repo.path().join(".gremlins").join("demo.yaml");

            let parent = Gremlin::create(
                "gr-test",
                &definition_path,
                None,
                None,
                None,
                &HashMap::new(),
                false,
                None,
                None,
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
            let definition_path = repo.path().join(".gremlins").join("demo.yaml");

            let parent = Gremlin::create(
                "gr-test",
                &definition_path,
                None,
                None,
                None,
                &HashMap::new(),
                false,
                None,
                None,
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
    fn resolve_definition_in_project_ignores_the_overlay_override() {
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
        // find the definition inside the project the state file names.
        env.set("GREMLINS_OVERLAY_DIR", "/nonexistent/overlay");
        let found = resolve_definition_in_project("demo", project.path());
        let missing = resolve_definition_in_project("nope", project.path());

        assert!(found.is_some_and(|path| path.ends_with("demo.yaml")));
        assert!(missing.is_none());
    }

    // --- cleanup ---

    /// A hand-built gremlin whose paths point wherever the test wants.
    ///
    /// `clean` only reads `id`, `state_dir`, `artifact_dir`, `worktree` and
    /// `project_root`, so a full `launch` would just be git fixture noise for
    /// the cases that are not about launch at all.
    fn test_gremlin(
        id: &str,
        state_dir: PathBuf,
        artifact_dir: PathBuf,
        worktree: Option<PathBuf>,
        project_root: PathBuf,
    ) -> Gremlin {
        Gremlin {
            id: validate_gremlin_id(id).unwrap(),
            state_dir,
            registry: ArtifactRegistry::new(artifact_dir.clone()),
            artifact_dir,
            definition_path: None,
            client_override: None,
            definition: GremlinDefinition::stub(),
            worktree,
            worktree_parent: None,
            project_root,
            base_ref_sha: String::new(),
            base_ref: String::new(),
            resume_from: None,
            state: StateData::new(Some(id.to_string())),
            env: HashMap::new(),
            client: Client::parse("cmd:true").unwrap(),
            loop_stack: Vec::new(),
            stage_inputs: HashMap::new(),
        }
    }

    #[test]
    fn clean_true_removes_everything() {
        with_sandbox(None, |sandbox| {
            let id = "gr-clean";
            let state_dir = sandbox.join("state").join(id);
            let scratch_dir = sandbox.join("scratch").join(id);
            let artifact_dir = scratch_dir.join("artifacts");
            let worktree = sandbox.join("worktree");
            std::fs::create_dir_all(&artifact_dir).unwrap();
            std::fs::create_dir_all(&state_dir).unwrap();
            std::fs::write(state_dir.join("state.json"), "{}").unwrap();
            std::fs::create_dir_all(&worktree).unwrap();
            std::fs::write(worktree.join("file"), "data").unwrap();

            test_gremlin(
                id,
                state_dir.clone(),
                artifact_dir.clone(),
                Some(worktree.clone()),
                PathBuf::new(),
            )
            .clean(true);

            assert!(!state_dir.exists(), "state dir should be gone");
            assert!(!scratch_dir.exists(), "scratch dir should be gone");
            assert!(!worktree.exists(), "worktree should be gone");
        });
    }

    #[test]
    fn clean_false_leaves_state_dir_with_closed_marker() {
        with_sandbox(None, |sandbox| {
            let id = "gr-clean-keep";
            let state_dir = sandbox.join("state").join(id);
            let scratch_dir = sandbox.join("scratch").join(id);
            let artifact_dir = scratch_dir.join("artifacts");
            let worktree = sandbox.join("worktree-keep");
            std::fs::create_dir_all(&artifact_dir).unwrap();
            std::fs::create_dir_all(&state_dir).unwrap();
            std::fs::write(state_dir.join("state.json"), "{}").unwrap();
            std::fs::create_dir_all(&worktree).unwrap();

            test_gremlin(
                id,
                state_dir.clone(),
                artifact_dir.clone(),
                Some(worktree.clone()),
                PathBuf::new(),
            )
            .clean(false);

            assert!(!worktree.exists(), "worktree should be gone");
            assert!(!scratch_dir.exists(), "scratch dir should be gone");
            assert!(state_dir.is_dir(), "state dir should remain");
            assert!(
                state_dir.join("closed").is_file(),
                "closed marker should be present"
            );
        });
    }

    #[test]
    fn clean_succeeds_without_a_worktree() {
        with_sandbox(None, |sandbox| {
            let id = "gr-clean-nowt";
            let state_dir = sandbox.join("state").join(id);
            let artifact_dir = sandbox.join("scratch").join(id).join("artifacts");
            std::fs::create_dir_all(&state_dir).unwrap();
            std::fs::create_dir_all(&artifact_dir).unwrap();

            test_gremlin(id, state_dir.clone(), artifact_dir, None, PathBuf::new()).clean(true);

            assert!(!state_dir.exists());
            assert!(!sandbox.join("scratch").join(id).exists());
        });
    }

    #[test]
    fn clean_falls_back_to_rmtree_outside_a_git_repo() {
        with_sandbox(None, |sandbox| {
            let id = "gr-clean-nogit";
            let state_dir = sandbox.join("state").join(id);
            let artifact_dir = sandbox.join("scratch").join(id).join("artifacts");
            std::fs::create_dir_all(&state_dir).unwrap();
            std::fs::create_dir_all(&artifact_dir).unwrap();

            // A worktree with a `project_root` that is not a repository: the
            // `git worktree remove` fails silently and the rmtree fallback is
            // what actually deletes the checkout.
            let worktree = sandbox.join("worktree-nogit");
            std::fs::create_dir_all(&worktree).unwrap();
            std::fs::write(worktree.join("file"), "data").unwrap();
            let project_root = sandbox.join("not-a-repo");
            std::fs::create_dir_all(&project_root).unwrap();

            test_gremlin(
                id,
                state_dir,
                artifact_dir,
                Some(worktree.clone()),
                project_root,
            )
            .clean(true);

            assert!(!worktree.exists(), "rmtree fallback should remove it");
        });
    }

    /// How many administrative worktree records git keeps under
    /// `.git/worktrees` — counted rather than path-matched, because git
    /// resolves a tempdir to its `/private` form and the test's own path does
    /// not.
    fn admin_worktree_entries(root: &Path) -> usize {
        std::fs::read_dir(root.join(".git").join("worktrees"))
            .map(|entries| entries.count())
            .unwrap_or(0)
    }

    #[test]
    fn clean_prunes_metadata_for_a_vanished_worktree() {
        if !git_available() {
            eprintln!("git is unavailable; skipping clean_prunes_metadata_for_a_vanished_worktree");
            return;
        }
        with_sandbox(None, |sandbox| {
            let repo = tempfile::tempdir().unwrap();
            if !init_repo(repo.path()) {
                eprintln!("could not prepare a git fixture; skipping");
                return;
            }
            let id = "gr-clean-stale";
            let state_dir = sandbox.join("state").join(id);
            let artifact_dir = sandbox.join("scratch").join(id).join("artifacts");
            std::fs::create_dir_all(&state_dir).unwrap();
            std::fs::create_dir_all(&artifact_dir).unwrap();

            // A registered worktree whose checkout is deleted out from under
            // git: the directory is gone, the admin record is not. The `clean`
            // path has to reach git *despite* the missing directory, or the
            // record outlives every gremlin that could explain it.
            let worktree = repo.path().join("stale-worktree");
            let worktree_str = worktree.to_string_lossy().into_owned();
            assert!(
                git(repo.path(), &["worktree", "add", "--detach", &worktree_str])
                    .status
                    .success(),
                "worktree add should succeed"
            );
            assert_eq!(admin_worktree_entries(repo.path()), 1);
            std::fs::remove_dir_all(&worktree).unwrap();
            assert!(!worktree.exists(), "checkout should be gone");
            assert_eq!(
                admin_worktree_entries(repo.path()),
                1,
                "git should still hold the stale record"
            );

            test_gremlin(
                id,
                state_dir,
                artifact_dir,
                Some(worktree.clone()),
                repo.path().to_path_buf(),
            )
            .clean(true);

            assert_eq!(
                admin_worktree_entries(repo.path()),
                0,
                "clean should prune git's administrative record"
            );
        });
    }
}
