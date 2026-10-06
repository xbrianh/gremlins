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

/// Parsed content of settings.yaml.
#[derive(Debug, Clone, Default)]
pub struct Config {
    default_client: Option<String>,
    exact_stage_clients: HashMap<String, String>,
    prefix_stage_clients: HashMap<String, String>,
    exact_task_clients: HashMap<String, String>,
    prefix_task_clients: HashMap<String, String>,
    path_overrides: PathOverrides,
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
    #[serde(rename = "task-clients")]
    task_clients: Option<IndexMap<String, StrictString>>,
    paths: Option<HashMap<String, StrictString>>,
}

/// Resolved provider authentication method.
#[derive(Debug, Clone)]
pub enum ProviderAuth {
    ApiKey(String),
    Token(String),
    ClientSecret,
    Cli,
    ManagedIdentity,
    DefaultAzure,
}

fn parse_provider_auth(v: &str) -> Result<ProviderAuth, String> {
    match v {
        "client-secret" => Ok(ProviderAuth::ClientSecret),
        "cli" => Ok(ProviderAuth::Cli),
        "managed-identity" => Ok(ProviderAuth::ManagedIdentity),
        "default" => Ok(ProviderAuth::DefaultAzure),
        other => Err(format!(
            "unknown auth value {other:?}: expected \"client-secret\", \"cli\", \"managed-identity\", or \"default\"",
        )),
    }
}

impl Config {
    /// Load from `user_config_root(None) / "settings.yaml"`.
    /// Returns `Config::default()` if the file doesn't exist.
    pub fn load() -> Result<Self, ConfigError> {
        let path = resolve_user_config_root(None).join("settings.yaml");
        let cfg_file = match parse_yaml_config(&path) {
            Ok(v) => v,
            Err(ConfigError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Config {
                    default_client: env_default_client(),
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

        let stage_clients: Option<IndexMap<String, String>> = cfg_file
            .default_client_by_stage
            .map(|m| m.into_iter().map(|(k, v)| (k, v.0)).collect());
        let (exact_stage_clients, prefix_stage_clients) =
            parse_stage_clients(stage_clients.as_ref());

        let task_clients: Option<IndexMap<String, String>> = cfg_file
            .task_clients
            .map(|m| m.into_iter().map(|(k, v)| (k, v.0)).collect());
        let (exact_task_clients, prefix_task_clients) = parse_task_clients(task_clients.as_ref());

        let path_overrides = cfg_file
            .paths
            .as_ref()
            .map(|m| {
                let string_map: HashMap<String, String> =
                    m.iter().map(|(k, v)| (k.clone(), v.0.clone())).collect();
                parse_path_overrides(&string_map)
            })
            .unwrap_or_default();

        Ok(Config {
            default_client,
            exact_stage_clients,
            prefix_stage_clients,
            exact_task_clients,
            prefix_task_clients,
            path_overrides,
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

    pub fn path_overrides(&self) -> &PathOverrides {
        &self.path_overrides
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

/// Copilot GitHub token: `COPILOT_GITHUB_ACCESS_TOKEN` then `GITHUB_TOKEN`.
pub(crate) fn copilot_github_token() -> Option<String> {
    std::env::var("COPILOT_GITHUB_ACCESS_TOKEN")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("GITHUB_TOKEN")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
}

/// Copilot OAuth token auto-discovered from the Copilot extension's
/// `apps.json` (e.g. `~/.config/github-copilot/apps.json`).
pub(crate) fn copilot_oauth_token() -> Option<String> {
    // Copilot uses XDG config convention on all platforms.
    let config_dir = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| Some(home_dir().join(".config")))?;
    let apps_path = config_dir.join("github-copilot").join("apps.json");
    let content = std::fs::read_to_string(&apps_path).ok()?;
    let apps: serde_json::Value = serde_json::from_str(&content).ok()?;
    // Return the oauth_token from the first entry.
    apps.as_object()?
        .values()
        .find_map(|v| v.get("oauth_token")?.as_str().map(String::from))
        .filter(|s| !s.trim().is_empty())
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
// Providers — loaded from providers.yaml, not part of Config
// ---------------------------------------------------------------------------

/// Parsed content of providers.yaml.
#[derive(Debug, Clone, Default)]
pub(crate) struct Providers {
    api_keys: HashMap<String, String>,
    pats: HashMap<String, String>,
    base_urls: HashMap<String, String>,
    endpoints: HashMap<String, String>,
    api_versions: HashMap<String, String>,
    tokens: HashMap<String, String>,
    auth_methods: HashMap<String, String>,
    auth_scopes: HashMap<String, String>,
}

/// Typed structure for providers.yaml — a newtype over the provider map.
///
/// Uses `StrictString` for credential fields so non-string values (e.g.
/// integers) are rejected at the deserialization layer rather than silently
/// coerced by serde_yaml.
#[derive(Debug, Deserialize)]
struct ProvidersFile(HashMap<String, ProviderEntry>);

/// Typed structure for a single provider entry in providers.yaml.
#[derive(Debug, Deserialize)]
struct ProviderEntry {
    #[serde(rename = "api-key", default)]
    api_key: Option<StrictString>,
    #[serde(default)]
    pat: Option<StrictString>,
    #[serde(rename = "base-url", default)]
    base_url: Option<StrictString>,
    #[serde(default)]
    endpoint: Option<StrictString>,
    #[serde(rename = "api-version", default)]
    api_version: Option<StrictString>,
    #[serde(default)]
    token: Option<StrictString>,
    #[serde(default)]
    auth: Option<StrictString>,
    #[serde(rename = "auth-scope", default)]
    auth_scope: Option<StrictString>,
}

impl Providers {
    /// Load from `user_config_root() / "providers.yaml"`.
    pub(crate) fn load() -> Self {
        let path = user_config_root().join("providers.yaml");
        match parse_api_keys(&path) {
            Ok(providers) => providers,
            Err(e) => {
                if !matches!(&e, ProvidersError::Io(io_err) if io_err.kind() == std::io::ErrorKind::NotFound)
                {
                    warn!("failed to load {}: {e}", path.display());
                }
                Providers::default()
            }
        }
    }

    /// Get the API key for a provider name (e.g. "openai", "xai").
    pub(crate) fn get(&self, provider: &str) -> Option<&str> {
        self.api_keys
            .get(provider)
            .map(|s| s.as_str())
            .filter(|s| !s.trim().is_empty())
    }

    /// Get the PAT (personal access token) for a provider name.
    pub(crate) fn pat(&self, provider: &str) -> Option<&str> {
        self.pats
            .get(provider)
            .map(|s| s.as_str())
            .filter(|s| !s.trim().is_empty())
    }

    /// Get the base_url for a provider name.
    pub(crate) fn base_url(&self, provider: &str) -> Option<&str> {
        self.base_urls
            .get(provider)
            .map(|s| s.as_str())
            .filter(|s| !s.trim().is_empty())
    }

    /// Get the endpoint for a provider name.
    pub(crate) fn endpoint(&self, provider: &str) -> Option<&str> {
        self.endpoints
            .get(provider)
            .map(|s| s.as_str())
            .filter(|s| !s.trim().is_empty())
    }

    /// Get the api_version for a provider name.
    pub(crate) fn api_version(&self, provider: &str) -> Option<&str> {
        self.api_versions
            .get(provider)
            .map(|s| s.as_str())
            .filter(|s| !s.trim().is_empty())
    }

    /// Get the auth token for a provider name.
    pub(crate) fn token(&self, provider: &str) -> Option<&str> {
        self.tokens
            .get(provider)
            .map(|s| s.as_str())
            .filter(|s| !s.trim().is_empty())
    }

    /// Get the auth method for a provider name.
    pub(crate) fn auth_method(&self, provider: &str) -> Option<&str> {
        self.auth_methods
            .get(provider)
            .map(|s| s.as_str())
            .filter(|s| !s.trim().is_empty())
    }

    /// Get the auth scope for a provider name.
    pub(crate) fn auth_scope(&self, provider: &str) -> Option<&str> {
        self.auth_scopes
            .get(provider)
            .map(|s| s.as_str())
            .filter(|s| !s.trim().is_empty())
    }
}

fn parse_api_keys(path: &Path) -> Result<Providers, ProvidersError> {
    let content = std::fs::read_to_string(path)?;
    let providers_file: ProvidersFile = serde_yaml::from_str(&content)?;
    let mut api_keys = HashMap::new();
    let mut pats = HashMap::new();
    let mut base_urls = HashMap::new();
    let mut endpoints = HashMap::new();
    let mut api_versions = HashMap::new();
    let mut tokens = HashMap::new();
    let mut auth_methods = HashMap::new();
    let mut auth_scopes = HashMap::new();
    for (k, v) in providers_file.0 {
        if let Some(api_key) = v.api_key.map(|s| s.0).filter(|s| !s.trim().is_empty()) {
            api_keys.insert(k.clone(), api_key);
        }
        if let Some(pat) = v.pat.map(|s| s.0).filter(|s| !s.trim().is_empty()) {
            pats.insert(k.clone(), pat);
        }
        if let Some(base_url) = v.base_url.map(|s| s.0).filter(|s| !s.trim().is_empty()) {
            base_urls.insert(k.clone(), base_url);
        }
        if let Some(endpoint) = v.endpoint.map(|s| s.0).filter(|s| !s.trim().is_empty()) {
            endpoints.insert(k.clone(), endpoint);
        }
        if let Some(api_version) = v.api_version.map(|s| s.0).filter(|s| !s.trim().is_empty()) {
            api_versions.insert(k.clone(), api_version);
        }
        if let Some(token) = v.token.map(|s| s.0).filter(|s| !s.trim().is_empty()) {
            tokens.insert(k.clone(), token);
        }
        if let Some(auth) = v.auth.map(|s| s.0).filter(|s| !s.trim().is_empty()) {
            auth_methods.insert(k.clone(), auth);
        }
        if let Some(auth_scope) = v.auth_scope.map(|s| s.0).filter(|s| !s.trim().is_empty()) {
            auth_scopes.insert(k.clone(), auth_scope);
        }
        if !api_keys.contains_key(&k)
            && !pats.contains_key(&k)
            && !base_urls.contains_key(&k)
            && !endpoints.contains_key(&k)
            && !api_versions.contains_key(&k)
            && !tokens.contains_key(&k)
            && !auth_methods.contains_key(&k)
            && !auth_scopes.contains_key(&k)
        {
            warn!("providers.yaml entry {k:?} has no non-empty fields — skipping");
        }
    }
    Ok(Providers {
        api_keys,
        pats,
        base_urls,
        endpoints,
        api_versions,
        tokens,
        auth_methods,
        auth_scopes,
    })
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ProvidersError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Yaml(#[from] serde_yaml::Error),
}

/// Resolve an API key for `provider`. Checks the named env var first,
/// then falls back to `providers.yaml`. Returns None if neither is set.
pub fn api_key(env_var_name: &str, provider_name: &str) -> Option<String> {
    if let Ok(key) = std::env::var(env_var_name) {
        if !key.trim().is_empty() {
            return Some(key);
        }
    }
    Providers::load().get(provider_name).map(|s| s.to_string())
}

/// Resolve a PAT (personal access token) for `provider` from
/// `providers.yaml`. Returns None if not set.
pub(crate) fn pat(provider_name: &str) -> Option<String> {
    Providers::load().pat(provider_name).map(|s| s.to_string())
}

/// Resolve base_url for a provider. Checks env var first, then providers.yaml, then default.
pub fn base_url(env_var_name: &str, provider_name: &str, default: &str) -> String {
    if let Ok(val) = std::env::var(env_var_name) {
        if !val.trim().is_empty() {
            return val;
        }
    }
    Providers::load()
        .base_url(provider_name)
        .map(|s| s.to_string())
        .unwrap_or_else(|| default.to_string())
}

/// Resolve endpoint for a provider. Checks env var first, then providers.yaml.
pub fn endpoint(env_var_name: &str, provider_name: &str) -> Option<String> {
    if let Ok(val) = std::env::var(env_var_name) {
        if !val.trim().is_empty() {
            return Some(val);
        }
    }
    Providers::load()
        .endpoint(provider_name)
        .map(|s| s.to_string())
}

/// Resolve api_version for a provider. Checks env var first, then providers.yaml, then default.
pub fn api_version(env_var_name: &str, provider_name: &str, default: &str) -> String {
    if let Ok(val) = std::env::var(env_var_name) {
        if !val.trim().is_empty() {
            return val;
        }
    }
    Providers::load()
        .api_version(provider_name)
        .map(|s| s.to_string())
        .unwrap_or_else(|| default.to_string())
}

/// Resolve auth token for a provider. Checks env var first, then providers.yaml.
pub fn auth_token(env_var_name: &str, provider_name: &str) -> Option<String> {
    if let Ok(val) = std::env::var(env_var_name) {
        if !val.trim().is_empty() {
            return Some(val);
        }
    }
    Providers::load()
        .token(provider_name)
        .map(|s| s.to_string())
}

/// Resolve auth scope for a provider. Checks env var first, then providers.yaml, then default.
pub fn auth_scope(env_var_name: &str, provider_name: &str, default: &str) -> String {
    if let Ok(val) = std::env::var(env_var_name) {
        if !val.trim().is_empty() {
            return val;
        }
    }
    Providers::load()
        .auth_scope(provider_name)
        .map(|s| s.to_string())
        .unwrap_or_else(|| default.to_string())
}

/// Resolve the auth method for a provider.
/// Precedence: env var → providers.yaml auth → fallback to token → api-key
///
/// `token_env_var_name` and `api_key_env_var_name` are used for the
/// static-credential fallback (step 3).
pub fn auth_method(
    env_var_name: &str,
    provider_name: &str,
    token_env_var_name: &str,
    api_key_env_var_name: &str,
) -> Result<ProviderAuth, String> {
    // 1. env var
    if let Ok(env_val) = std::env::var(env_var_name) {
        let v = env_val.trim();
        if !v.is_empty() {
            return parse_provider_auth(v);
        }
    }

    // 2. providers.yaml provider.auth
    if let Some(auth) = Providers::load().auth_method(provider_name) {
        let v = auth.trim();
        return parse_provider_auth(v);
    }

    // 3. Fall back to token → api-key
    if let Some(token) = auth_token(token_env_var_name, provider_name) {
        return Ok(ProviderAuth::Token(token));
    }
    if let Some(key) = api_key(api_key_env_var_name, provider_name) {
        return Ok(ProviderAuth::ApiKey(key));
    }

    Err(format!(
        "no credentials for provider {provider_name:?}: set {env_var_name}, \
         or add {provider_name}.auth / {provider_name}.token / {provider_name}.api-key in providers.yaml"
    ))
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
        let _env = EnvGuard::lock();
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
    // Providers tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_providers_load_missing() {
        let _sandbox = Sandbox::new();
        let keys = Providers::load();
        assert!(keys.get("openai").is_none());
    }

    #[test]
    fn test_providers_load_valid() {
        let sandbox = Sandbox::new();
        let config_dir = sandbox.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("providers.yaml"),
            r#"{"openai": {"api-key": "sk-test"}, "xai": {"api-key": "xai-test"}}"#,
        )
        .unwrap();
        let keys = Providers::load();
        assert_eq!(keys.get("openai"), Some("sk-test"));
        assert_eq!(keys.get("xai"), Some("xai-test"));
    }

    #[test]
    fn test_providers_object_empty_api_key_ignored() {
        let _sandbox = Sandbox::with_providers(r#"{"openai": {"api-key": ""}}"#);
        let keys = Providers::load();
        assert!(keys.get("openai").is_none());
    }

    #[test]
    fn test_providers_object_whitespace_api_key_ignored() {
        let _sandbox = Sandbox::with_providers(r#"{"openai": {"api-key": "   "}}"#);
        let keys = Providers::load();
        assert!(keys.get("openai").is_none());
    }

    #[test]
    fn test_providers_object_integer_api_key_rejected() {
        // Non-string api-key values (e.g. integers) must be rejected —
        // YAML type coercion would otherwise turn 42 into "42".
        let _sandbox = Sandbox::with_providers(r#"{"openai": {"api-key": 42}}"#);
        let keys = Providers::load();
        assert!(
            keys.get("openai").is_none(),
            "integer api-key must be rejected"
        );
    }

    #[test]
    fn test_providers_malformed_yaml() {
        let _sandbox = Sandbox::with_providers("{bad");
        let keys = Providers::load();
        assert!(keys.get("openai").is_none());
    }

    #[test]
    fn test_providers_not_an_object() {
        let _sandbox = Sandbox::with_providers("[1, 2, 3]");
        let keys = Providers::load();
        assert!(keys.get("openai").is_none());
    }

    #[test]
    fn test_providers_string_value_ignored() {
        // YAML will reject a string value where a mapping is expected.
        let _sandbox = Sandbox::with_providers(r#"{"openai": "sk-test"}"#);
        let keys = Providers::load();
        assert!(keys.get("openai").is_none());
    }

    #[test]
    fn test_providers_object_missing_api_key() {
        let _sandbox = Sandbox::with_providers(r#"{"openai": {}}"#);
        let keys = Providers::load();
        assert!(keys.get("openai").is_none());
    }

    #[test]
    fn test_providers_unknown_provider() {
        let _sandbox = Sandbox::with_providers(r#"{"foo": "bar"}"#);
        let keys = Providers::load();
        assert!(keys.get("openai").is_none());
    }

    #[test]
    fn test_providers_pat_field() {
        let _sandbox = Sandbox::with_providers(r#"{"copilot": {"pat": "ghp_test_token"}}"#);
        let keys = Providers::load();
        assert!(keys.get("copilot").is_none());
        assert_eq!(keys.pat("copilot"), Some("ghp_test_token"));
    }

    #[test]
    fn test_providers_pat_empty_ignored() {
        let _sandbox = Sandbox::with_providers(r#"{"copilot": {"pat": ""}}"#);
        let keys = Providers::load();
        assert!(keys.pat("copilot").is_none());
    }

    #[test]
    fn test_providers_pat_whitespace_ignored() {
        let _sandbox = Sandbox::with_providers(r#"{"copilot": {"pat": "   "}}"#);
        let keys = Providers::load();
        assert!(keys.pat("copilot").is_none());
    }

    #[test]
    fn test_providers_both_api_key_and_pat() {
        let _sandbox =
            Sandbox::with_providers(r#"{"copilot": {"api-key": "sk-fake", "pat": "ghp_fake"}}"#);
        let keys = Providers::load();
        assert_eq!(keys.get("copilot"), Some("sk-fake"));
        assert_eq!(keys.pat("copilot"), Some("ghp_fake"));
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
}
