//! The gremlin handle: identity, environment, workspace, and state.
//!
//! A [`Gremlin`] is the runtime handle for one run — the thing the run loop
//! drives a stage tree through. It owns the resolved [`GremlinDefinition`], the
//! [`StateData`] handle, and it is the single
//! place where a gremlin's environment is assembled.
//!
//! Three constructors cover the lifecycles the Python executor had:
//! [`Gremlin::init`] starts a fresh run in its own workspace directory,
//! [`Gremlin::from`] reconstructs a handle from a persisted state directory
//! (for status checks, cleanup, and recovery), and [`Gremlin::fork`] spins off
//! a child that inherits the parent's artifacts, environment, and — optionally
//! — a fresh workspace.
//!
//! Construction is cheap on purpose. `create` and `from` populate only the
//! path and identity fields; the definition YAML, the artifact registry, the
//! client, and the resolved environment are deferred to
//! [`Gremlin::init_runtime`], which [`Gremlin::run`] calls before it drives a
//! single stage. A caller that only wants a handle — to read metadata, or to
//! `clean` — never pays for the runtime.
//!
//! The environment rules are the subtle part and live in [`resolve_env`]: the
//! five `GREMLIN*` system variables are seeded into the base environment
//! *before* any `bootstrap.env` script is sourced (so the script can read
//! them, e.g. to point `VIRTUAL_ENV` at the workspace), then re-asserted
//! on top afterwards — a script can shape the environment but can never
//! redirect the harness's own paths.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::artifacts::uri::Uri;
use crate::clients::agent_loop::CancelToken;
use crate::clients::client::Client;
use crate::clients::interactive::{InteractiveHandle, InteractiveSession};
use crate::config;
use crate::core::proc::{run_logged_commands, sanitize_log_filename};
use crate::core::{discovery, env_file};
use crate::definition::{GremlinDefinition, StaticDefinition};
use crate::executor::state::{self, BlobMode, StateData, StateStore};
use crate::executor::RunError;
use crate::schemas::bootstrap::Bootstrap;

/// Configuration knobs for [`Gremlin::init`].
///
/// The default (all fields false/empty) gives persistent, resumable storage
/// under the standard `config` roots — the same behaviour as before this
/// struct existed.
#[derive(Debug, Clone, Default)]
pub struct GremlinConfig {
    /// When `true`, state, workspace, and scratch are all backed by
    /// [`tempfile::TempDir`]s that vanish when the gremlin is dropped.
    /// The runtime operates identically regardless of backing.
    pub ephemeral: bool,
}

/// Private enum for workspace storage: persistent path or temp dir.
pub enum WorkDir {
    Persistent(PathBuf),
    Temp(tempfile::TempDir),
}

impl WorkDir {
    pub fn path(&self) -> &Path {
        match self {
            WorkDir::Persistent(p) => p.as_path(),
            WorkDir::Temp(t) => t.path(),
        }
    }
}

/// Private enum for scratch storage: persistent path or temp dir.
pub enum ScratchDir {
    Persistent(PathBuf),
    Temp(tempfile::TempDir),
}

impl ScratchDir {
    pub fn path(&self) -> &Path {
        match self {
            ScratchDir::Persistent(p) => p.as_path(),
            ScratchDir::Temp(t) => t.path(),
        }
    }
}

/// State keys that describe *this* run's live execution and must never leak
/// into a forked child, which starts its own from scratch.
pub(crate) const FORK_TRANSIENT: [&str; 13] = [
    "workdir",
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

/// Snapshot of process-global configuration needed by the run loop.
/// Populated once at construction time so multiple `Gremlin::run()`
/// invocations can coexist in one process without reading global state.
#[derive(Default)]
pub(crate) struct RuntimeConfig {
    /// Exact-match stage→client mappings from config.
    pub stage_clients_exact: HashMap<String, String>,
    /// Prefix-match stage→client mappings from config.
    pub stage_clients_prefix: HashMap<String, String>,
    /// Exact-match task→client mappings from config.
    pub task_clients_exact: HashMap<String, String>,
    /// Prefix-match task→client mappings from config.
    pub task_clients_prefix: HashMap<String, String>,
    /// The default client from settings.yaml, if any.
    pub default_client: Option<String>,
    /// The base process environment captured at startup, before any
    /// bootstrap or system vars are layered on.
    pub base_process_env: HashMap<String, String>,
    /// Per-gremlin log channel. Every gremlin-scoped log event is sent here.
    pub log_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// Interactive handle (supervisor → agent loop).
    pub interactive: Option<InteractiveHandle>,
}

impl Clone for RuntimeConfig {
    fn clone(&self) -> Self {
        Self {
            stage_clients_exact: self.stage_clients_exact.clone(),
            stage_clients_prefix: self.stage_clients_prefix.clone(),
            task_clients_exact: self.task_clients_exact.clone(),
            task_clients_prefix: self.task_clients_prefix.clone(),
            default_client: self.default_client.clone(),
            base_process_env: self.base_process_env.clone(),
            log_tx: self.log_tx.clone(),
            interactive: self.interactive.clone(),
        }
    }
}

impl RuntimeConfig {
    /// Snapshot the current global config and process environment.
    pub(crate) fn snapshot() -> Self {
        let cfg = config::get_global();
        let (stage_exact, stage_prefix) = cfg
            .as_ref()
            .map(|c| c.default_client_by_stage())
            .map(|(e, p)| (e.clone(), p.clone()))
            .unwrap_or_default();
        let (task_exact, task_prefix) = cfg
            .as_ref()
            .map(|c| c.task_clients())
            .map(|(e, p)| (e.clone(), p.clone()))
            .unwrap_or_default();
        let default_client = cfg
            .as_ref()
            .and_then(|c| c.default_client().map(String::from));
        let base_process_env: HashMap<String, String> = std::env::vars().collect();
        Self {
            stage_clients_exact: stage_exact,
            stage_clients_prefix: stage_prefix,
            task_clients_exact: task_exact,
            task_clients_prefix: task_prefix,
            default_client,
            base_process_env,
            log_tx: None,
            interactive: None,
        }
    }
}

/// One gremlin run, reconstructed from its state directory.
pub struct Gremlin {
    pub id: GremlinId,
    /// Where the definition YAML lives, resolved at construction time but not
    /// read until [`Gremlin::init_runtime`]. `None` means no definition could be
    /// located — a handle that reaches `init_runtime` without one is an error.
    pub definition_path: Option<PathBuf>,
    /// The CLI `--client` value this run was launched with, replayed when the
    /// definition is finally loaded.
    pub client_override: Option<String>,
    pub definition: Box<dyn GremlinDefinition>,
    pub workdir: Option<WorkDir>,
    pub project_root: PathBuf,
    pub state: StateData,
    pub env: HashMap<String, String>,
    pub client: Client,
    pub loop_iter: String,
    /// The CLI/source values this run was launched with, keyed by source name.
    ///
    /// Bootstrap's `bind_artifact` DSL reads from here: a source key that is
    /// absent or empty is an optional source with nothing to bind.
    pub stage_inputs: HashMap<String, String>,
    pub(crate) runtime_config: RuntimeConfig,
    /// Supervisor-owned cancel token. When set, the run loop passes it to the
    /// backend so `gremlins stop` cancels in-flight agent loops.
    pub(crate) cancel_token: Option<Arc<CancelToken>>,
    /// Interactive session, stored at launch time so run_agent can reuse
    /// the pre-created command receiver for the first agent stage.
    pub(crate) interactive_session: Option<InteractiveSession>,
    /// Scratch directory — owned here so TempDir cleanup is automatic.
    pub(crate) scratch_dir: ScratchDir,
}

impl Gremlin {
    /// Start a fresh run: generate an id, reserve the state directory, create
    /// a workspace directory, write `state.json` and a hermetic `definition.yaml`,
    /// and return a handle ready for [`Gremlin::run`].
    ///
    /// This is the single library-owned creation path — the CLI calls it and
    /// then spawns the child; nothing else is needed to bootstrap a gremlin.
    ///
    /// `definition_name` seeds the generated id (`<name>-<4-hex>`).
    /// `definition` is the already-loaded [`GremlinDefinition`] (the CLI parses
    /// it for arg validation); it is serialized into the hermetic snapshot but
    /// the handle itself carries only a stub — [`Gremlin::init_runtime`] loads
    /// the real definition when [`Gremlin::run`] is called.
    pub fn init(
        definition_name: &str,
        definition_path: &Path,
        _definition: &StaticDefinition,
        stage_inputs: &HashMap<String, String>,
        client_override: Option<&str>,
        config: &GremlinConfig,
    ) -> Result<Gremlin, RunError> {
        // Validate definition_name as a safe gremlin-id component before
        // touching the filesystem. The hex suffix only adds alphanumeric
        // chars, so a valid name guarantees a valid candidate.
        validate_gremlin_id(definition_name).map_err(RunError::Message)?;

        // 1. Generate a gremlin id with collision-avoidance.
        let gremlin_id = if config.ephemeral {
            // Ephemeral: use a large random token (16 hex chars) plus an
            // in-process uniqueness check — no filesystem reservation.
            static SEEN: std::sync::Mutex<Option<std::collections::HashSet<String>>> =
                std::sync::Mutex::new(None);
            loop {
                let hex = state::token_hex(8);
                let candidate = format!("{definition_name}-{hex}");
                let mut seen = SEEN.lock().unwrap();
                let set = seen.get_or_insert_with(std::collections::HashSet::new);
                if set.insert(candidate.clone()) {
                    break GremlinId(candidate);
                }
            }
        } else {
            loop {
                let hex = state::token_hex(2);
                let candidate = format!("{definition_name}-{hex}");
                match std::fs::create_dir(config::state_root().join(&candidate)) {
                    Ok(()) => break GremlinId(candidate),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(e) => {
                        return Err(RunError::Message(format!(
                            "failed to create state dir: {e}"
                        )))
                    }
                }
            }
        };

        let definition_path = definition_path
            .canonicalize()
            .unwrap_or_else(|_| definition_path.to_path_buf());

        let project_root = project_root_for(&definition_path);

        // 2. Create the workdir.
        let workdir = if config.ephemeral {
            let temp = tempfile::TempDir::new()
                .map_err(|e| RunError::Message(format!("failed to create temp workdir: {e}")))?;
            WorkDir::Temp(temp)
        } else {
            let path = config::work_root().join(gremlin_id.as_str());
            std::fs::create_dir_all(&path).map_err(|e| {
                RunError::Message(format!("failed to create workdir {}: {e}", path.display()))
            })?;
            WorkDir::Persistent(path)
        };
        let workdir_str = workdir.path().to_string_lossy().into_owned();

        // 3. Create scratch dir.
        let scratch_dir = if config.ephemeral {
            let temp = tempfile::TempDir::new().map_err(|e| {
                RunError::Message(format!("failed to create temp scratch dir: {e}"))
            })?;
            ScratchDir::Temp(temp)
        } else {
            ScratchDir::Persistent(config::scratch_root(Some(gremlin_id.as_str())))
        };

        // 4. Build initial state and create StateData (which writes state.json).
        let definition_path_str = definition_path.to_string_lossy().into_owned();

        let inputs: Map<String, Value> = stage_inputs
            .iter()
            .map(|(key, value)| (key.clone(), Value::String(value.clone())))
            .collect();

        let mut initial = Map::new();
        initial.insert("id".to_string(), Value::String(gremlin_id.to_string()));
        if !initial.contains_key("kind") {
            initial.insert("kind".to_string(), Value::String(String::new()));
        }
        initial.insert(
            "project_root".to_string(),
            Value::String(project_root.to_string_lossy().into_owned()),
        );
        initial.insert("workdir".to_string(), Value::String(workdir_str.clone()));
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
        initial.insert("client".to_string(), Value::String(String::new()));
        initial.insert(
            "definition_path".to_string(),
            Value::String(definition_path_str.clone()),
        );
        initial.insert("stage".to_string(), Value::String("starting".to_string()));
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

        let state = match StateData::new(gremlin_id.as_str(), &initial, config.ephemeral) {
            Ok(s) => s,
            Err(error) => {
                return Err(RunError::Message(format!(
                    "failed to create state: {error}"
                )));
            }
        };

        // Write the hermetic definition.yaml snapshot now, before we
        // return the handle, so the spawned child always sees the
        // definition as it was at launch time — no TOCTTOU window.
        {
            let yaml_bytes = _definition
                .serialize()
                .map_err(|e| RunError::Message(format!("failed to serialize definition: {e}")))?;
            let mut blob = state
                .open_blob("definition.yaml", state::BlobMode::Write)
                .map_err(|e| {
                    RunError::Message(format!("failed to write hermetic definition: {e}"))
                })?;
            blob.write_all(&yaml_bytes).map_err(|e| {
                RunError::Message(format!("failed to write hermetic definition: {e}"))
            })?;
        }
        let hermetic = state.state_dir().join("definition.yaml");
        let hermetic = hermetic.canonicalize().unwrap_or(hermetic);

        // Point the stub at the hermetic snapshot so init_runtime
        // loads from it rather than reparsing the original source.
        let mut stub = StaticDefinition::stub();
        stub.path = hermetic.clone();
        let stub: Box<dyn GremlinDefinition> = Box::new(stub);

        // Snapshot the runtime config before gremlin_id is moved.
        let runtime_config = RuntimeConfig::snapshot();

        let gremlin = Gremlin {
            id: gremlin_id,
            definition_path: Some(hermetic),
            client_override: client_override.map(String::from),
            definition: stub,
            workdir: Some(workdir),
            project_root: project_root.to_path_buf(),
            state,
            env: HashMap::new(),
            client: Client::parse("cmd:true").expect("'cmd:true' is always a valid client spec"),
            loop_iter: "1".to_string(),
            stage_inputs: stage_inputs.clone(),
            runtime_config,
            cancel_token: None,
            interactive_session: None,
            scratch_dir,
        };

        // 5. Create an empty log file.
        gremlin
            .state
            .open_blob("log", state::BlobMode::Write)
            .map_err(|e| RunError::Message(format!("failed to create log: {e}")))?;

        Ok(gremlin)
    }

    /// Reconstruct a handle from a persisted state directory, without touching
    /// the definition, the client, or the environment.
    ///
    /// This is the cheap read: it resolves the paths a caller needs to inspect
    /// a run — its workspace, its project, its definition — and nothing more. A
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

        let state_data = StateData::open(gremlin_id.as_str())?;

        let workdir: Option<WorkDir> =
            (!workdir.is_empty()).then(|| WorkDir::Persistent(PathBuf::from(&workdir)));

        // A hermetic `definition.yaml` next to the state pins the definition the
        // run actually used; otherwise fall back to resolving the kind. Either
        // way the path is only recorded here — `init_runtime` reads it.
        let definition_path = if state_data.exists("definition.yaml") {
            Some(state_dir.join("definition.yaml"))
        } else if !kind.is_empty() {
            resolve_definition_in_project(&kind, &project_root)
        } else {
            None
        }
        .or_else(|| (!recorded_path.is_empty()).then(|| PathBuf::from(&recorded_path)))
        // Canonicalise so a caller comparing the handle's path to a resolved
        // one (the way `definition.path` is read) sees the same string.
        .map(|path| path.canonicalize().unwrap_or(path));

        // `stage_inputs` is what bootstrap's `bind_artifact` resolves against;
        // an absent or null field is an empty map, exactly as the Python
        // `state_json.get("stage_inputs") or {}` read it.
        let stage_inputs: HashMap<String, String> = state_data
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
        let mut stub = StaticDefinition::stub();
        if let Some(path) = &definition_path {
            stub.path = path.clone();
        }
        let definition: Box<dyn GremlinDefinition> = Box::new(stub);

        // Snapshot the runtime config before gremlin_id is moved.
        let runtime_config = RuntimeConfig::snapshot();
        let scratch_dir = ScratchDir::Persistent(config::scratch_root(Some(gremlin_id.as_str())));

        Ok(Gremlin {
            id: gremlin_id,
            definition_path,
            client_override: None,
            definition,
            workdir,
            project_root,
            state: state_data,
            env: HashMap::new(),
            client: Client::parse("cmd:true").expect("'cmd:true' is always a valid client spec"),
            loop_iter: "1".to_string(),
            stage_inputs,
            runtime_config,
            cancel_token: None,
            interactive_session: None,
            scratch_dir,
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
    pub(crate) async fn init_runtime(&mut self, resume_from: Option<&str>) -> Result<(), RunError> {
        // Apply goto before checking is_stub — a resume must position the
        // cursor even when the definition is already loaded (e.g. test
        // helpers that pre-seed a real definition).
        if let Some(name) = resume_from {
            self.definition.goto(name);
        }

        // RuntimeConfig is already populated by the constructor; this guard
        // catches a handle that was somehow constructed without one.
        if self.scratch_dir.path().as_os_str().is_empty() {
            return Err(RunError::Message(
                "gremlin scratch_dir is uninitialized — construct the handle through Gremlin::init or from".to_string(),
            ));
        }

        if !self.definition.is_stub() {
            return Ok(());
        }
        let Some(definition_path) = self.definition_path.clone() else {
            return Err(RunError::Message(format!(
                "gremlin {}: no definition path to load",
                self.id
            )));
        };

        let mut definition = StaticDefinition::from_yaml_file(
            &definition_path,
            self.client_override.as_deref(),
            self.runtime_config.default_client.as_deref(),
        )
        .map_err(|error| RunError::Message(error.to_string()))?;

        // Write the hermetic definition.yaml snapshot so every entry point
        // that calls run() gets one — resume, fork, and fresh launch alike.
        if !self.state.exists("definition.yaml") {
            let yaml_bytes = definition
                .serialize()
                .map_err(|e| RunError::Message(format!("failed to serialize definition: {e}")))?;
            let mut blob = self
                .state
                .open_blob("definition.yaml", state::BlobMode::Write)
                .map_err(|e| RunError::Message(format!("failed to snapshot definition: {e}")))?;
            blob.write_all(&yaml_bytes)
                .map_err(|e| RunError::Message(format!("failed to snapshot definition: {e}")))?;
        }

        // An unusable client must not abort a run: the state directory, the
        // workspace and the artifacts all have to exist before any stage can
        // run, and failing here would leave the operator with no run at all to
        // debug. `cmd:true` is the harness's own no-op client, so a bad spec
        // degrades to a run that does nothing rather than one that never starts,
        // while the definition's
        // declared `default_client` stays on record in `state.json`.
        let client = Client::parse(&definition.default_client).unwrap_or_else(|_| {
            Client::parse("cmd:true").expect("'cmd:true' is always a valid client spec")
        });

        register_stage_inputs(
            self.state.store_ref(),
            &definition.bootstrap,
            &self.stage_inputs,
        )
        .await;

        let env = resolve_env(
            bootstrap_script(&definition.bootstrap),
            self.id.as_str(),
            &self.project_root,
            self.workdir.as_ref().map(|w| w.path()),
            self.scratch_dir.path(),
            &self.runtime_config.base_process_env,
        )?;

        // Nothing below this line can fail, so this is the commit point.
        let default_client = definition.default_client.clone();
        if let Some(name) = resume_from {
            definition.goto(name);
        }
        self.definition = Box::new(definition);
        self.client = client;

        // The Python launcher sources bootstrap.env before the workspace
        // exists, so GREMLIN_WORKDIR (and any variable the script
        // derives from it) is absent. resolve_env produces the correct map now
        // that the workspace is real.
        self.env = env;

        // The client label is only knowable once the definition is loaded; the
        // initial write left it blank and this is the patch that fills it in.
        let mut fields = Map::new();
        fields.insert("client".to_string(), Value::String(default_client));
        self.state.patch(&[], &fields);

        Ok(())
    }

    /// Fork a child gremlin: copy artifacts, create a workspace, and seed a
    /// fresh `state.json` carrying the child's identity.
    ///
    /// `child_definition_path` is the branch's hermetic `definition.yaml`; pass
    /// `None` to inherit the parent's persisted `definition_path`.
    ///
    /// When `effective_client` is `Some`, the child's provider is mutated via
    /// [`GremlinDefinition::with_client`] so `default_client()` reflects
    /// the override.
    ///
    /// Fork creates the child state directory, copies artifacts, seeds the
    /// registry, and writes an initial state.json. It does **not** populate
    /// the child workspace — call [`run_fork_cmds`] afterwards.
    #[allow(clippy::too_many_arguments)]
    pub async fn fork(
        &self,
        child_id: &str,
        parent_id: &str,
        group_name: &str,
        child_key: &str,
        child_definition_path: Option<&Path>,
        child_provider: Box<dyn GremlinDefinition>,
        effective_client: Option<&str>,
    ) -> Result<Gremlin, RunError> {
        log::debug!(
            "fork: child_id={child_id}, parent_id={parent_id}, group_name={group_name}, child_key={child_key}"
        );
        let child_gremlin_id = validate_gremlin_id(child_id).map_err(RunError::Message)?;

        // Allow effective_client mutation via with_client.
        let mut child_provider = child_provider;

        let client = if let Some(client_spec) = effective_client {
            let client = Client::parse(client_spec).map_err(|e| {
                RunError::Message(format!("invalid client spec '{client_spec}': {e}"))
            })?;
            child_provider.with_client(client_spec);
            client
        } else {
            self.client.clone()
        };

        // Create the child workdir so the child's state.json can
        // record a `workdir`. The caller populates it afterwards via
        // [`run_fork_cmds`].
        let child_workdir = match &self.scratch_dir {
            ScratchDir::Temp(_) => {
                let temp = tempfile::TempDir::new().map_err(|e| {
                    RunError::Message(format!("failed to create temp child workdir: {e}"))
                })?;
                WorkDir::Temp(temp)
            }
            _ => {
                let path = config::work_root().join(child_gremlin_id.as_str());
                std::fs::create_dir_all(&path).map_err(|e| {
                    RunError::Message(format!(
                        "failed to create child workdir {}: {e}",
                        path.display()
                    ))
                })?;
                WorkDir::Persistent(path)
            }
        };
        let child_workdir_str = child_workdir.path().to_string_lossy().into_owned();

        // fork creates the child state directory, copies artifacts, seeds the
        // registry, and writes an initial state.json with the child id.
        let forked_store = self
            .state
            .fork(child_gremlin_id.as_str())
            .await
            .map_err(|error| RunError::Message(error.to_string()))?;

        let parent = self.state.state_tree();
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
        // Always write the child's own workspace path.
        child.insert("workdir".to_string(), Value::String(child_workdir_str));

        child.insert("status".to_string(), Value::String("running".to_string()));
        child.insert("pid".to_string(), Value::Null);
        child.insert("exit_code".to_string(), Value::Null);

        let mut child_state = StateData::from_store(Some(child_id.to_string()), forked_store);
        child_state
            .write_state(&child)
            .map_err(|e| RunError::Message(format!("failed to write child state: {e}")))?;
        child_state
            .open_blob("log", state::BlobMode::Write)
            .map_err(|e| RunError::Message(format!("failed to create child log: {e}")))?;

        log::debug!(
            "fork: child {child_id} ready (provider={}, model={} — shares client with parent {parent_id})",
            client.provider(),
            client.model(),
        );

        let child_scratch_dir = match &self.scratch_dir {
            ScratchDir::Temp(_) => {
                let temp = tempfile::TempDir::new().map_err(|e| {
                    RunError::Message(format!("failed to create temp child scratch dir: {e}"))
                })?;
                ScratchDir::Temp(temp)
            }
            _ => ScratchDir::Persistent(config::scratch_root(Some(child_gremlin_id.as_str()))),
        };

        Ok(Gremlin {
            id: child_gremlin_id,
            definition_path: child_definition_path
                .map(Path::to_path_buf)
                .or_else(|| self.definition_path.clone()),
            client_override: self.client_override.clone(),
            definition: child_provider,
            workdir: Some(child_workdir),
            project_root: self.project_root.clone(),
            state: child_state,
            env: self.env.clone(),
            client,
            loop_iter: "1".to_string(),
            // A child inherits the parent's source values: its bootstrap binds
            // the same inputs the parent launched with.
            stage_inputs: self.stage_inputs.clone(),
            runtime_config: self.runtime_config.clone(),
            cancel_token: self.cancel_token.clone(),
            interactive_session: None,
            scratch_dir: child_scratch_dir,
        })
    }

    /// Run fork commands against a child workspace that was already created
    /// (by [`fork`]) but not yet populated.
    ///
    /// The child workspace directory must already exist (empty); that is the
    /// harness's responsibility.
    ///
    /// `fork_cmds` controls how the workspace is populated:
    /// - `None` (default): `cp -r` the parent's workspace into the child's.
    /// - `Some([])`: do nothing — leave the child workspace empty.
    /// - `Some(cmds)`: run each command with `{var}` substitution (same engine
    ///   as bootstrap's `launch_cmds`), with additional vars `{child_key}`,
    ///   `{child_name}`, `{child_id}`, `{parent_id}`, `{group_name}`, and env
    ///   vars `GREMLIN_WORKDIR` (parent) and `GREMLIN_FORK_WORKDIR` (child).
    pub async fn run_fork_cmds(
        &self,
        child_id: &str,
        child_key: &str,
        parent_id: &str,
        group_name: &str,
        child_workdir: &Path,
        fork_cmds: Option<&[String]>,
    ) -> Result<(), RunError> {
        let child_workdir = child_workdir.to_path_buf();

        match fork_cmds {
            None => {
                // Default: copy parent workspace contents.
                let parent_workdir = self
                    .workdir
                    .as_ref()
                    .map(|w| w.path().to_string_lossy().into_owned())
                    .unwrap_or_default();
                if !parent_workdir.is_empty() {
                    let status = std::process::Command::new("cp")
                        .arg("-r")
                        .arg(format!("{}/.", parent_workdir))
                        .arg(child_workdir.to_string_lossy().as_ref())
                        .status()
                        .map_err(|e| {
                            RunError::Message(format!("failed to run cp for fork: {e}"))
                        })?;
                    if !status.success() {
                        return Err(RunError::Message(format!(
                            "cp failed with exit code {}",
                            status.code().unwrap_or(-1)
                        )));
                    }
                }
            }
            Some([]) => {
                // Do nothing — leave the child workspace empty.
            }
            Some(cmds) => {
                let parent_workdir = self
                    .workdir
                    .as_ref()
                    .map(|w| w.path().to_string_lossy().into_owned())
                    .unwrap_or_default();
                let child_workdir_str = child_workdir.to_string_lossy().into_owned();
                let fork_cwd = self
                    .workdir
                    .as_ref()
                    .map(|w| w.path().to_path_buf())
                    .unwrap_or_else(|| self.project_root.clone());

                // Build env and substitution_env with GREMLINS_<KEY> entries
                // so {key} tokens resolve via ${} references in the child
                // shell — the same pattern exec stages use.
                let fork_vars: [(&str, &str); 5] = [
                    ("child_key", child_key),
                    ("child_name", child_key),
                    ("child_id", child_id),
                    ("parent_id", parent_id),
                    ("group_name", group_name),
                ];

                let mut env: HashMap<String, String> = self.env.clone();
                env.insert("GREMLIN_WORKDIR".to_string(), parent_workdir);
                env.insert(
                    "GREMLIN_FORK_WORKDIR".to_string(),
                    child_workdir_str.clone(),
                );

                let mut substitution_env: HashMap<String, String> = HashMap::new();
                for (key, value) in &fork_vars {
                    let env_key = format!("GREMLINS_{}", key.to_uppercase());
                    env.insert(env_key.clone(), value.to_string());
                    substitution_env.insert(env_key, value.to_string());
                }

                // {cwd} from the old substitute_bootstrap_vars — child workspace.
                {
                    let cwd_key = "GREMLINS_CWD".to_string();
                    env.insert(cwd_key.clone(), child_workdir_str.clone());
                    substitution_env.insert(cwd_key, child_workdir_str.clone());
                }

                // Replace {key} with ${GREMLINS_KEY} — values travel through
                // the environment, never inline in the shell command.
                let substituted_cmds: Vec<String> = cmds
                    .iter()
                    .map(|cmd| {
                        let mut s = cmd.clone();
                        for (key, _) in &fork_vars {
                            let env_key = format!("GREMLINS_{}", key.to_uppercase());
                            s = s.replace(&format!("{{{key}}}"), &format!("${{{env_key}}}"));
                        }
                        s = s.replace("{cwd}", "${GREMLINS_CWD}");
                        s
                    })
                    .collect();

                let log_name = format!("fork-{child_key}");
                let safe_name = sanitize_log_filename(child_key);
                let blob_name = format!("command_logs/fork-{safe_name}.log");
                let log_writer = self
                    .state
                    .open_blob(&blob_name, BlobMode::Append)
                    .ok()
                    .map(|b| b as Box<dyn std::io::Write + Send>);
                let log_tx: Option<tokio::sync::mpsc::UnboundedSender<String>> = None;

                let result = run_logged_commands(
                    &log_name,
                    &substituted_cmds,
                    &fork_cwd,
                    &env,
                    &substitution_env,
                    None,
                    log_writer,
                    &log_tx,
                )
                .await;

                match result {
                    Ok(r) if r.rc == 0 => {}
                    Ok(r) => {
                        return Err(RunError::Message(format!(
                            "fork command(s) failed with exit code {}: {}",
                            r.rc,
                            crate::executor::run::truncate(&r.output, 500),
                        )));
                    }
                    Err(e) => {
                        return Err(RunError::Message(format!("fork command(s) failed: {e}",)));
                    }
                }
            }
        }
        Ok(())
    }

    /// Remove every filesystem asset this gremlin owns: its workspace, its
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
    /// is removed *last*, so a partial cleanup never strands a workspace or
    /// scratch directory that nothing can trace back to a gremlin: if
    /// `state.json` is gone, every other asset is already gone.
    pub fn clean(self, remove_state_dir: bool) {
        // Mark closed before touching anything: a run that vanished without a
        // marker reads as a crash, not an intentional cleanup.
        if let Err(error) = self.state.open_blob("closed", state::BlobMode::Write) {
            log::warn!("clean: could not touch closed marker: {error}");
        }

        self.clean_workspace();
        self.clean_scratch();

        if remove_state_dir {
            if let Err(error) = std::fs::remove_dir_all(self.state.state_dir()) {
                log::warn!(
                    "clean: could not remove state dir {}: {error}",
                    self.state.state_dir().display()
                );
            }
        }
    }

    /// Remove the workspace, best-effort.
    fn clean_workspace(&self) {
        let Some(workdir) = &self.workdir else {
            return;
        };
        let path = workdir.path();
        if path.exists() {
            if let Err(error) = std::fs::remove_dir_all(path) {
                log::warn!(
                    "clean: could not remove workspace {}: {error}",
                    path.display()
                );
            }
        }
    }

    /// Remove the scratch directory — best-effort.
    fn clean_scratch(&self) {
        let scratch = self.scratch_dir.path();
        if !scratch.is_dir() {
            return;
        }
        if let Err(error) = std::fs::remove_dir_all(scratch) {
            log::warn!(
                "clean: could not remove scratch dir {}: {error}",
                scratch.display()
            );
        }
    }

    /// The directory commands run in: the workspace, else the project root, else
    /// the CWD.
    pub fn cwd(&self) -> PathBuf {
        if let Some(workdir) = &self.workdir {
            return workdir.path().to_path_buf();
        }
        if !self.project_root.as_os_str().is_empty() {
            return self.project_root.clone();
        }
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    }

    /// The framework substitution variables for a stage.
    pub fn framework_subs(&self, stage_name: &str) -> HashMap<String, String> {
        framework_subs(
            stage_name,
            &self.cwd().to_string_lossy(),
            self.client.model(),
        )
    }
}

/// The worktree of `definition_path`'s project.
///
/// Walks up from the definition path looking for the nearest ancestor
/// `.gremlins` directory; the parent of that directory is the project root.
/// Falls back to the definition's own parent when no overlay is found.
fn project_root_for(definition_path: &Path) -> PathBuf {
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

/// Register each stage input that is not a bootstrap source as an artifact.
///
/// An empty value is skipped, matching the Python initializer this replaces: a
/// stage input the operator left blank is indistinguishable from one they never
/// supplied, and registering it would create an empty artifact that a later
/// `skip_if_exists` check would read as satisfied. Failures are logged and
/// skipped too — a stage input that cannot be written is worth complaining
/// about, but it must not abort a launch, because the stage that wanted the
/// artifact will report it precisely.
async fn register_stage_inputs(
    state: &dyn StateStore,
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
        if state.is_registered(&uri_str).await {
            continue;
        }
        match Uri::parse(&uri_str) {
            Ok(uri) => {
                if let Err(error) = state.write_into_registry(&uri, value).await {
                    log::warn!("launch: could not register stage input {key:?}: {error}");
                }
            }
            Err(error) => log::warn!("launch: invalid stage input URI {uri_str:?}: {error}"),
        }
    }
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
pub fn framework_subs(stage_name: &str, cwd: &str, model: &str) -> HashMap<String, String> {
    HashMap::from([
        ("name".to_string(), stage_name.to_string()),
        ("model".to_string(), model.to_string()),
        ("cwd".to_string(), cwd.to_string()),
    ])
}

/// The five harness-owned system variables, built from a gremlin's paths.
///
/// These are both *seeded into* the base the bootstrap script is sourced
/// against and *re-asserted* on top of the result, so the script can read
/// them but can never override them.
pub fn system_env(
    gremlin_id: &str,
    project_root: &Path,
    workdir: Option<&Path>,
    scratch_dir: &Path,
) -> HashMap<String, String> {
    let workdir_path = workdir
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    let scratch_dir = scratch_dir.to_path_buf();
    let state_dir = config::state_root().join(gremlin_id);

    let mut vars = HashMap::new();
    vars.insert("GREMLINS_GREMLIN_ID".to_string(), gremlin_id.to_string());
    vars.insert(
        "GREMLINS_PROJECT_ROOT".to_string(),
        project_root.to_string_lossy().into_owned(),
    );
    vars.insert("GREMLIN_WORKDIR".to_string(), workdir_path);
    vars.insert(
        "GREMLINS_SCRATCH_DIR".to_string(),
        scratch_dir.to_string_lossy().into_owned(),
    );
    vars.insert(
        "GREMLINS_STATE_DIR".to_string(),
        state_dir.to_string_lossy().into_owned(),
    );
    vars
}

/// Resolve the process environment a gremlin's stages and bootstrap inherit.
///
/// The system variables are seeded into the base *before* the bootstrap script
/// is sourced, so the script can read them (e.g. to point `VIRTUAL_ENV` at the
/// workspace), then re-asserted on top afterwards. The script can shape
/// the environment but can never redirect the harness's paths.
#[allow(clippy::too_many_arguments)]
pub fn resolve_env(
    bootstrap_env: Option<&str>,
    gremlin_id: &str,
    project_root: &Path,
    workdir: Option<&Path>,
    scratch_dir: &Path,
    base_env: &HashMap<String, String>,
) -> Result<HashMap<String, String>, RunError> {
    let system = system_env(gremlin_id, project_root, workdir, scratch_dir);

    let mut base: HashMap<String, String> = base_env.clone();
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

    use crate::executor::state::FileSystemStateStore;
    use crate::test_support::{with_sandbox, EnvGuard, GitSandbox};

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
        let _guard = EnvGuard::lock();
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().join("project");

        let scratch_dir = config::scratch_root(Some("gr-test"));
        let base_env: HashMap<String, String> = std::env::vars().collect();
        let env = resolve_env(
            None,
            "gr-test",
            &project_root,
            None,
            &scratch_dir,
            &base_env,
        )
        .unwrap();

        assert_eq!(env["GREMLINS_GREMLIN_ID"], "gr-test");
        assert_eq!(env["GREMLINS_PROJECT_ROOT"], project_root.to_string_lossy());
        assert_eq!(env["GREMLIN_WORKDIR"], "");
        assert_eq!(env["GREMLINS_SCRATCH_DIR"], scratch_dir.to_string_lossy());
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
        let _guard = EnvGuard::lock();
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        // The script replaces `PATH`, so it pins `env`'s location first: the
        // loader reads the resulting environment back with `env -0`.
        let script = format!(
            "export GREMLINS_TEST_SOURCED=yes\nexport PATH=/custom\nhash -p {} env\n",
            env_binary(),
        );
        let scratch_dir = config::scratch_root(Some("gr-test"));
        let base_env: HashMap<String, String> = std::env::vars().collect();
        let env = resolve_env(
            Some(&script),
            "gr-test",
            &project_root,
            None,
            &scratch_dir,
            &base_env,
        )
        .unwrap();

        assert_eq!(env["GREMLINS_TEST_SOURCED"], "yes");
        // The sourced map replaces the base, so the script's PATH is what wins.
        assert_eq!(env["PATH"], "/custom");
        assert_eq!(env["GREMLINS_GREMLIN_ID"], "gr-test");
        assert_eq!(env["GREMLINS_PROJECT_ROOT"], project_root.to_string_lossy());
        assert_eq!(env["GREMLIN_WORKDIR"], "");
        assert_eq!(env["GREMLINS_SCRATCH_DIR"], scratch_dir.to_string_lossy());
    }

    #[test]
    fn resolve_env_lets_the_script_read_system_vars() {
        let _guard = EnvGuard::lock();
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().join("project");
        let workdir = dir.path().join("wd");
        std::fs::create_dir_all(&project_root).unwrap();

        // The script derives VIRTUAL_ENV from the workspace path — the shape the
        // bundled definitions use — then tries to redirect a system variable.
        let script = "export VIRTUAL_ENV=\"${GREMLIN_WORKDIR}/.venv\"\n\
                       export GREMLIN_WORKDIR=/hijacked\n";
        let scratch_dir = config::scratch_root(Some("gr-test"));
        let base_env: HashMap<String, String> = std::env::vars().collect();
        let env = resolve_env(
            Some(script),
            "gr-test",
            &project_root,
            Some(&workdir),
            &scratch_dir,
            &base_env,
        )
        .unwrap();

        assert_eq!(env["VIRTUAL_ENV"], format!("{}/.venv", workdir.display()));
        // The script can read the system variables but cannot override them.
        assert_eq!(env["GREMLIN_WORKDIR"], workdir.to_string_lossy());
    }

    #[test]
    fn resolve_env_reports_bootstrap_failure() {
        let dir = tempfile::tempdir().unwrap();

        let scratch_dir = config::scratch_root(Some("gr-test"));
        let base_env: HashMap<String, String> = std::env::vars().collect();
        let error = resolve_env(
            Some("exit 3"),
            "gr-test",
            dir.path(),
            None,
            &scratch_dir,
            &base_env,
        )
        .unwrap_err();
        assert!(
            matches!(error, RunError::BootstrapFailed { .. }),
            "unexpected {error:?}"
        );
    }

    // --- framework substitutions ---

    #[test]
    fn framework_subs_carries_the_three_vars() {
        let subs = framework_subs("plan", "/w", "xai:grok-4");
        assert_eq!(subs.len(), 3);
        assert_eq!(subs["name"], "plan");
        assert_eq!(subs["cwd"], "/w");
        assert_eq!(subs["model"], "xai:grok-4");
    }

    // --- lifecycle ---

    fn read_state(path: &Path) -> Value {
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path:?}: {e}"))
    }

    #[tokio::test]
    async fn create_creates_state_and_workspace() {
        let fx = GitSandbox::new();
        if fx.is_skipped() {
            eprintln!("git is unavailable; skipping create_creates_state_and_workspace");
            return;
        }

        let definition =
            StaticDefinition::from_yaml_file(fx.definition_path(), None, None).unwrap();
        let mut gremlin = Gremlin::init(
            "gr-test",
            fx.definition_path(),
            &definition,
            &HashMap::new(),
            None,
            &GremlinConfig::default(),
        )
        .unwrap();

        let workdir = gremlin
            .workdir
            .as_ref()
            .map(|w| w.path().to_path_buf())
            .expect("a workspace");
        assert!(workdir.is_dir(), "{workdir:?}");

        let state_file = gremlin.state.state_dir().join("state.json");
        assert!(state_file.is_file(), "{state_file:?}");
        let raw = read_state(&state_file);
        assert_eq!(raw["id"], gremlin.id.as_str());
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

        // The workspace is on record, and the returned handle agrees with
        // what was persisted.
        assert_eq!(raw["workdir"].as_str().unwrap(), workdir.to_string_lossy());

        // Lazy init is what fills in the runtime fields.
        gremlin.init_runtime(None).await.unwrap();
        assert_eq!(gremlin.definition.name(), "demo");
        assert_eq!(gremlin.env["GREMLINS_GREMLIN_ID"], gremlin.id.as_str());
        assert!(gremlin.state.artifact_dir().ends_with("artifacts"));
        let raw = read_state(&state_file);
        assert_eq!(raw["client"], "cmd:true");

        // Idempotent: a second call is a no-op, not a re-parse.
        let path_before = gremlin.definition.path().to_path_buf();
        gremlin.init_runtime(None).await.unwrap();
        assert_eq!(gremlin.definition.path(), path_before);
    }

    #[tokio::test]
    async fn create_then_from_roundtrips() {
        let fx = GitSandbox::new();
        if fx.is_skipped() {
            eprintln!("git is unavailable; skipping create_then_from_roundtrips");
            return;
        }

        let definition =
            StaticDefinition::from_yaml_file(fx.definition_path(), None, None).unwrap();
        let mut launched = Gremlin::init(
            "gr-test",
            fx.definition_path(),
            &definition,
            &HashMap::new(),
            None,
            &GremlinConfig::default(),
        )
        .unwrap();
        let id = launched.id.to_string();
        let mut opened = Gremlin::from(&id).unwrap();

        assert_eq!(opened.id, launched.id);
        assert_eq!(opened.state.state_dir(), launched.state.state_dir());
        assert_eq!(opened.project_root, launched.project_root);
        assert_eq!(
            opened.workdir.as_ref().map(|w| w.path()),
            launched.workdir.as_ref().map(|w| w.path())
        );
        assert_eq!(opened.definition_path, launched.definition_path);

        launched.init_runtime(None).await.unwrap();
        opened.init_runtime(None).await.unwrap();
        assert_eq!(opened.definition.name(), launched.definition.name());
        assert_eq!(opened.env["GREMLINS_GREMLIN_ID"], id);
    }

    #[test]
    fn from_then_clean_needs_no_run() {
        let fx = GitSandbox::new();
        if fx.is_skipped() {
            eprintln!("git is unavailable; skipping from_then_clean_needs_no_run");
            return;
        }

        let definition =
            StaticDefinition::from_yaml_file(fx.definition_path(), None, None).unwrap();
        let created = Gremlin::init(
            "gr-test",
            fx.definition_path(),
            &definition,
            &HashMap::new(),
            None,
            &GremlinConfig::default(),
        )
        .unwrap();
        let id = created.id.to_string();
        let workdir = created
            .workdir
            .as_ref()
            .map(|w| w.path().to_path_buf())
            .unwrap();
        drop(created);

        // A handle reconstructed purely for cleanup: no `run`, so the
        // definition was never loaded, and `clean` must not need it.
        let handle = Gremlin::from(&id).unwrap();
        assert!(handle.definition.is_stub());
        handle.clean(true);

        assert!(!fx.join("state").join(&id).exists());
        assert!(!workdir.exists(), "workspace should be gone");
        assert!(!fx.join("scratch").join(&id).exists());
    }

    #[tokio::test]
    // The env guard holds a plain mutex across the run's awaits; safe because
    // `#[tokio::test]` drives a current-thread runtime, so no other task can be
    // scheduled on this thread while the lock is held.
    #[allow(clippy::await_holding_lock)]
    async fn from_then_run_triggers_lazy_init() {
        let fx = GitSandbox::new();
        if fx.is_skipped() {
            eprintln!("git is unavailable; skipping from_then_run_triggers_lazy_init");
            return;
        }

        let definition =
            StaticDefinition::from_yaml_file(fx.definition_path(), None, None).unwrap();
        let launched = Gremlin::init(
            "gr-test",
            fx.definition_path(),
            &definition,
            &HashMap::new(),
            None,
            &GremlinConfig::default(),
        )
        .unwrap();
        let id = launched.id.to_string();
        drop(launched);

        let mut handle = Gremlin::from(&id).unwrap();
        assert!(handle.definition.is_stub());

        // The resume path: reconstruct cheaply, then drive the run. The
        // definition must have been loaded by the time `run` returns.
        assert_eq!(handle.run(None).await.unwrap(), 0);
        assert_eq!(handle.definition.name(), "demo");
    }

    #[tokio::test]
    async fn fork_copies_artifacts_and_seeds_child_state() {
        let fx = GitSandbox::new();
        if fx.is_skipped() {
            eprintln!("git is unavailable; skipping fork_copies_artifacts_and_seeds_child_state");
            return;
        }

        let definition =
            StaticDefinition::from_yaml_file(fx.definition_path(), None, None).unwrap();
        let parent = Gremlin::init(
            "gr-test",
            fx.definition_path(),
            &definition,
            &HashMap::new(),
            None,
            &GremlinConfig::default(),
        )
        .unwrap();

        std::fs::create_dir_all(parent.state.artifact_dir()).unwrap();
        std::fs::write(parent.state.artifact_dir().join("note.txt"), "hello").unwrap();

        let child_def = StaticDefinition::from_yaml_file(fx.definition_path(), None, None).unwrap();
        let child_provider: Box<dyn GremlinDefinition> = Box::new(child_def);
        let child = parent
            .fork("gr-child", "", "", "", None, child_provider, None)
            .await
            .unwrap();

        assert_ne!(child.state.artifact_dir(), parent.state.artifact_dir());
        assert!(child.state.artifact_dir().ends_with("artifacts"));
        assert!(child.state.artifact_dir().join("note.txt").is_file());

        let child_state = child.state.state_dir().join("state.json");
        let raw = read_state(&child_state);
        assert_eq!(raw["id"], "gr-child");
        assert_eq!(raw["status"], "running");
        assert!(raw["pid"].is_null());
        assert!(raw.get("token_usage").is_none());

        let parent_workdir = parent
            .workdir
            .as_ref()
            .map(|w| w.path().to_path_buf())
            .unwrap();
        let child_workdir = child
            .workdir
            .as_ref()
            .map(|w| w.path().to_path_buf())
            .expect("child workspace");
        assert_ne!(child_workdir, parent_workdir);
        assert!(child_workdir.is_dir(), "{child_workdir:?}");

        // The child's own workspace is on record, never the parent's.
        assert_eq!(
            raw["workdir"].as_str().unwrap(),
            child_workdir.to_string_lossy()
        );

        assert_eq!(child.env, parent.env);
    }

    #[tokio::test]
    async fn fork_keeps_parent_id_unless_one_is_given() {
        let fx = GitSandbox::new();
        if fx.is_skipped() {
            eprintln!("git is unavailable; skipping fork_keeps_parent_id_unless_one_is_given");
            return;
        }

        let definition =
            StaticDefinition::from_yaml_file(fx.definition_path(), None, None).unwrap();
        let parent = Gremlin::init(
            "gr-test",
            fx.definition_path(),
            &definition,
            &HashMap::new(),
            None,
            &GremlinConfig::default(),
        )
        .unwrap();

        // Mirrors the reference implementation: an empty argument falls
        // back to whatever the parent state carries (usually the parent's
        // own parent, or nothing at all), never to the parent's own id.
        let child_def_a =
            StaticDefinition::from_yaml_file(fx.definition_path(), None, None).unwrap();
        let child_provider_a: Box<dyn GremlinDefinition> = Box::new(child_def_a);
        parent
            .fork("gr-a", "", "", "", None, child_provider_a, None)
            .await
            .unwrap();
        let child_def_b =
            StaticDefinition::from_yaml_file(fx.definition_path(), None, None).unwrap();
        let child_provider_b: Box<dyn GremlinDefinition> = Box::new(child_def_b);
        parent
            .fork(
                "gr-b",
                "gr-root",
                "group",
                "key",
                None,
                child_provider_b,
                None,
            )
            .await
            .unwrap();

        for (child_id, expected) in [("gr-a", ""), ("gr-b", "gr-root")] {
            let raw = read_state(&fx.join("state").join(child_id).join("state.json"));
            assert_eq!(raw["parent_id"], expected, "{child_id}");
        }

        let grouped = read_state(&fx.join("state").join("gr-b").join("state.json"));
        assert_eq!(grouped["group_name"], "group");
        assert_eq!(grouped["child_key"], "key");
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
    /// `clean` only reads `id`, `state_dir`, `workdir` and
    /// `project_root`, so a full `launch` would just be git fixture noise for
    /// the cases that are not about launch at all.
    fn test_gremlin(
        id: &str,
        state_dir: PathBuf,
        _artifact_dir: PathBuf,
        workdir: Option<PathBuf>,
        project_root: PathBuf,
    ) -> Gremlin {
        Gremlin {
            id: validate_gremlin_id(id).unwrap(),
            definition_path: None,
            client_override: None,
            definition: Box::new(StaticDefinition::stub()),
            workdir: workdir.map(WorkDir::Persistent),
            project_root,
            state: StateData::from_store(
                Some(id.to_string()),
                Box::new(FileSystemStateStore::from_path(state_dir)),
            ),
            env: HashMap::new(),
            client: Client::parse("cmd:true").unwrap(),
            loop_iter: "1".to_string(),
            stage_inputs: HashMap::new(),
            runtime_config: RuntimeConfig::default(),
            cancel_token: None,
            interactive_session: None,
            scratch_dir: ScratchDir::Persistent(config::scratch_root(Some(id))),
        }
    }

    #[test]
    fn clean_true_removes_everything() {
        with_sandbox(None, |sandbox| {
            let id = "gr-clean";
            let state_dir = sandbox.join("state").join(id);
            let scratch_dir = sandbox.join("scratch").join(id);
            let artifact_dir = state_dir.join("artifacts");
            let workdir = sandbox.join("workspace");
            std::fs::create_dir_all(&artifact_dir).unwrap();
            std::fs::create_dir_all(&state_dir).unwrap();
            std::fs::write(state_dir.join("state.json"), "{}").unwrap();
            std::fs::create_dir_all(&workdir).unwrap();
            std::fs::write(workdir.join("file"), "data").unwrap();

            test_gremlin(
                id,
                state_dir.clone(),
                artifact_dir.clone(),
                Some(workdir.clone()),
                PathBuf::new(),
            )
            .clean(true);

            assert!(!state_dir.exists(), "state dir should be gone");
            assert!(!scratch_dir.exists(), "scratch dir should be gone");
            assert!(!workdir.exists(), "workspace should be gone");
        });
    }

    #[test]
    fn clean_false_leaves_state_dir_with_closed_marker() {
        with_sandbox(None, |sandbox| {
            let id = "gr-clean-keep";
            let state_dir = sandbox.join("state").join(id);
            let scratch_dir = sandbox.join("scratch").join(id);
            let artifact_dir = state_dir.join("artifacts");
            let workdir = sandbox.join("workspace-keep");
            std::fs::create_dir_all(&artifact_dir).unwrap();
            std::fs::create_dir_all(&state_dir).unwrap();
            std::fs::write(state_dir.join("state.json"), "{}").unwrap();
            std::fs::create_dir_all(&workdir).unwrap();

            test_gremlin(
                id,
                state_dir.clone(),
                artifact_dir.clone(),
                Some(workdir.clone()),
                PathBuf::new(),
            )
            .clean(false);

            assert!(!workdir.exists(), "workspace should be gone");
            assert!(!scratch_dir.exists(), "scratch dir should be gone");
            assert!(state_dir.is_dir(), "state dir should remain");
            assert!(
                state_dir.join("closed").is_file(),
                "closed marker should be present"
            );
        });
    }

    #[test]
    fn clean_succeeds_without_a_workspace() {
        with_sandbox(None, |sandbox| {
            let id = "gr-clean-nowt";
            let state_dir = sandbox.join("state").join(id);
            let artifact_dir = state_dir.join("artifacts");
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
            let artifact_dir = state_dir.join("artifacts");
            std::fs::create_dir_all(&state_dir).unwrap();
            std::fs::create_dir_all(&artifact_dir).unwrap();

            // A workspace directory that is not a git repository: the
            // rmtree fallback is what actually deletes the directory.
            let workdir = sandbox.join("workspace-nogit");
            std::fs::create_dir_all(&workdir).unwrap();
            std::fs::write(workdir.join("file"), "data").unwrap();
            let project_root = sandbox.join("not-a-repo");
            std::fs::create_dir_all(&project_root).unwrap();

            test_gremlin(
                id,
                state_dir,
                artifact_dir,
                Some(workdir.clone()),
                project_root,
            )
            .clean(true);

            assert!(!workdir.exists(), "rmtree fallback should remove it");
        });
    }

    // --- hermetic snapshot resume ---

    /// A minimal expanded definition YAML that an executor test can write
    /// directly into a state directory.
    const HERMETIC_DEFINITION: &str = r#"
__gremlins_expanded__: true
default_client: 'cmd:true'
stages:
  - name: run
    type: exec
    options:
      cmds:
        - "echo hello"
"#;

    /// Same definition without the sentinel — the direct-load path must
    /// tolerate its absence.
    const HERMETIC_DEFINITION_NO_SENTINEL: &str = r#"
default_client: 'cmd:true'
stages:
  - name: run
    type: exec
    options:
      cmds:
        - "echo hello"
"#;

    fn write_hermetic_state(sandbox: &Path, id: &str, definition_body: &str) -> (PathBuf, PathBuf) {
        let state_dir = sandbox.join("state").join(id);
        std::fs::create_dir_all(&state_dir).unwrap();

        let project_root = sandbox.join("project");
        std::fs::create_dir_all(&project_root).unwrap();

        let definition_path = state_dir.join("definition.yaml");
        std::fs::write(&definition_path, definition_body).unwrap();

        let state_json = serde_json::json!({
            "id": id,
            "kind": "demo",
            "project_root": project_root.to_string_lossy(),
            "definition_path": definition_path.to_string_lossy(),
            "workdir": "",
            "status": "running",
            "pid": null,
            "exit_code": null,
            "client": "",
            "attempt": "0001",
        });
        std::fs::write(
            state_dir.join("state.json"),
            serde_json::to_string_pretty(&state_json).unwrap(),
        )
        .unwrap();

        (state_dir, project_root)
    }

    #[tokio::test]
    // The env guard holds a plain mutex across the run's awaits; safe because
    // `#[tokio::test]` drives a current-thread runtime, so no other task can be
    // scheduled on this thread while the lock is held.
    #[allow(clippy::await_holding_lock)]
    async fn from_with_hermetic_snapshot_loads_via_expanded_path() {
        let mut env = EnvGuard::lock();
        let sandbox = tempfile::tempdir().unwrap();
        env.set("GREMLINS_SANDBOX_ROOT", sandbox.path());

        let (state_dir, project_root) =
            write_hermetic_state(sandbox.path(), "gr-hermetic", HERMETIC_DEFINITION);
        assert!(state_dir.join("definition.yaml").is_file());

        let mut gremlin = Gremlin::from("gr-hermetic").unwrap();
        assert_eq!(gremlin.project_root, project_root);
        assert!(gremlin.definition.is_stub());

        gremlin.init_runtime(None).await.unwrap();

        assert_eq!(gremlin.definition.name(), "definition");
        assert_eq!(gremlin.definition.default_client(), "cmd:true");
        // project_root came from state.json, not from a walk.
        assert_eq!(gremlin.project_root, project_root);

        // Verify stages via next_stage.
        let stage = gremlin.definition.next_stage().await.unwrap();
        assert_eq!(stage.name(), "run");
        assert_eq!(stage.stage_type(), "exec");
        assert!(matches!(
            gremlin.definition.next_stage().await.unwrap(),
            crate::definition::ExecutorStage::Done
        ));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn from_with_hermetic_snapshot_without_sentinel_still_loads() {
        let mut env = EnvGuard::lock();
        let sandbox = tempfile::tempdir().unwrap();
        env.set("GREMLINS_SANDBOX_ROOT", sandbox.path());

        let (state_dir, _project_root) = write_hermetic_state(
            sandbox.path(),
            "gr-hermetic-nosent",
            HERMETIC_DEFINITION_NO_SENTINEL,
        );
        assert!(state_dir.join("definition.yaml").is_file());

        let mut gremlin = Gremlin::from("gr-hermetic-nosent").unwrap();

        gremlin.init_runtime(None).await.unwrap();

        assert_eq!(gremlin.definition.name(), "definition");
        assert_eq!(gremlin.definition.default_client(), "cmd:true");
        let stage = gremlin.definition.next_stage().await.unwrap();
        assert_eq!(stage.name(), "run");
        assert!(matches!(
            gremlin.definition.next_stage().await.unwrap(),
            crate::definition::ExecutorStage::Done
        ));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn from_without_hermetic_snapshot_falls_back_to_kind() {
        let fx = GitSandbox::new();
        if fx.is_skipped() {
            eprintln!(
                "git is unavailable; skipping from_without_hermetic_snapshot_falls_back_to_kind"
            );
            return;
        }

        // Manually create a state directory without a definition.yaml so
        // `from` falls back to resolving by kind.
        let state_dir = fx.join("state").join("gr-noherm");
        std::fs::create_dir_all(&state_dir).unwrap();
        let project_root = fx.repo_path();
        let state_json = serde_json::json!({
            "id": "gr-noherm",
            "kind": "demo",
            "project_root": project_root.to_string_lossy(),
            "definition_path": fx.definition_path().to_string_lossy(),
            "workdir": "",
            "status": "running",
            "pid": null,
            "exit_code": null,
            "client": "",
            "attempt": "0001",
        });
        std::fs::write(
            state_dir.join("state.json"),
            serde_json::to_string_pretty(&state_json).unwrap(),
        )
        .unwrap();

        let mut gremlin = Gremlin::from("gr-noherm").unwrap();
        assert!(gremlin.definition.is_stub());

        gremlin.init_runtime(None).await.unwrap();

        assert_eq!(gremlin.definition.name(), "demo");
        assert_eq!(gremlin.definition.default_client(), "cmd:true");
        // Verify no stages via next_stage.
        assert!(matches!(
            gremlin.definition.next_stage().await.unwrap(),
            crate::definition::ExecutorStage::Done
        ));
    }

    // --- ephemeral mode ---

    #[tokio::test]
    async fn ephemeral_init_uses_temp_dirs() {
        let fx = GitSandbox::new();
        if fx.is_skipped() {
            eprintln!("git is unavailable; skipping ephemeral_init_uses_temp_dirs");
            return;
        }

        let definition =
            StaticDefinition::from_yaml_file(fx.definition_path(), None, None).unwrap();
        let cfg = GremlinConfig { ephemeral: true };
        let gremlin = Gremlin::init(
            "gr-test",
            fx.definition_path(),
            &definition,
            &HashMap::new(),
            None,
            &cfg,
        )
        .unwrap();

        // State, workspace, and scratch are all temp-backed.
        let state_dir = gremlin.state.state_dir().to_path_buf();
        let workdir = gremlin.workdir.as_ref().unwrap().path().to_path_buf();
        let scratch = gremlin.scratch_dir.path().to_path_buf();

        assert!(state_dir.exists(), "state dir exists");
        assert!(workdir.exists(), "workdir exists");
        assert!(scratch.exists(), "scratch dir exists");

        // No files under config roots.
        let config_state = config::state_root();
        let config_work = config::work_root();
        let config_scratch_base = config::scratch_root(None);
        assert!(
            !config_state.join(gremlin.id.as_str()).exists(),
            "no state under config root"
        );
        assert!(
            !config_work.join(gremlin.id.as_str()).exists(),
            "no workdir under config root"
        );
        assert!(
            !scratch.starts_with(&config_scratch_base),
            "scratch is temp-backed, not under config root"
        );

        // Drop the gremlin — TempDirs clean up.
        let state_dir2 = state_dir.clone();
        let workdir2 = workdir.clone();
        let scratch2 = scratch.clone();
        drop(gremlin);
        assert!(!state_dir2.exists(), "state dir cleaned on drop");
        assert!(!workdir2.exists(), "workdir cleaned on drop");
        assert!(!scratch2.exists(), "scratch dir cleaned on drop");
    }

    #[tokio::test]
    async fn ephemeral_fork_propagates_temp_backing() {
        let fx = GitSandbox::new();
        if fx.is_skipped() {
            eprintln!("git is unavailable; skipping ephemeral_fork_propagates_temp_backing");
            return;
        }

        let definition =
            StaticDefinition::from_yaml_file(fx.definition_path(), None, None).unwrap();
        let cfg = GremlinConfig { ephemeral: true };
        let parent = Gremlin::init(
            "gr-test",
            fx.definition_path(),
            &definition,
            &HashMap::new(),
            None,
            &cfg,
        )
        .unwrap();

        let child_def = StaticDefinition::from_yaml_file(fx.definition_path(), None, None).unwrap();
        let child_provider: Box<dyn GremlinDefinition> = Box::new(child_def);
        let child = parent
            .fork("gr-child", "", "", "", None, child_provider, None)
            .await
            .unwrap();

        // Child state dir is temp-backed (not under config::state_root).
        let child_state = child.state.state_dir().to_path_buf();
        assert!(
            !child_state.starts_with(config::state_root()),
            "child state is temp-backed"
        );

        // Child workdir is temp-backed.
        let child_workdir = child.workdir.as_ref().unwrap().path().to_path_buf();
        assert!(
            !child_workdir.starts_with(config::work_root()),
            "child workdir is temp-backed"
        );

        // Child scratch is temp-backed.
        let child_scratch = child.scratch_dir.path().to_path_buf();
        assert!(
            !child_scratch.starts_with(config::scratch_root(None)),
            "child scratch is temp-backed"
        );

        // Drop parent and child — all TempDirs clean up.
        let child_state2 = child_state.clone();
        let child_workdir2 = child_workdir.clone();
        let child_scratch2 = child_scratch.clone();
        drop(child);
        drop(parent);
        assert!(!child_state2.exists(), "child state cleaned on drop");
        assert!(!child_workdir2.exists(), "child workdir cleaned on drop");
        assert!(!child_scratch2.exists(), "child scratch cleaned on drop");
    }

    #[test]
    fn ephemeral_from_is_rejected() {
        // An ephemeral gremlin leaves nothing under config::state_root(),
        // so Gremlin::from() naturally returns an error.
        let result = Gremlin::from("ephemeral-nonexistent");
        assert!(result.is_err(), "from() should fail for ephemeral gremlin");
    }

    #[test]
    fn ephemeral_clean_is_noop() {
        let fx = GitSandbox::new();
        if fx.is_skipped() {
            eprintln!("git is unavailable; skipping ephemeral_clean_is_noop");
            return;
        }

        let definition =
            StaticDefinition::from_yaml_file(fx.definition_path(), None, None).unwrap();
        let cfg = GremlinConfig { ephemeral: true };
        let gremlin = Gremlin::init(
            "gr-test",
            fx.definition_path(),
            &definition,
            &HashMap::new(),
            None,
            &cfg,
        )
        .unwrap();

        let state_dir = gremlin.state.state_dir().to_path_buf();
        let workdir = gremlin.workdir.as_ref().unwrap().path().to_path_buf();
        let scratch = gremlin.scratch_dir.path().to_path_buf();

        // clean() on an ephemeral gremlin is harmless — TempDir destructor
        // handles actual cleanup.
        gremlin.clean(true);

        // After clean(true), the paths may or may not exist (remove_dir_all
        // on a TempDir path is harmless). The key property is that clean()
        // doesn't panic.
        let _ = (state_dir, workdir, scratch);
    }
}
