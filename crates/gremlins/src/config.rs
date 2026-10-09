use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use indexmap::IndexMap;
use log::warn;
use serde::Deserialize;

/// Default name of the project-local overlay directory.
pub(crate) const OVERLAY_DIRNAME: &str = ".gremlins";

// ---------------------------------------------------------------------------
// Path overrides from settings.yaml "paths" section
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct PathOverrides {
    pub state_root: Option<PathBuf>,
    pub work_root: Option<PathBuf>,
    pub(crate) config_root: Option<PathBuf>,
    pub project_root: Option<PathBuf>,
    pub(crate) overlay_dir: Option<PathBuf>,
    pub scratch_root: Option<PathBuf>,
}

fn parse_path_overrides(paths: &HashMap<String, String>) -> PathOverrides {
    PathOverrides {
        state_root: paths.get("state-root").map(PathBuf::from),
        work_root: paths.get("work-root").map(PathBuf::from),
        config_root: paths.get("config-root").map(PathBuf::from),
        project_root: paths.get("project-root").map(PathBuf::from),
        overlay_dir: paths.get("overlay-dir").map(PathBuf::from),
        scratch_root: paths.get("scratch-root").map(PathBuf::from),
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Settings for the Azure OpenAI backend, read from settings.yaml.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct AzureOpenAiSettings {
    #[serde(default)]
    pub auth: Option<StrictString>,
    #[serde(rename = "auth-scope", default)]
    pub auth_scope: Option<StrictString>,
    #[serde(default)]
    pub endpoint: Option<StrictString>,
    #[serde(rename = "api-version", default)]
    pub api_version: Option<StrictString>,
    #[serde(default)]
    pub token: Option<StrictString>,
    #[serde(rename = "api-key", default)]
    pub api_key: Option<StrictString>,
}

/// A named client profile from `settings.yaml`.
///
/// A profile is flat: one concrete `client:` spec and an optional
/// `task-clients` map. No inheritance, no merging between profiles.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClientProfile {
    pub client: String,
    pub task_clients: Option<IndexMap<String, String>>,
}

/// Parsed content of settings.yaml.
#[derive(Debug, Clone, Default)]
pub struct Config {
    default_client: Option<String>,
    exact_stage_clients: HashMap<String, String>,
    prefix_stage_clients: HashMap<String, String>,
    exact_task_clients: HashMap<String, String>,
    prefix_task_clients: HashMap<String, String>,
    default_task_clients: Option<IndexMap<String, String>>,
    client_profiles: IndexMap<String, ClientProfile>,
    path_overrides: PathOverrides,
    azure_openai: Option<AzureOpenAiSettings>,
    max_tool_output_bytes: u64,
}

/// A string newtype that rejects non-string YAML scalars (numbers,
/// booleans, etc.) instead of silently coercing them via `serde_yaml`.
#[derive(Debug, Clone)]
pub(crate) struct StrictString(pub(crate) String);

impl<'de> Deserialize<'de> for StrictString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = StrictString;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a string")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<StrictString, E> {
                Ok(StrictString(v.to_owned()))
            }

            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<StrictString, E> {
                Ok(StrictString(v))
            }

            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<StrictString, E> {
                Err(serde::de::Error::invalid_type(
                    serde::de::Unexpected::Bool(v),
                    &self,
                ))
            }

            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<StrictString, E> {
                Err(serde::de::Error::invalid_type(
                    serde::de::Unexpected::Signed(v),
                    &self,
                ))
            }

            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<StrictString, E> {
                Err(serde::de::Error::invalid_type(
                    serde::de::Unexpected::Unsigned(v),
                    &self,
                ))
            }

            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<StrictString, E> {
                Err(serde::de::Error::invalid_type(
                    serde::de::Unexpected::Float(v),
                    &self,
                ))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// Typed structure for settings.yaml deserialization.
#[derive(Debug, Deserialize)]
struct ConfigFile {
    #[serde(rename = "default-client")]
    default_client: Option<StrictString>,
    #[serde(rename = "default-client-by-stage")]
    default_client_by_stage: Option<IndexMap<String, StrictString>>,
    /// Deprecated spelling. `default-task-clients` wins when both are set.
    #[serde(rename = "task-clients")]
    task_clients: Option<IndexMap<String, StrictString>>,
    #[serde(rename = "default-task-clients")]
    default_task_clients: Option<IndexMap<String, StrictString>>,
    #[serde(rename = "client-profiles")]
    client_profiles: Option<IndexMap<String, ProfileFile>>,
    paths: Option<HashMap<String, StrictString>>,
    #[serde(rename = "azure-openai", default)]
    azure_openai: Option<AzureOpenAiSettings>,
    #[serde(rename = "max-tool-output-bytes", default)]
    max_tool_output_bytes: Option<u64>,
}

/// A single `client-profiles` entry as it appears in `settings.yaml`.
#[derive(Debug, Deserialize)]
struct ProfileFile {
    client: Option<StrictString>,
    #[serde(rename = "task-clients", default)]
    task_clients: Option<IndexMap<String, StrictString>>,
}

impl Config {
    /// Load from `user_config_root(None) / "settings.yaml"`.
    /// Returns `Config::default()` if the file doesn't exist.
    pub fn load() -> Result<Self, ConfigError> {
        let path = resolve_user_config_root(None).join("settings.yaml");
        let cfg_file = match parse_yaml_config(&path) {
            Ok(v) => v,
            Err(ConfigError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                let max_tool_output_bytes = std::env::var("GREMLINS_MAX_TOOL_OUTPUT_BYTES")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(DEFAULT_MAX_TOOL_OUTPUT_BYTES);
                return Ok(Config {
                    default_client: env_default_client(),
                    max_tool_output_bytes,
                    ..Config::default()
                });
            }
            Err(e) => return Err(e),
        };

        let default_client = cfg_file
            .default_client
            .map(|s| s.0)
            .filter(|s| !s.is_empty())
            .or_else(env_default_client);

        let stage_clients_raw: Option<IndexMap<String, String>> = cfg_file
            .default_client_by_stage
            .map(|m| m.into_iter().map(|(k, v)| (k, v.0)).collect());

        let task_clients: Option<IndexMap<String, String>> = cfg_file
            .default_task_clients
            .or(cfg_file.task_clients)
            .map(|m| m.into_iter().map(|(k, v)| (k, v.0)).collect());
        let (exact_task_clients, prefix_task_clients) = parse_task_clients(task_clients.as_ref());

        let client_profiles: IndexMap<String, ClientProfile> = cfg_file
            .client_profiles
            .unwrap_or_default()
            .into_iter()
            .map(|(name, profile)| {
                let task_clients = profile
                    .task_clients
                    .map(|m| m.into_iter().map(|(k, v)| (k, v.0)).collect());
                (
                    name,
                    ClientProfile {
                        client: profile.client.map(|s| s.0).unwrap_or_default(),
                        task_clients,
                    },
                )
            })
            .collect();

        // Resolve profile references in stage client specifiers.
        let stage_clients: Option<IndexMap<String, String>> = match stage_clients_raw {
            Some(map) => {
                let resolved: Result<IndexMap<String, String>, String> = map
                    .into_iter()
                    .map(|(k, v)| {
                        let resolved = if let Some(name) = v.strip_prefix("profile:") {
                            let profile = client_profiles.get(name).ok_or_else(|| {
                                format!(
                                    "unknown client profile {name:?} in default-client-by-stage"
                                )
                            })?;
                            if profile.client.trim().is_empty() {
                                return Err(format!("client profile {name:?} has no client"));
                            }
                            profile.client.clone()
                        } else {
                            v
                        };
                        Ok((k, resolved))
                    })
                    .collect();
                Some(resolved.map_err(ConfigError::Profile)?)
            }
            None => None,
        };
        let (exact_stage_clients, prefix_stage_clients) =
            parse_stage_clients(stage_clients.as_ref());

        let path_overrides = cfg_file
            .paths
            .as_ref()
            .map(|m| {
                let string_map: HashMap<String, String> =
                    m.iter().map(|(k, v)| (k.clone(), v.0.clone())).collect();
                parse_path_overrides(&string_map)
            })
            .unwrap_or_default();

        let max_tool_output_bytes = cfg_file
            .max_tool_output_bytes
            .or_else(|| {
                std::env::var("GREMLINS_MAX_TOOL_OUTPUT_BYTES")
                    .ok()
                    .and_then(|v| v.parse().ok())
            })
            .unwrap_or(DEFAULT_MAX_TOOL_OUTPUT_BYTES);

        Ok(Config {
            default_client,
            exact_stage_clients,
            prefix_stage_clients,
            exact_task_clients,
            prefix_task_clients,
            default_task_clients: task_clients,
            client_profiles,
            path_overrides,
            azure_openai: cfg_file.azure_openai,
            max_tool_output_bytes,
        })
    }

    pub fn default_client(&self) -> Option<&str> {
        self.default_client.as_deref()
    }

    /// Returns `(exact_map, prefix_map)` from `default-client-by-stage`.
    pub fn default_client_by_stage(&self) -> (&HashMap<String, String>, &HashMap<String, String>) {
        (&self.exact_stage_clients, &self.prefix_stage_clients)
    }

    /// Returns `(exact_map, prefix_map)` from `task-clients`.
    ///
    /// Exact-map keys are matched by equality; prefix-map keys by
    /// `description.starts_with(prefix)`, longest prefix winning. All matching
    /// is case-insensitive: map keys are lowercased at parse time and the
    /// caller lowercases the `description` it looks up.
    pub fn task_clients(&self) -> (&HashMap<String, String>, &HashMap<String, String>) {
        (&self.exact_task_clients, &self.prefix_task_clients)
    }

    /// The raw `default-task-clients` map, in declaration order.
    ///
    /// Used by stage-level `task-clients` merging at pipeline-load time: a
    /// stage map starts from this and adds its own entries.
    pub fn default_task_clients(&self) -> Option<&IndexMap<String, String>> {
        self.default_task_clients.as_ref()
    }

    /// The flat client-profile lookup table.
    pub fn client_profiles(&self) -> &IndexMap<String, ClientProfile> {
        &self.client_profiles
    }

    pub fn path_overrides(&self) -> &PathOverrides {
        &self.path_overrides
    }

    pub(crate) fn azure_openai(&self) -> Option<&AzureOpenAiSettings> {
        self.azure_openai.as_ref()
    }

    pub fn max_tool_output_bytes(&self) -> u64 {
        self.max_tool_output_bytes
    }

    pub fn overlay_dirname(&self) -> &'static str {
        OVERLAY_DIRNAME
    }
}

// ---------------------------------------------------------------------------
// Stage client parsing
// ---------------------------------------------------------------------------

fn parse_stage_clients(
    map: Option<&IndexMap<String, String>>,
) -> (HashMap<String, String>, HashMap<String, String>) {
    let Some(obj) = map else {
        return (HashMap::new(), HashMap::new());
    };

    let mut exact = HashMap::new();
    let mut prefix = HashMap::new();

    for (key, val_str) in obj {
        if let Some(p) = key.strip_suffix('*') {
            if p.is_empty() {
                warn!(
                    "config key {:?} in default-client-by-stage produces an empty prefix, \
                     which would match every stage — skipping",
                    key
                );
                continue;
            }
            prefix.insert(p.to_string(), val_str.clone());
        } else {
            exact.insert(key.clone(), val_str.clone());
        }
    }

    (exact, prefix)
}

/// Parse the `task-clients` map, which overrides the model used by a Task tool
/// invocation whose `description` matches a key.
///
/// Keys are lowercased here so lookup is case-insensitive; a key ending in `*`
/// denotes a prefix match, anything else an exact match.
/// Parse an already-resolved stage-level `task-clients` map into the
/// `(exact, prefix)` lookup form used by task runners.
pub fn parse_task_clients_map(
    map: Option<&IndexMap<String, String>>,
) -> (HashMap<String, String>, HashMap<String, String>) {
    parse_task_clients(map)
}

fn parse_task_clients(
    map: Option<&IndexMap<String, String>>,
) -> (HashMap<String, String>, HashMap<String, String>) {
    let Some(obj) = map else {
        return (HashMap::new(), HashMap::new());
    };

    let mut exact = HashMap::new();
    let mut prefix = HashMap::new();
    let mut seen = std::collections::HashSet::new();

    for (key, val_str) in obj {
        let normalized = if let Some(p) = key.strip_suffix('*') {
            if p.is_empty() {
                warn!(
                    "config key {:?} in task-clients produces an empty prefix, \
                     which would match every task — skipping",
                    key
                );
                continue;
            }
            p.to_lowercase()
        } else {
            key.to_lowercase()
        };

        if !seen.insert(normalized.clone()) {
            warn!(
                "config key {:?} in task-clients normalizes to {:?}, \
                 which duplicates another key — skipping",
                key, normalized
            );
            continue;
        }

        if key.ends_with('*') {
            prefix.insert(normalized, val_str.clone());
        } else {
            exact.insert(normalized, val_str.clone());
        }
    }

    (exact, prefix)
}

// ---------------------------------------------------------------------------
// YAML parsing
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Yaml(#[from] serde_yaml::Error),
    #[error("profile error: {0}")]
    Profile(String),
}

fn parse_yaml_config(path: &Path) -> Result<ConfigFile, ConfigError> {
    let content = std::fs::read_to_string(path)?;
    let cfg: ConfigFile = serde_yaml::from_str(&content)?;
    Ok(cfg)
}

// ---------------------------------------------------------------------------
// Process-global singleton
// ---------------------------------------------------------------------------

static GLOBAL_CONFIG: Mutex<Option<Arc<Config>>> = Mutex::new(None);

pub fn init_global() -> Result<(), ConfigError> {
    *GLOBAL_CONFIG.lock().unwrap() = Some(Arc::new(Config::load()?));
    Ok(())
}

pub(crate) fn get_global() -> Option<Arc<Config>> {
    GLOBAL_CONFIG.lock().unwrap().clone()
}

/// Get the global config, loading lazily on first access.
pub fn global_config() -> Result<Arc<Config>, ConfigError> {
    let mut guard = GLOBAL_CONFIG.lock().unwrap();
    if let Some(ref cfg) = *guard {
        return Ok(cfg.clone());
    }
    let cfg = Arc::new(Config::load()?);
    *guard = Some(cfg.clone());
    Ok(cfg)
}

pub fn clear_global() {
    *GLOBAL_CONFIG.lock().unwrap() = None;
}

// ---------------------------------------------------------------------------
// Env-var helpers
// ---------------------------------------------------------------------------

fn env_default_client() -> Option<String> {
    std::env::var("GREMLINS_DEFAULT_CLIENT")
        .ok()
        .filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// Client-profile resolution
// ---------------------------------------------------------------------------

/// Resolve a client reference through the flat `client-profiles` table.
///
/// Strings starting with `profile:` are profile lookups; everything else is
/// already a `provider:model[:k=v]` spec and passes through unchanged. An
/// unknown profile is an error.
pub fn resolve_client_reference(spec: &str, config: &Config) -> Result<String, String> {
    let name = match spec.strip_prefix("profile:") {
        Some(name) => name,
        None => return Ok(spec.to_string()),
    };
    let profile = config
        .client_profiles
        .get(name)
        .ok_or_else(|| format!("unknown client profile {name:?}"))?;
    if profile.client.trim().is_empty() {
        return Err(format!("client profile {name:?} has no client"));
    }
    Ok(profile.client.clone())
}

/// Resolve a `client:` / `default-client:` value through the global config.
pub fn resolve_client_with_global(spec: &str) -> Result<String, String> {
    let config = global_config().map_err(|e| e.to_string())?;
    resolve_client_reference(spec, &config)
}

/// Merge a stage-level `task-clients` map over the global
/// `default-task-clients`, and resolve every value through profiles.
///
/// Stage entries win on key conflict; keys not present at the stage level
/// fall back to the global map.
pub fn merge_task_clients(
    stage: Option<&IndexMap<String, String>>,
    global: Option<&IndexMap<String, String>>,
    config: &Config,
) -> Result<Option<IndexMap<String, String>>, String> {
    let mut merged: IndexMap<String, String> = global.cloned().unwrap_or_default();
    if let Some(stage) = stage {
        for (key, value) in stage {
            // Remove case-insensitive duplicate so stage wins
            let key_lower = key.to_lowercase();
            merged.retain(|k, _| k.to_lowercase() != key_lower);
            merged.insert(key.clone(), value.clone());
        }
    }
    if merged.is_empty() {
        return Ok(None);
    }
    let mut resolved = IndexMap::new();
    for (key, value) in &merged {
        resolved.insert(key.clone(), resolve_client_reference(value, config)?);
    }
    Ok(Some(resolved))
}

/// Merge three layers of already-resolved task-client maps at runtime.
///
/// Layer order (later wins on key conflict):
/// 1. Global `default-task-clients` from settings.yaml
/// 2. Enclosing composite stage's effective task-clients
/// 3. Current stage's own `task-clients`
///
/// Returns `None` when all layers are empty.
pub fn merge_task_clients_runtime(
    global: Option<&IndexMap<String, String>>,
    enclosing: Option<&IndexMap<String, String>>,
    stage: Option<&IndexMap<String, String>>,
) -> Option<IndexMap<String, String>> {
    let mut merged: IndexMap<String, String> = IndexMap::new();
    if let Some(g) = global {
        for (k, v) in g {
            merged.insert(k.clone(), v.clone());
        }
    }
    if let Some(e) = enclosing {
        for (k, v) in e {
            merged.insert(k.clone(), v.clone());
        }
    }
    if let Some(s) = stage {
        for (k, v) in s {
            merged.insert(k.clone(), v.clone());
        }
    }
    if merged.is_empty() {
        None
    } else {
        Some(merged)
    }
}

fn sandbox_override(subdir: &str) -> Option<PathBuf> {
    std::env::var("GREMLINS_SANDBOX_ROOT")
        .ok()
        .map(|root| PathBuf::from(root).join(subdir))
}

fn project_root_env_override() -> Option<PathBuf> {
    std::env::var("GREMLINS_PROJECT_ROOT")
        .ok()
        .map(PathBuf::from)
}

fn overlay_dir_env_override() -> Option<PathBuf> {
    std::env::var("GREMLINS_OVERLAY_DIR")
        .ok()
        .map(PathBuf::from)
}

// ---------------------------------------------------------------------------
// Runtime / behavior env-var accessors
// ---------------------------------------------------------------------------

/// GREMLINS_STREAM_IDLE_TIMEOUT in seconds. Default 600.0.
pub(crate) fn stream_idle_timeout() -> f64 {
    std::env::var("GREMLINS_STREAM_IDLE_TIMEOUT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(600.0)
}

/// GREMLINS_AGENT_MAX_TURNS, with GREMLINS_OPENAI_AGENTS_MAX_TURNS as fallback.
/// Default 1000.
pub(crate) fn max_agent_turns() -> usize {
    std::env::var("GREMLINS_AGENT_MAX_TURNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .or_else(|| {
            std::env::var("GREMLINS_OPENAI_AGENTS_MAX_TURNS")
                .ok()
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(1000)
}

/// GREMLINS_REASONING_EFFORT override. None means use provider default.
pub(crate) fn reasoning_effort() -> Option<String> {
    std::env::var("GREMLINS_REASONING_EFFORT").ok()
}

/// Copilot API key: `GITHUB_COPILOT_API_KEY` then `COPILOT_API_KEY`.
pub(crate) fn copilot_api_key() -> Option<String> {
    std::env::var("GITHUB_COPILOT_API_KEY")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("COPILOT_API_KEY")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
}

/// GREMLINS_TELEMETRY — "1" or "true" enables per-turn telemetry logging.
pub(crate) fn telemetry_enabled() -> bool {
    std::env::var("GREMLINS_TELEMETRY")
        .map(|v| v == "1" || v.to_lowercase() == "true")
        .unwrap_or(false)
}

/// GREMLINS_ARTIFACT_REMINDER_BUDGET — how many times to nudge when expected
/// artifacts are missing. Default 3.
pub(crate) fn artifact_reminder_budget() -> usize {
    std::env::var("GREMLINS_ARTIFACT_REMINDER_BUDGET")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3)
}

/// GREMLINS_COMPLETION_NUDGE_BUDGET — how many empty-turn nudges to inject
/// before giving up. Default 11.
pub(crate) fn completion_nudge_budget() -> usize {
    std::env::var("GREMLINS_COMPLETION_NUDGE_BUDGET")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(11)
}

/// Default cap for tool output: 300 000 bytes.
pub(crate) const DEFAULT_MAX_TOOL_OUTPUT_BYTES: u64 = 300_000;

/// GREMLINS_MAX_TOOL_OUTPUT_BYTES — cap tool output at this many bytes.
/// Default 300 000. 0 means no limit. Settings.yaml `max-tool-output-bytes`
/// takes precedence over the env var.
pub(crate) fn max_tool_output_bytes() -> u64 {
    get_global()
        .map(|c| c.max_tool_output_bytes())
        .or_else(|| {
            std::env::var("GREMLINS_MAX_TOOL_OUTPUT_BYTES")
                .ok()
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(DEFAULT_MAX_TOOL_OUTPUT_BYTES)
}

/// GREMLINS_SCRATCH_DIR for tool scratch space. Creates the directory.
pub(crate) fn scratch_dir(gremlin_id: Option<&str>) -> Option<PathBuf> {
    let path: PathBuf = std::env::var("GREMLINS_SCRATCH_DIR")
        .ok()
        .filter(|s| !s.is_empty())?
        .into();
    let p = if let Some(id) = gremlin_id {
        path.join(id)
    } else {
        path
    };
    std::fs::create_dir_all(&p).ok()?;
    Some(p)
}

/// HOME directory with platform fallback.
pub(crate) fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
}

// ---------------------------------------------------------------------------
// Internal path resolvers — pure, no global dependency
// ---------------------------------------------------------------------------

pub fn resolve_user_config_root(overrides: Option<&PathOverrides>) -> PathBuf {
    if let Some(sandbox) = sandbox_override("config") {
        return sandbox;
    }
    if let Some(o) = overrides {
        if let Some(ref p) = o.config_root {
            return p.clone();
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("gremlins")
}

pub fn resolve_state_root(overrides: Option<&PathOverrides>) -> PathBuf {
    if let Some(sandbox) = sandbox_override("state") {
        let p = sandbox;
        std::fs::create_dir_all(&p).ok();
        return p;
    }
    if let Some(o) = overrides {
        if let Some(ref p) = o.state_root {
            std::fs::create_dir_all(p).ok();
            return p.clone();
        }
    }
    let p = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .or_else(dirs::data_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("gremlins");
    std::fs::create_dir_all(&p).ok();
    p
}

pub fn resolve_work_root(overrides: Option<&PathOverrides>) -> PathBuf {
    if let Some(sandbox) = sandbox_override("work") {
        let p = sandbox;
        std::fs::create_dir_all(&p).ok();
        return p;
    }
    if let Some(o) = overrides {
        if let Some(ref p) = o.work_root {
            std::fs::create_dir_all(p).ok();
            return p.clone();
        }
    }
    let p = std::env::temp_dir().join("gremlins");
    std::fs::create_dir_all(&p).ok();
    p
}

pub fn resolve_project_root(overrides: Option<&PathOverrides>) -> PathBuf {
    if let Some(p) = project_root_env_override() {
        return p;
    }
    if let Some(o) = overrides {
        if let Some(ref p) = o.project_root {
            return p.clone();
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// The overlay of `project_root`, with the `paths.overlay-dir` override from
/// settings.yaml (`overrides`) still winning: it is part of the resolved
/// configuration, not a per-process accident.
fn overlay_dir_for(overrides: Option<&PathOverrides>, project_root: &Path) -> PathBuf {
    if let Some(o) = overrides {
        if let Some(ref p) = o.overlay_dir {
            return p.clone();
        }
    }
    project_root.join(OVERLAY_DIRNAME)
}

pub fn resolve_project_overlay_dir(
    overrides: Option<&PathOverrides>,
    project_root: &Path,
) -> PathBuf {
    if let Some(p) = overlay_dir_env_override() {
        return p;
    }
    overlay_dir_for(overrides, project_root)
}

/// The overlay dir for `project_root`, with `explicit` honoured before
/// `GREMLINS_OVERLAY_DIR`.
///
/// A caller that knows which overlay it means — `status` resolving a definition
/// inside the project its state file names — passes it as `explicit`, so the
/// process-wide export a running gremlin carries cannot redirect the lookup.
/// Reading the choice rather than clearing the variable for the duration of a
/// resolution keeps the environment from ever being observed half-swapped, and
/// so needs no lock.
pub(crate) fn overlay_dir_preferring(explicit: Option<&Path>, project_root: &Path) -> PathBuf {
    match explicit {
        Some(p) => p.to_path_buf(),
        None => project_overlay_dir(project_root),
    }
}

/// The configured overlay of `project_root`, ignoring `GREMLINS_OVERLAY_DIR`.
///
/// The process-wide export a running gremlin carries must not redirect a
/// resolution meant for the project a state file names; the settings.yaml
/// override, by contrast, is part of the resolved layout and is honoured.
pub(crate) fn overlay_dir_without_env(project_root: &Path) -> PathBuf {
    let overrides = get_global().map(|c| c.path_overrides().clone());
    overlay_dir_for(overrides.as_ref(), project_root)
}

pub fn resolve_scratch_root(
    overrides: Option<&PathOverrides>,
    gremlin_id: Option<&str>,
) -> PathBuf {
    let sub = gremlin_id.unwrap_or("direct");
    if let Some(sandbox) = sandbox_override("scratch") {
        let p = sandbox.join(sub);
        std::fs::create_dir_all(&p).ok();
        return p;
    }
    if let Some(o) = overrides {
        if let Some(ref base) = o.scratch_root {
            let p = base.join(sub);
            std::fs::create_dir_all(&p).ok();
            return p;
        }
    }
    let p = std::env::temp_dir().join("gremlins-scratch").join(sub);
    std::fs::create_dir_all(&p).ok();
    p
}

// ---------------------------------------------------------------------------
// Public entry points — use the process-global config's overrides
// ---------------------------------------------------------------------------

pub fn state_root() -> PathBuf {
    let overrides = get_global().map(|c| c.path_overrides().clone());
    resolve_state_root(overrides.as_ref())
}

pub fn work_root() -> PathBuf {
    let overrides = get_global().map(|c| c.path_overrides().clone());
    resolve_work_root(overrides.as_ref())
}

pub fn user_config_root() -> PathBuf {
    // Bootstrap: never consult settings.yaml for config-root during load.
    // Post-bootstrap, honour the override.
    let overrides = get_global().map(|c| c.path_overrides().clone());
    resolve_user_config_root(overrides.as_ref())
}

pub fn project_root() -> PathBuf {
    let overrides = get_global().map(|c| c.path_overrides().clone());
    resolve_project_root(overrides.as_ref())
}

pub fn overlay_dirname() -> &'static str {
    OVERLAY_DIRNAME
}

pub fn project_overlay_dir(project_root: &Path) -> PathBuf {
    let overrides = get_global().map(|c| c.path_overrides().clone());
    resolve_project_overlay_dir(overrides.as_ref(), project_root)
}

/// Directories searched for stage-definition .yaml files (e.g. in
/// ``stage-definitions:`` blocks).  Returns overlay ``stages/`` subdirectory.
pub(crate) fn stage_definition_dirs() -> Vec<PathBuf> {
    let overlay = resolve_project_overlay_dir(None, &project_root());
    vec![overlay.join("stages")]
}

pub fn scratch_root(gremlin_id: Option<&str>) -> PathBuf {
    let overrides = get_global().map(|c| c.path_overrides().clone());
    resolve_scratch_root(overrides.as_ref(), gremlin_id)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use crate::test_support::{EnvGuard, Sandbox};

    // -----------------------------------------------------------------------
    // Config parsing tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_config_default_client() {
        let cfg_file: ConfigFile =
            serde_yaml::from_str(r#"default-client: "openai:gpt-4o""#).unwrap();
        assert_eq!(
            cfg_file.default_client.as_ref().map(|s| s.0.as_str()),
            Some("openai:gpt-4o")
        );
    }

    #[test]
    fn test_config_default_client_empty_string() {
        // Config::load filters empty default-client strings.
        let _sandbox = Sandbox::with_config(Some(r#"default-client: """#));
        let cfg = Config::load().unwrap();
        assert!(
            cfg.default_client().is_none(),
            "empty default-client must be filtered out"
        );
    }

    #[test]
    fn test_config_default_client_by_stage() {
        let map: IndexMap<String, String> = [
            (
                "local-review-*".to_string(),
                "openrouter:doomclientv5".to_string(),
            ),
            ("plan-*".to_string(), "openai:gpt-5".to_string()),
        ]
        .into();
        let (exact, prefix) = parse_stage_clients(Some(&map));
        assert!(exact.is_empty());
        assert_eq!(prefix.len(), 2);
        assert_eq!(
            prefix.get("local-review-").unwrap(),
            "openrouter:doomclientv5"
        );
        assert_eq!(prefix.get("plan-").unwrap(), "openai:gpt-5");
    }

    #[test]
    fn test_config_exact_and_prefix() {
        let map: IndexMap<String, String> = [
            ("review".to_string(), "openai:gpt-5".to_string()),
            ("plan-*".to_string(), "openai:gpt-4o".to_string()),
        ]
        .into();
        let (exact, prefix) = parse_stage_clients(Some(&map));
        assert_eq!(exact.get("review").unwrap(), "openai:gpt-5");
        assert_eq!(prefix.get("plan-").unwrap(), "openai:gpt-4o");
    }

    #[test]
    fn test_config_non_string_value_coerced() {
        // StrictString rejects non-string scalars — 42 must not be coerced
        // into "42".
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.yaml");
        fs::write(
            &path,
            r#"default-client-by-stage:
  prefix-*: 42
  valid-*: openrouter:model
"#,
        )
        .unwrap();
        let result = parse_yaml_config(&path);
        assert!(
            result.is_err(),
            "numeric value must be rejected, not coerced to string"
        );
    }

    #[test]
    fn test_config_empty_prefix_star_skipped() {
        let map: IndexMap<String, String> = [
            ("*".to_string(), "openrouter:model".to_string()),
            ("plan-*".to_string(), "openai:gpt-5".to_string()),
        ]
        .into();
        let (exact, prefix) = parse_stage_clients(Some(&map));
        assert!(exact.is_empty());
        assert_eq!(prefix.len(), 1);
        assert_eq!(prefix.get("plan-").unwrap(), "openai:gpt-5");
    }

    #[test]
    fn test_parse_task_clients() {
        // Exact and prefix keys, both normalized to lowercase.
        let map: IndexMap<String, String> = [
            ("Scout".to_string(), "openai:gpt-4o-mini".to_string()),
            ("Implement*".to_string(), "openai:gpt-4o".to_string()),
        ]
        .into();
        let (exact, prefix) = parse_task_clients(Some(&map));
        assert_eq!(exact.len(), 1);
        assert_eq!(exact.get("scout").unwrap(), "openai:gpt-4o-mini");
        assert_eq!(prefix.len(), 1);
        assert_eq!(prefix.get("implement").unwrap(), "openai:gpt-4o");

        // An empty prefix is dropped.
        let map: IndexMap<String, String> = [
            ("*".to_string(), "openai:gpt-4o".to_string()),
            ("ok-*".to_string(), "openai:gpt-5".to_string()),
        ]
        .into();
        let (exact, prefix) = parse_task_clients(Some(&map));
        assert!(exact.is_empty());
        assert_eq!(prefix.len(), 1);
        assert_eq!(prefix.get("ok-").unwrap(), "openai:gpt-5");

        // Case-only duplicates are detected and the later one is skipped.
        let map: IndexMap<String, String> = [
            ("Scout".to_string(), "openai:gpt-4o-mini".to_string()),
            ("scout".to_string(), "openai:gpt-5".to_string()),
        ]
        .into();
        let (exact, _) = parse_task_clients(Some(&map));
        assert_eq!(exact.len(), 1);
        assert_eq!(exact.get("scout").unwrap(), "openai:gpt-4o-mini");

        // Absent key yields empty maps.
        let (exact, prefix) = parse_task_clients(None);
        assert!(exact.is_empty() && prefix.is_empty());
    }

    #[test]
    fn test_config_yaml_decode_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.yaml");
        fs::write(&path, "{bad").unwrap();
        let result = parse_yaml_config(&path);
        assert!(result.is_err());
    }

    #[test]
    fn test_config_not_an_object() {
        // A YAML array cannot be deserialized as ConfigFile — serde_yaml
        // will produce an error.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("array.yaml");
        fs::write(&path, "[1, 2, 3]").unwrap();
        let result = parse_yaml_config(&path);
        assert!(result.is_err(), "YAML array rejected as config");
    }

    #[test]
    fn test_config_file_not_found() {
        let result = parse_yaml_config(Path::new("/nonexistent/settings.yaml"));
        assert!(matches!(result, Err(ConfigError::Io(_))));
    }

    #[test]
    fn test_paths_section_absent() {
        // A real settings.yaml, with no `paths` key: the loader must not invent
        // overrides for a section that is simply absent.
        let _sandbox = Sandbox::with_config(Some(r#"{"default-client": "a:b"}"#));
        let cfg = Config::load().unwrap();
        let overrides = cfg.path_overrides();
        assert!(overrides.state_root.is_none());
        assert!(overrides.work_root.is_none());
    }

    // -----------------------------------------------------------------------
    // Path resolution tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_state_root_env_override() {
        let sandbox = Sandbox::new();
        let result = resolve_state_root(None);
        assert_eq!(result, sandbox.path().join("state"));
        assert!(result.exists());
    }

    #[test]
    fn test_state_root_config_override() {
        let _env = EnvGuard::lock();
        let dir = tempfile::tempdir().unwrap();
        let overrides = PathOverrides {
            state_root: Some(dir.path().join("my-state")),
            ..Default::default()
        };
        let result = resolve_state_root(Some(&overrides));
        assert_eq!(result, dir.path().join("my-state"));
        assert!(result.exists());
    }

    #[test]
    fn test_state_root_default() {
        let _env = EnvGuard::lock();
        let result = resolve_state_root(None);
        // Should be under the platform state dir
        assert!(result.to_str().unwrap().contains("gremlins"));
        assert!(result.exists());
    }

    #[test]
    fn test_project_root_env_override() {
        let mut env = EnvGuard::lock();
        let dir = tempfile::tempdir().unwrap();
        env.set("GREMLINS_PROJECT_ROOT", dir.path());
        let result = resolve_project_root(None);
        assert_eq!(result, dir.path());
    }

    #[test]
    fn test_project_root_config_override() {
        let _env = EnvGuard::lock();
        let dir = tempfile::tempdir().unwrap();
        let overrides = PathOverrides {
            project_root: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        let result = resolve_project_root(Some(&overrides));
        assert_eq!(result, dir.path());
    }

    #[test]
    fn test_project_root_default() {
        let _env = EnvGuard::lock();
        let result = resolve_project_root(None);
        assert_eq!(result, std::env::current_dir().unwrap());
    }

    #[test]
    fn test_work_root_sandbox() {
        let sandbox = Sandbox::new();
        let result = resolve_work_root(None);
        assert_eq!(result, sandbox.path().join("work"));
        assert!(result.exists());
    }

    #[test]
    fn test_work_root_default() {
        let _env = EnvGuard::lock();
        let result = resolve_work_root(None);
        assert!(result.to_str().unwrap().contains("gremlins"));
        assert!(result.exists());
    }

    #[test]
    fn test_user_config_root_sandbox() {
        let sandbox = Sandbox::new();
        let result = resolve_user_config_root(None);
        assert_eq!(result, sandbox.path().join("config"));
    }

    #[test]
    fn test_user_config_root_default() {
        let _env = EnvGuard::lock();
        let result = resolve_user_config_root(None);
        assert!(result.to_str().unwrap().contains("gremlins"));
    }

    #[test]
    fn test_project_overlay_dir_env() {
        let mut env = EnvGuard::lock();
        let dir = tempfile::tempdir().unwrap();
        env.set("GREMLINS_OVERLAY_DIR", dir.path());
        let result = resolve_project_overlay_dir(None, Path::new("/fake/project"));
        assert_eq!(result, dir.path());
    }

    #[test]
    fn test_project_overlay_dir_config() {
        let _env = EnvGuard::lock();
        let dir = tempfile::tempdir().unwrap();
        let overrides = PathOverrides {
            overlay_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        let result = resolve_project_overlay_dir(Some(&overrides), Path::new("/fake/project"));
        assert_eq!(result, dir.path());
    }

    #[test]
    fn test_project_overlay_dir_default() {
        let _env = EnvGuard::lock();
        let result = resolve_project_overlay_dir(None, Path::new("/fake/project"));
        assert_eq!(result, Path::new("/fake/project").join(".gremlins"));
    }

    #[test]
    fn test_overlay_dirname() {
        assert_eq!(overlay_dirname(), ".gremlins");
    }

    #[test]
    fn test_scratch_root_sandbox() {
        let sandbox = Sandbox::new();
        let result = resolve_scratch_root(None, Some("my-gremlin"));
        assert_eq!(result, sandbox.path().join("scratch").join("my-gremlin"));
        assert!(result.exists());
    }

    #[test]
    fn test_scratch_root_default() {
        let _env = EnvGuard::lock();
        let result = resolve_scratch_root(None, Some("my-gremlin"));
        assert!(result.to_str().unwrap().contains("gremlins-scratch"));
        assert!(result.to_str().unwrap().contains("my-gremlin"));
        assert!(result.exists());
    }

    #[test]
    fn test_scratch_root_no_id() {
        let _env = EnvGuard::lock();
        let result = resolve_scratch_root(None, None);
        assert!(result.to_str().unwrap().contains("direct"));
        assert!(result.exists());
    }

    #[test]
    fn test_precedence_env_over_config() {
        let mut env = EnvGuard::lock();
        let env_dir = tempfile::tempdir().unwrap();
        let cfg_dir = tempfile::tempdir().unwrap();
        env.set("GREMLINS_SANDBOX_ROOT", env_dir.path());

        let overrides = PathOverrides {
            state_root: Some(cfg_dir.path().join("cfg-state")),
            ..Default::default()
        };
        let result = resolve_state_root(Some(&overrides));
        // Env var wins
        assert_eq!(result, env_dir.path().join("state"));
    }

    #[test]
    fn test_global_singleton() {
        let _sandbox = Sandbox::new();
        assert!(get_global().is_none());
        init_global().unwrap();
        assert!(get_global().is_some());
        clear_global();
        assert!(get_global().is_none());

        // Lazy load only happens once — values are identical
        let cfg1 = global_config().unwrap();
        let cfg2 = global_config().unwrap();
        assert_eq!(cfg1.default_client(), cfg2.default_client());

        // After clear, a new Arc is created
        clear_global();
        let cfg3 = global_config().unwrap();
        assert!(!Arc::ptr_eq(&cfg1, &cfg3));
    }

    // -----------------------------------------------------------------------
    // Env-var accessor tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_artifact_reminder_budget_default() {
        let mut env = EnvGuard::lock();
        env.remove("GREMLINS_ARTIFACT_REMINDER_BUDGET");
        assert_eq!(artifact_reminder_budget(), 3);
    }

    #[test]
    fn test_artifact_reminder_budget_from_env() {
        let mut env = EnvGuard::lock();
        env.set("GREMLINS_ARTIFACT_REMINDER_BUDGET", "5");
        assert_eq!(artifact_reminder_budget(), 5);
    }

    #[test]
    fn test_completion_nudge_budget_default() {
        let mut env = EnvGuard::lock();
        env.remove("GREMLINS_COMPLETION_NUDGE_BUDGET");
        assert_eq!(completion_nudge_budget(), 11);
    }

    #[test]
    fn test_completion_nudge_budget_from_env() {
        let mut env = EnvGuard::lock();
        env.set("GREMLINS_COMPLETION_NUDGE_BUDGET", "7");
        assert_eq!(completion_nudge_budget(), 7);
    }

    #[test]
    fn test_max_tool_output_bytes_default() {
        let _env = EnvGuard::lock();
        assert_eq!(max_tool_output_bytes(), DEFAULT_MAX_TOOL_OUTPUT_BYTES);
    }

    #[test]
    fn test_max_tool_output_bytes_from_env() {
        let mut env = EnvGuard::lock();
        env.set("GREMLINS_MAX_TOOL_OUTPUT_BYTES", "5000");
        assert_eq!(max_tool_output_bytes(), 5000);
    }

    #[test]
    fn test_max_tool_output_bytes_zero_from_env() {
        let mut env = EnvGuard::lock();
        env.set("GREMLINS_MAX_TOOL_OUTPUT_BYTES", "0");
        assert_eq!(max_tool_output_bytes(), 0);
    }

    #[test]
    fn test_max_tool_output_bytes_from_settings_yaml() {
        let _sandbox = Sandbox::with_config(Some(r#"{"max-tool-output-bytes": 12345}"#));
        let cfg = Config::load().unwrap();
        assert_eq!(cfg.max_tool_output_bytes(), 12345);
    }

    #[test]
    fn test_max_tool_output_bytes_settings_overrides_env() {
        let _sandbox = Sandbox::with_config(Some(r#"{"max-tool-output-bytes": 77777}"#));
        // Env var is set after sandbox creation (sandbox clears it on init).
        // settings.yaml value (77777) must win over the env var (999).
        std::env::set_var("GREMLINS_MAX_TOOL_OUTPUT_BYTES", "999");
        let cfg = Config::load().unwrap();
        assert_eq!(cfg.max_tool_output_bytes(), 77777);
    }

    #[test]
    fn test_max_tool_output_bytes_env_fallback_no_settings_file() {
        let mut env = EnvGuard::lock();
        let dir = tempfile::tempdir().unwrap();
        // No config/settings.yaml — simulates absent settings file.
        env.set("GREMLINS_SANDBOX_ROOT", dir.path());
        env.set("GREMLINS_MAX_TOOL_OUTPUT_BYTES", "4242");
        let cfg = Config::load().unwrap();
        assert_eq!(cfg.max_tool_output_bytes(), 4242);
    }

    #[test]
    fn test_max_tool_output_bytes_default_no_settings_file() {
        let mut env = EnvGuard::lock();
        let dir = tempfile::tempdir().unwrap();
        env.set("GREMLINS_SANDBOX_ROOT", dir.path());
        // No settings.yaml, no env var — should get the 300_000 default.
        let cfg = Config::load().unwrap();
        assert_eq!(cfg.max_tool_output_bytes(), DEFAULT_MAX_TOOL_OUTPUT_BYTES);
    }
}
