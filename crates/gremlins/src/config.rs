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
    azure: Option<AzureConfig>,
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
    azure: Option<AzureConfigFile>,
}

/// Deserialization helper for the `azure` section of settings.yaml.
#[derive(Debug, Deserialize)]
struct AzureConfigFile {
    endpoint: Option<StrictString>,
    #[serde(rename = "api-version")]
    api_version: Option<StrictString>,
    token: Option<StrictString>,
    #[serde(rename = "api-key")]
    api_key: Option<StrictString>,
    #[serde(default, deserialize_with = "deserialize_azure_auth")]
    auth: Option<String>,
}

/// Custom deserializer for `azure.auth`: rejects non-string values and
/// unknown auth-method names at parse time.
fn deserialize_azure_auth<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct AuthVisitor;
    impl<'de> serde::de::Visitor<'de> for AuthVisitor {
        type Value = Option<String>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str(
                "a string: \"client-secret\", \"cli\", \"managed-identity\", or \"default\"",
            )
        }

        fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
            match v.trim() {
                "client-secret" | "cli" | "managed-identity" | "default" => {
                    Ok(Some(v.trim().to_owned()))
                }
                other => Err(serde::de::Error::invalid_value(
                    serde::de::Unexpected::Str(other),
                    &self,
                )),
            }
        }

        fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
            self.visit_str(&v)
        }

        fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Self::Value, E> {
            Err(serde::de::Error::invalid_type(
                serde::de::Unexpected::Bool(v),
                &self,
            ))
        }

        fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
            Err(serde::de::Error::invalid_type(
                serde::de::Unexpected::Signed(v),
                &self,
            ))
        }

        fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
            Err(serde::de::Error::invalid_type(
                serde::de::Unexpected::Unsigned(v),
                &self,
            ))
        }

        fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
            Err(serde::de::Error::invalid_type(
                serde::de::Unexpected::Float(v),
                &self,
            ))
        }
    }

    deserializer.deserialize_any(AuthVisitor)
}

/// Azure backend configuration from settings.yaml.
#[derive(Debug, Clone, Default)]
pub struct AzureConfig {
    pub(crate) endpoint: Option<StrictString>,
    pub(crate) api_version: Option<StrictString>,
    pub(crate) token: Option<StrictString>,
    pub(crate) api_key: Option<StrictString>,
    pub(crate) auth: Option<String>,
}

/// Resolved Azure authentication method.
#[derive(Debug, Clone)]
pub(crate) enum AzureAuthMethod {
    ApiKey(String),
    Token(String),
    ClientSecret,
    Cli,
    ManagedIdentity,
    DefaultAzure,
}

/// Resolve the effective [`AzureAuthMethod`] from config and env vars.
///
/// Precedence:
/// 1. `settings.yaml` `azure.auth` field
/// 2. `GREMLINS_AZURE_AUTH` env var
/// 3. If neither is set, fall back to static credentials: `azure.token` → `azure.api-key`
pub(crate) fn resolve_azure_auth_method() -> Result<AzureAuthMethod, String> {
    // 1. settings.yaml azure.auth
    if let Some(cfg) = get_global() {
        if let Some(azure) = cfg.azure() {
            if let Some(ref auth) = azure.auth {
                let v = auth.trim();
                return parse_auth_method(v);
            }
        }
    }

    // 2. GREMLINS_AZURE_AUTH env var
    if let Ok(env_val) = std::env::var("GREMLINS_AZURE_AUTH") {
        let v = env_val.trim();
        return parse_auth_method(v);
    }

    // 3. Fall back to static credentials: token → api-key
    if let Some(token) = azure_auth_token() {
        return Ok(AzureAuthMethod::Token(token));
    }
    if let Some(key) = azure_api_key() {
        return Ok(AzureAuthMethod::ApiKey(key));
    }

    Err(
        "no credentials for provider 'azure': set GREMLINS_AZURE_AUTH, \
         GREMLINS_AZURE_TOKEN, GREMLINS_AZURE_API_KEY, \
         or add azure.auth / azure.token / azure.api-key in settings.yaml"
            .to_string(),
    )
}

fn parse_auth_method(v: &str) -> Result<AzureAuthMethod, String> {
    match v {
        "client-secret" => Ok(AzureAuthMethod::ClientSecret),
        "cli" => Ok(AzureAuthMethod::Cli),
        "managed-identity" => Ok(AzureAuthMethod::ManagedIdentity),
        "default" => Ok(AzureAuthMethod::DefaultAzure),
        other => Err(format!(
            "unknown azure.auth value {other:?}: expected \"client-secret\", \"cli\", \"managed-identity\", or \"default\"",
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

        let azure = cfg_file.azure.map(|a| AzureConfig {
            endpoint: a.endpoint,
            api_version: a.api_version,
            token: a.token,
            api_key: a.api_key,
            auth: a.auth,
        });

        Ok(Config {
            default_client,
            exact_stage_clients,
            prefix_stage_clients,
            exact_task_clients,
            prefix_task_clients,
            path_overrides,
            azure,
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

    pub fn azure(&self) -> Option<&AzureConfig> {
        self.azure.as_ref()
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

/// GREMLINS_AZURE_ENDPOINT or settings.yaml `azure.endpoint`.
/// Required for the Azure backend.
pub(crate) fn azure_endpoint() -> Option<String> {
    if let Some(cfg) = get_global() {
        if let Some(azure) = cfg.azure() {
            if let Some(ref ep) = azure.endpoint {
                if !ep.0.trim().is_empty() {
                    return Some(ep.0.clone());
                }
            }
        }
    }
    std::env::var("GREMLINS_AZURE_ENDPOINT")
        .ok()
        .filter(|v| !v.trim().is_empty())
}

/// GREMLINS_AZURE_API_VERSION or settings.yaml `azure.api-version`.
/// Defaults to "2024-10-21".
pub(crate) fn azure_api_version() -> String {
    if let Some(cfg) = get_global() {
        if let Some(azure) = cfg.azure() {
            if let Some(ref ver) = azure.api_version {
                if !ver.0.trim().is_empty() {
                    return ver.0.clone();
                }
            }
        }
    }
    std::env::var("GREMLINS_AZURE_API_VERSION")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "2024-10-21".into())
}

/// GREMLINS_AZURE_TOKEN or settings.yaml `azure.token`.
/// Entra ID bearer token for Azure auth.
pub(crate) fn azure_auth_token() -> Option<String> {
    if let Some(cfg) = get_global() {
        if let Some(azure) = cfg.azure() {
            if let Some(ref token) = azure.token {
                if !token.0.trim().is_empty() {
                    return Some(token.0.clone());
                }
            }
        }
    }
    std::env::var("GREMLINS_AZURE_TOKEN")
        .ok()
        .filter(|v| !v.trim().is_empty())
}

/// GREMLINS_AZURE_API_KEY or settings.yaml `azure.api-key`.
pub(crate) fn azure_api_key() -> Option<String> {
    if let Some(cfg) = get_global() {
        if let Some(azure) = cfg.azure() {
            if let Some(ref key) = azure.api_key {
                if !key.0.trim().is_empty() {
                    return Some(key.0.clone());
                }
            }
        }
    }
    std::env::var("GREMLINS_AZURE_API_KEY")
        .ok()
        .filter(|v| !v.trim().is_empty())
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
// ApiKeys — loaded from providers.yaml, not part of Config
// ---------------------------------------------------------------------------

/// Parsed content of providers.yaml.
#[derive(Debug, Clone, Default)]
pub(crate) struct ApiKeys {
    api_keys: HashMap<String, String>,
    pats: HashMap<String, String>,
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
}

impl ApiKeys {
    /// Load from `user_config_root() / "providers.yaml"`.
    pub(crate) fn load() -> Self {
        let path = user_config_root().join("providers.yaml");
        match parse_api_keys(&path) {
            Ok((api_keys, pats)) => ApiKeys { api_keys, pats },
            Err(e) => {
                if !matches!(&e, ApiKeysError::Io(io_err) if io_err.kind() == std::io::ErrorKind::NotFound)
                {
                    warn!("failed to load {}: {e}", path.display());
                }
                ApiKeys::default()
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
}

type ParsedApiKeys = (HashMap<String, String>, HashMap<String, String>);

fn parse_api_keys(path: &Path) -> Result<ParsedApiKeys, ApiKeysError> {
    let content = std::fs::read_to_string(path)?;
    let providers_file: ProvidersFile = serde_yaml::from_str(&content)?;
    let mut api_keys = HashMap::new();
    let mut pats = HashMap::new();
    for (k, v) in providers_file.0 {
        if let Some(api_key) = v.api_key.map(|s| s.0).filter(|s| !s.trim().is_empty()) {
            api_keys.insert(k.clone(), api_key);
        }
        if let Some(pat) = v.pat.map(|s| s.0).filter(|s| !s.trim().is_empty()) {
            pats.insert(k.clone(), pat);
        }
        if !api_keys.contains_key(&k) && !pats.contains_key(&k) {
            warn!("providers.yaml entry {k:?} has no non-empty \"api-key\" or \"pat\" field — skipping");
        }
    }
    Ok((api_keys, pats))
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ApiKeysError {
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
    ApiKeys::load().get(provider_name).map(|s| s.to_string())
}

/// Resolve a PAT (personal access token) for `provider` from
/// `providers.yaml`. Returns None if not set.
pub(crate) fn pat(provider_name: &str) -> Option<String> {
    ApiKeys::load().pat(provider_name).map(|s| s.to_string())
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
    // ApiKeys tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_api_keys_load_missing() {
        let _sandbox = Sandbox::new();
        let keys = ApiKeys::load();
        assert!(keys.get("openai").is_none());
    }

    #[test]
    fn test_api_keys_load_valid() {
        let sandbox = Sandbox::new();
        let config_dir = sandbox.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("providers.yaml"),
            r#"{"openai": {"api-key": "sk-test"}, "xai": {"api-key": "xai-test"}}"#,
        )
        .unwrap();
        let keys = ApiKeys::load();
        assert_eq!(keys.get("openai"), Some("sk-test"));
        assert_eq!(keys.get("xai"), Some("xai-test"));
    }

    #[test]
    fn test_api_keys_object_empty_api_key_ignored() {
        let _sandbox = Sandbox::with_providers(r#"{"openai": {"api-key": ""}}"#);
        let keys = ApiKeys::load();
        assert!(keys.get("openai").is_none());
    }

    #[test]
    fn test_api_keys_object_whitespace_api_key_ignored() {
        let _sandbox = Sandbox::with_providers(r#"{"openai": {"api-key": "   "}}"#);
        let keys = ApiKeys::load();
        assert!(keys.get("openai").is_none());
    }

    #[test]
    fn test_api_keys_object_integer_api_key_rejected() {
        // Non-string api-key values (e.g. integers) must be rejected —
        // YAML type coercion would otherwise turn 42 into "42".
        let _sandbox = Sandbox::with_providers(r#"{"openai": {"api-key": 42}}"#);
        let keys = ApiKeys::load();
        assert!(
            keys.get("openai").is_none(),
            "integer api-key must be rejected"
        );
    }

    #[test]
    fn test_api_keys_malformed_yaml() {
        let _sandbox = Sandbox::with_providers("{bad");
        let keys = ApiKeys::load();
        assert!(keys.get("openai").is_none());
    }

    #[test]
    fn test_api_keys_not_an_object() {
        let _sandbox = Sandbox::with_providers("[1, 2, 3]");
        let keys = ApiKeys::load();
        assert!(keys.get("openai").is_none());
    }

    #[test]
    fn test_api_keys_string_value_ignored() {
        // YAML will reject a string value where a mapping is expected.
        let _sandbox = Sandbox::with_providers(r#"{"openai": "sk-test"}"#);
        let keys = ApiKeys::load();
        assert!(keys.get("openai").is_none());
    }

    #[test]
    fn test_api_keys_object_missing_api_key() {
        let _sandbox = Sandbox::with_providers(r#"{"openai": {}}"#);
        let keys = ApiKeys::load();
        assert!(keys.get("openai").is_none());
    }

    #[test]
    fn test_api_keys_unknown_provider() {
        let _sandbox = Sandbox::with_providers(r#"{"foo": "bar"}"#);
        let keys = ApiKeys::load();
        assert!(keys.get("openai").is_none());
    }

    #[test]
    fn test_api_keys_pat_field() {
        let _sandbox = Sandbox::with_providers(r#"{"copilot": {"pat": "ghp_test_token"}}"#);
        let keys = ApiKeys::load();
        assert!(keys.get("copilot").is_none());
        assert_eq!(keys.pat("copilot"), Some("ghp_test_token"));
    }

    #[test]
    fn test_api_keys_pat_empty_ignored() {
        let _sandbox = Sandbox::with_providers(r#"{"copilot": {"pat": ""}}"#);
        let keys = ApiKeys::load();
        assert!(keys.pat("copilot").is_none());
    }

    #[test]
    fn test_api_keys_pat_whitespace_ignored() {
        let _sandbox = Sandbox::with_providers(r#"{"copilot": {"pat": "   "}}"#);
        let keys = ApiKeys::load();
        assert!(keys.pat("copilot").is_none());
    }

    #[test]
    fn test_api_keys_both_api_key_and_pat() {
        let _sandbox =
            Sandbox::with_providers(r#"{"copilot": {"api-key": "sk-fake", "pat": "ghp_fake"}}"#);
        let keys = ApiKeys::load();
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

    // -----------------------------------------------------------------------
    // Azure config tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_azure_config_deserialization_all_fields() {
        let _sandbox = Sandbox::with_config(Some(
            r#"{"azure": {"endpoint": "https://example.openai.azure.com", "api-version": "2025-01-01", "token": "bearer-token", "api-key": "sk-azure-key"}}"#,
        ));
        let cfg = Config::load().unwrap();
        let azure = cfg.azure().expect("azure section should be present");
        assert_eq!(
            azure.endpoint.as_ref().map(|s| s.0.as_str()),
            Some("https://example.openai.azure.com")
        );
        assert_eq!(
            azure.api_version.as_ref().map(|s| s.0.as_str()),
            Some("2025-01-01")
        );
        assert_eq!(
            azure.token.as_ref().map(|s| s.0.as_str()),
            Some("bearer-token")
        );
        assert_eq!(
            azure.api_key.as_ref().map(|s| s.0.as_str()),
            Some("sk-azure-key")
        );
    }

    #[test]
    fn test_azure_config_fields_default_to_none() {
        let _sandbox = Sandbox::with_config(Some(r#"{"azure": {}}"#));
        let cfg = Config::load().unwrap();
        let azure = cfg.azure().expect("azure section should be present");
        assert!(azure.endpoint.is_none());
        assert!(azure.api_version.is_none());
        assert!(azure.token.is_none());
        assert!(azure.api_key.is_none());
    }

    #[test]
    fn test_azure_config_absent_section() {
        let _sandbox = Sandbox::with_config(Some(r#"{"default-client": "a:b"}"#));
        let cfg = Config::load().unwrap();
        assert!(cfg.azure().is_none());
    }

    #[test]
    fn test_azure_endpoint_settings_yaml_wins_over_env() {
        let mut env = EnvGuard::lock();
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("settings.yaml"),
            r#"{"azure": {"endpoint": "https://yaml.openai.azure.com"}}"#,
        )
        .unwrap();
        env.set("GREMLINS_SANDBOX_ROOT", tmp.path());
        env.set("GREMLINS_AZURE_ENDPOINT", "https://env.openai.azure.com");
        init_global().unwrap();
        assert_eq!(
            azure_endpoint().as_deref(),
            Some("https://yaml.openai.azure.com")
        );
    }

    #[test]
    fn test_azure_endpoint_env_var_fallback() {
        let mut env = EnvGuard::lock();
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("settings.yaml"), r#"{"azure": {}}"#).unwrap();
        env.set("GREMLINS_SANDBOX_ROOT", tmp.path());
        env.set("GREMLINS_AZURE_ENDPOINT", "https://env.openai.azure.com");
        init_global().unwrap();
        assert_eq!(
            azure_endpoint().as_deref(),
            Some("https://env.openai.azure.com")
        );
    }

    #[test]
    fn test_azure_api_key_settings_yaml_wins_over_env() {
        let mut env = EnvGuard::lock();
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("settings.yaml"),
            r#"{"azure": {"api-key": "yaml-key"}}"#,
        )
        .unwrap();
        env.set("GREMLINS_SANDBOX_ROOT", tmp.path());
        env.set("GREMLINS_AZURE_API_KEY", "env-key");
        init_global().unwrap();
        assert_eq!(azure_api_key().as_deref(), Some("yaml-key"));
    }

    #[test]
    fn test_azure_api_key_env_var_fallback() {
        let mut env = EnvGuard::lock();
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("settings.yaml"), r#"{"azure": {}}"#).unwrap();
        env.set("GREMLINS_SANDBOX_ROOT", tmp.path());
        env.set("GREMLINS_AZURE_API_KEY", "env-key");
        init_global().unwrap();
        assert_eq!(azure_api_key().as_deref(), Some("env-key"));
    }

    #[test]
    fn test_azure_api_version_default_fallback() {
        let mut env = EnvGuard::lock();
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("settings.yaml"), r#"{"azure": {}}"#).unwrap();
        env.set("GREMLINS_SANDBOX_ROOT", tmp.path());
        init_global().unwrap();
        assert_eq!(azure_api_version(), "2024-10-21");
    }

    #[test]
    fn test_azure_api_version_settings_yaml_wins_over_env() {
        let mut env = EnvGuard::lock();
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("settings.yaml"),
            r#"{"azure": {"api-version": "2025-01-01"}}"#,
        )
        .unwrap();
        env.set("GREMLINS_SANDBOX_ROOT", tmp.path());
        env.set("GREMLINS_AZURE_API_VERSION", "2025-06-01");
        init_global().unwrap();
        assert_eq!(azure_api_version(), "2025-01-01");
    }

    #[test]
    fn test_azure_token_settings_yaml_wins_over_env() {
        let mut env = EnvGuard::lock();
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("settings.yaml"),
            r#"{"azure": {"token": "yaml-token"}}"#,
        )
        .unwrap();
        env.set("GREMLINS_SANDBOX_ROOT", tmp.path());
        env.set("GREMLINS_AZURE_TOKEN", "env-token");
        init_global().unwrap();
        assert_eq!(azure_auth_token().as_deref(), Some("yaml-token"));
    }

    #[test]
    fn test_azure_token_env_var_fallback() {
        let mut env = EnvGuard::lock();
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("settings.yaml"), r#"{"azure": {}}"#).unwrap();
        env.set("GREMLINS_SANDBOX_ROOT", tmp.path());
        env.set("GREMLINS_AZURE_TOKEN", "env-token");
        init_global().unwrap();
        assert_eq!(azure_auth_token().as_deref(), Some("env-token"));
    }

    // ── legacy AZURE_OPENAI_* namespace is ignored ──────────────────

    #[test]
    fn test_legacy_azure_openai_endpoint_ignored() {
        let mut env = EnvGuard::lock();
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("settings.yaml"), r#"{"azure": {}}"#).unwrap();
        env.set("GREMLINS_SANDBOX_ROOT", tmp.path());
        env.set("AZURE_OPENAI_ENDPOINT", "https://legacy.openai.azure.com");
        init_global().unwrap();
        assert!(
            azure_endpoint().is_none(),
            "legacy AZURE_OPENAI_ENDPOINT must be ignored"
        );
    }

    #[test]
    fn test_legacy_azure_openai_api_key_ignored() {
        let mut env = EnvGuard::lock();
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("settings.yaml"), r#"{"azure": {}}"#).unwrap();
        env.set("GREMLINS_SANDBOX_ROOT", tmp.path());
        env.set("AZURE_OPENAI_API_KEY", "legacy-key");
        init_global().unwrap();
        assert!(
            azure_api_key().is_none(),
            "legacy AZURE_OPENAI_API_KEY must be ignored"
        );
    }

    #[test]
    fn test_legacy_azure_openai_token_ignored() {
        let mut env = EnvGuard::lock();
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("settings.yaml"), r#"{"azure": {}}"#).unwrap();
        env.set("GREMLINS_SANDBOX_ROOT", tmp.path());
        env.set("AZURE_OPENAI_TOKEN", "legacy-bearer");
        init_global().unwrap();
        assert!(
            azure_auth_token().is_none(),
            "legacy AZURE_OPENAI_TOKEN must be ignored"
        );
    }

    #[test]
    fn test_legacy_azure_openai_api_version_ignored() {
        let mut env = EnvGuard::lock();
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("settings.yaml"), r#"{"azure": {}}"#).unwrap();
        env.set("GREMLINS_SANDBOX_ROOT", tmp.path());
        env.set("AZURE_OPENAI_API_VERSION", "2025-06-01");
        init_global().unwrap();
        assert_eq!(
            azure_api_version(),
            "2024-10-21",
            "legacy AZURE_OPENAI_API_VERSION must be ignored; default should prevail"
        );
    }
}
