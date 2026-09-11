use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use log::warn;
use serde_json::Value;

/// Default name of the project-local overlay directory.
pub(crate) const OVERLAY_DIRNAME: &str = ".gremlins";

/// System prompt injected into every agent stage.
pub(crate) const AGENT_SYSTEM_PROMPT: &str = "\
Keep your context lean: delegate every self-contained piece of work to a subagent. Subagents \
have isolated context — they absorb the noise so you don't have to. When you have multiple \
independent tasks, fan them out with the parallel tool. Plan the fan-out before you start; \
parallel work is cheaper than serial drift.\n";

// ---------------------------------------------------------------------------
// Path overrides from config.json "paths" section
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

fn parse_path_overrides(paths: &HashMap<String, Value>) -> PathOverrides {
    fn str_to_path(v: &Value) -> Option<PathBuf> {
        v.as_str().map(PathBuf::from)
    }
    PathOverrides {
        state_root: paths.get("state-root").and_then(str_to_path),
        work_root: paths.get("work-root").and_then(str_to_path),
        config_root: paths.get("config-root").and_then(str_to_path),
        project_root: paths.get("project-root").and_then(str_to_path),
        overlay_dir: paths.get("overlay-dir").and_then(str_to_path),
        scratch_root: paths.get("scratch-root").and_then(str_to_path),
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Parsed content of config.json.
#[derive(Debug, Clone, Default)]
pub struct Config {
    raw: HashMap<String, Value>,
    default_client: Option<String>,
    exact_stage_clients: HashMap<String, String>,
    prefix_stage_clients: HashMap<String, String>,
    path_overrides: PathOverrides,
}

impl Config {
    /// Load from `user_config_root(None) / "config.json"`.
    /// Returns `Config::default()` if the file doesn't exist.
    pub fn load() -> Result<Self, ConfigError> {
        let path = resolve_user_config_root(None).join("config.json");
        let raw = match parse_json_config(&path) {
            Ok(raw) => raw,
            Err(ConfigError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(e) => return Err(e),
        };

        let default_client = raw
            .get("default-client")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);

        let (exact_stage_clients, prefix_stage_clients) = parse_stage_clients(&raw);

        let path_overrides = raw
            .get("paths")
            .and_then(|v| v.as_object())
            .map(|obj| {
                let m: HashMap<String, Value> =
                    obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                parse_path_overrides(&m)
            })
            .unwrap_or_default();

        Ok(Config {
            raw,
            default_client,
            exact_stage_clients,
            prefix_stage_clients,
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

    pub fn raw(&self) -> &HashMap<String, Value> {
        &self.raw
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
    raw: &HashMap<String, Value>,
) -> (HashMap<String, String>, HashMap<String, String>) {
    let obj = match raw
        .get("default-client-by-stage")
        .and_then(|v| v.as_object())
    {
        Some(o) => o,
        None => return (HashMap::new(), HashMap::new()),
    };

    let mut exact = HashMap::new();
    let mut prefix = HashMap::new();

    for (key, value) in obj {
        let val_str = match value.as_str() {
            Some(s) => s,
            None => {
                warn!(
                    "config key {:?} in default-client-by-stage has non-string value {:?} — skipping",
                    key, value
                );
                continue;
            }
        };

        if let Some(p) = key.strip_suffix('*') {
            if p.is_empty() {
                warn!(
                    "config key {:?} in default-client-by-stage produces an empty prefix, \
                     which would match every stage — skipping",
                    key
                );
                continue;
            }
            prefix.insert(p.to_string(), val_str.to_string());
        } else {
            exact.insert(key.clone(), val_str.to_string());
        }
    }

    (exact, prefix)
}

// ---------------------------------------------------------------------------
// JSON parsing
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("config file must contain a JSON object")]
    NotAnObject,
}

fn parse_json_config(path: &Path) -> Result<HashMap<String, Value>, ConfigError> {
    let content = std::fs::read_to_string(path)?;
    let value: Value = serde_json::from_str(&content)?;
    match value {
        Value::Object(map) => Ok(map.into_iter().collect()),
        _ => Err(ConfigError::NotAnObject),
    }
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

/// Ensure the global config has a non-None default_client.  If a real
/// default-client is already set, does nothing.  Otherwise injects the
/// sentinel `cmd:true` so structural validation never fails on a missing
/// client.  Used by `gremlins validate`.
pub fn inject_sentinals() -> Result<(), ConfigError> {
    let mut guard = GLOBAL_CONFIG.lock().unwrap();
    let empty = guard.is_none();
    if empty {
        // No config at all — seed one from disk (or default).
        *guard = Some(Arc::new(Config::load().unwrap_or_default()));
    }
    let cfg = Arc::get_mut(guard.as_mut().unwrap()).expect("inject_sentinals: unique reference");
    if cfg.default_client.is_some() {
        return Ok(());
    }
    cfg.raw.insert(
        "default-client".to_string(),
        serde_json::Value::String("cmd:true".to_string()),
    );
    cfg.default_client = Some("cmd:true".to_string());
    Ok(())
}

// ---------------------------------------------------------------------------
// Env-var helpers
// ---------------------------------------------------------------------------

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

/// GREMLINS_TELEMETRY — "1" or "true" enables per-turn telemetry logging.
pub(crate) fn telemetry_enabled() -> bool {
    std::env::var("GREMLINS_TELEMETRY")
        .map(|v| v == "1" || v.to_lowercase() == "true")
        .unwrap_or(false)
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

pub fn resolve_project_overlay_dir(
    overrides: Option<&PathOverrides>,
    project_root: &Path,
) -> PathBuf {
    if let Some(p) = overlay_dir_env_override() {
        return p;
    }
    if let Some(o) = overrides {
        if let Some(ref p) = o.overlay_dir {
            return p.clone();
        }
    }
    project_root.join(OVERLAY_DIRNAME)
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
    // Bootstrap: never consult config.json for config-root during load.
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
/// ``stage-definitions:`` blocks).  Returns overlay ``stages/`` subdirectory;
/// bundled recipes live in ``assets::RECIPES`` and are resolved separately.
pub(crate) fn stage_definition_dirs() -> Vec<PathBuf> {
    let overlay = resolve_project_overlay_dir(None, &project_root());
    vec![overlay.join("stages")]
}

pub fn scratch_root(gremlin_id: Option<&str>) -> PathBuf {
    let overrides = get_global().map(|c| c.path_overrides().clone());
    resolve_scratch_root(overrides.as_ref(), gremlin_id)
}

// ---------------------------------------------------------------------------
// ApiKeys — loaded from providers.json, not part of Config
// ---------------------------------------------------------------------------

/// Parsed content of providers.json.
#[derive(Debug, Clone, Default)]
pub(crate) struct ApiKeys {
    keys: HashMap<String, String>,
}

impl ApiKeys {
    /// Load from `user_config_root() / "providers.json"`.
    pub(crate) fn load() -> Self {
        let path = user_config_root().join("providers.json");
        match parse_api_keys(&path) {
            Ok(keys) => ApiKeys { keys },
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
        self.keys
            .get(provider)
            .map(|s| s.as_str())
            .filter(|s| !s.trim().is_empty())
    }
}

fn parse_api_keys(path: &Path) -> Result<HashMap<String, String>, ApiKeysError> {
    let content = std::fs::read_to_string(path)?;
    let value: serde_json::Value = serde_json::from_str(&content)?;
    let obj = value.as_object().ok_or(ApiKeysError::NotAnObject)?;
    let mut keys = HashMap::new();
    for (k, v) in obj {
        match v.as_object() {
            Some(obj) => {
                if let Some(api_key) = obj.get("api-key").and_then(|v| v.as_str()) {
                    if !api_key.trim().is_empty() {
                        keys.insert(k.clone(), api_key.to_string());
                    }
                } else {
                    warn!("providers.json entry {k:?} missing string \"api-key\" field — skipping");
                }
            }
            None => {
                warn!(
                    "providers.json entry {k:?} value must be an object with \"api-key\" — skipping"
                );
            }
        }
    }
    Ok(keys)
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ApiKeysError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("providers.json must contain a JSON object")]
    NotAnObject,
}

/// Resolve an API key for `provider`. Checks the named env var first,
/// then falls back to `providers.json`. Returns None if neither is set.
pub fn api_key(env_var_name: &str, provider_name: &str) -> Option<String> {
    if let Ok(key) = std::env::var(env_var_name) {
        if !key.trim().is_empty() {
            return Some(key);
        }
    }
    ApiKeys::load().get(provider_name).map(|s| s.to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex as StdMutex;

    // Serialize env-var tests to prevent races.
    static ENV_MUTEX: StdMutex<()> = StdMutex::new(());

    fn clear_sandbox_env() {
        std::env::remove_var("GREMLINS_SANDBOX_ROOT");
        std::env::remove_var("GREMLINS_PROJECT_ROOT");
        std::env::remove_var("GREMLINS_OVERLAY_DIR");
    }

    // -----------------------------------------------------------------------
    // Config parsing tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_config_default_client() {
        let raw: HashMap<String, Value> =
            serde_json::from_str(r#"{"default-client": "openai:gpt-4o"}"#).unwrap();
        let cfg = Config {
            raw: raw.clone(),
            default_client: raw
                .get("default-client")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from),
            exact_stage_clients: HashMap::new(),
            prefix_stage_clients: HashMap::new(),
            path_overrides: PathOverrides::default(),
        };
        assert_eq!(cfg.default_client(), Some("openai:gpt-4o"));
    }

    #[test]
    fn test_config_default_client_empty_string() {
        let raw: HashMap<String, Value> =
            serde_json::from_str(r#"{"default-client": ""}"#).unwrap();
        let cfg = Config {
            raw: raw.clone(),
            default_client: raw
                .get("default-client")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from),
            exact_stage_clients: HashMap::new(),
            prefix_stage_clients: HashMap::new(),
            path_overrides: PathOverrides::default(),
        };
        assert_eq!(cfg.default_client(), None);
    }

    #[test]
    fn test_config_default_client_by_stage() {
        let raw: HashMap<String, Value> = serde_json::from_str(
            r#"{"default-client-by-stage": {"local-review-*": "openrouter:doomclientv5", "plan-*": "openai:gpt-5"}}"#,
        )
        .unwrap();
        let (exact, prefix) = parse_stage_clients(&raw);
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
        let raw: HashMap<String, Value> = serde_json::from_str(
            r#"{"default-client-by-stage": {"review": "openai:gpt-5", "plan-*": "openai:gpt-4o"}}"#,
        )
        .unwrap();
        let (exact, prefix) = parse_stage_clients(&raw);
        assert_eq!(exact.get("review").unwrap(), "openai:gpt-5");
        assert_eq!(prefix.get("plan-").unwrap(), "openai:gpt-4o");
    }

    #[test]
    fn test_config_non_string_value_skipped() {
        let raw: HashMap<String, Value> = serde_json::from_str(
            r#"{"default-client-by-stage": {"prefix-*": 42, "valid-*": "openrouter:model"}}"#,
        )
        .unwrap();
        let (exact, prefix) = parse_stage_clients(&raw);
        assert!(exact.is_empty());
        assert_eq!(prefix.len(), 1);
        assert_eq!(prefix.get("valid-").unwrap(), "openrouter:model");
    }

    #[test]
    fn test_config_empty_prefix_star_skipped() {
        let raw: HashMap<String, Value> = serde_json::from_str(
            r#"{"default-client-by-stage": {"*": "openrouter:model", "plan-*": "openai:gpt-5"}}"#,
        )
        .unwrap();
        let (exact, prefix) = parse_stage_clients(&raw);
        assert!(exact.is_empty());
        assert_eq!(prefix.len(), 1);
        assert_eq!(prefix.get("plan-").unwrap(), "openai:gpt-5");
    }

    #[test]
    fn test_config_json_decode_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        fs::write(&path, "{bad").unwrap();
        let result = parse_json_config(&path);
        assert!(result.is_err());
    }

    #[test]
    fn test_config_not_an_object() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("array.json");
        fs::write(&path, "[1, 2, 3]").unwrap();
        let result = parse_json_config(&path);
        assert!(matches!(result, Err(ConfigError::NotAnObject)));
    }

    #[test]
    fn test_config_file_not_found() {
        let result = parse_json_config(Path::new("/nonexistent/config.json"));
        assert!(matches!(result, Err(ConfigError::Io(_))));
    }

    #[test]
    fn test_paths_section_absent() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_sandbox_env();
        let dir = tempfile::tempdir().unwrap();
        let config_json = dir.path().join("config.json");
        fs::write(&config_json, r#"{"default-client": "a:b"}"#).unwrap();
        std::env::set_var("GREMLINS_SANDBOX_ROOT", dir.path());
        let cfg = Config::load().unwrap();
        let overrides = cfg.path_overrides();
        assert!(overrides.state_root.is_none());
        assert!(overrides.work_root.is_none());
        clear_sandbox_env();
    }

    // -----------------------------------------------------------------------
    // Path resolution tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_state_root_env_override() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_sandbox_env();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("GREMLINS_SANDBOX_ROOT", dir.path());
        let result = resolve_state_root(None);
        assert_eq!(result, dir.path().join("state"));
        assert!(result.exists());
        clear_sandbox_env();
    }

    #[test]
    fn test_state_root_config_override() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_sandbox_env();
        let dir = tempfile::tempdir().unwrap();
        let overrides = PathOverrides {
            state_root: Some(dir.path().join("my-state")),
            ..Default::default()
        };
        let result = resolve_state_root(Some(&overrides));
        assert_eq!(result, dir.path().join("my-state"));
        assert!(result.exists());
        clear_sandbox_env();
    }

    #[test]
    fn test_state_root_default() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_sandbox_env();
        let result = resolve_state_root(None);
        // Should be under the platform state dir
        assert!(result.to_str().unwrap().contains("gremlins"));
        assert!(result.exists());
    }

    #[test]
    fn test_project_root_env_override() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_sandbox_env();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("GREMLINS_PROJECT_ROOT", dir.path());
        let result = resolve_project_root(None);
        assert_eq!(result, dir.path());
        clear_sandbox_env();
    }

    #[test]
    fn test_project_root_config_override() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_sandbox_env();
        let dir = tempfile::tempdir().unwrap();
        let overrides = PathOverrides {
            project_root: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        let result = resolve_project_root(Some(&overrides));
        assert_eq!(result, dir.path());
        clear_sandbox_env();
    }

    #[test]
    fn test_project_root_default() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_sandbox_env();
        let result = resolve_project_root(None);
        assert_eq!(result, std::env::current_dir().unwrap());
    }

    #[test]
    fn test_work_root_sandbox() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_sandbox_env();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("GREMLINS_SANDBOX_ROOT", dir.path());
        let result = resolve_work_root(None);
        assert_eq!(result, dir.path().join("work"));
        assert!(result.exists());
        clear_sandbox_env();
    }

    #[test]
    fn test_work_root_default() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_sandbox_env();
        let result = resolve_work_root(None);
        assert!(result.to_str().unwrap().contains("gremlins"));
        assert!(result.exists());
    }

    #[test]
    fn test_user_config_root_sandbox() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_sandbox_env();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("GREMLINS_SANDBOX_ROOT", dir.path());
        let result = resolve_user_config_root(None);
        assert_eq!(result, dir.path().join("config"));
        clear_sandbox_env();
    }

    #[test]
    fn test_user_config_root_default() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_sandbox_env();
        let result = resolve_user_config_root(None);
        assert!(result.to_str().unwrap().contains("gremlins"));
    }

    #[test]
    fn test_project_overlay_dir_env() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_sandbox_env();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("GREMLINS_OVERLAY_DIR", dir.path());
        let result = resolve_project_overlay_dir(None, Path::new("/fake/project"));
        assert_eq!(result, dir.path());
        clear_sandbox_env();
    }

    #[test]
    fn test_project_overlay_dir_config() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_sandbox_env();
        let dir = tempfile::tempdir().unwrap();
        let overrides = PathOverrides {
            overlay_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        let result = resolve_project_overlay_dir(Some(&overrides), Path::new("/fake/project"));
        assert_eq!(result, dir.path());
        clear_sandbox_env();
    }

    #[test]
    fn test_project_overlay_dir_default() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_sandbox_env();
        let result = resolve_project_overlay_dir(None, Path::new("/fake/project"));
        assert_eq!(result, Path::new("/fake/project").join(".gremlins"));
    }

    #[test]
    fn test_overlay_dirname() {
        assert_eq!(overlay_dirname(), ".gremlins");
    }

    #[test]
    fn test_scratch_root_sandbox() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_sandbox_env();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("GREMLINS_SANDBOX_ROOT", dir.path());
        let result = resolve_scratch_root(None, Some("my-gremlin"));
        assert_eq!(result, dir.path().join("scratch").join("my-gremlin"));
        assert!(result.exists());
        clear_sandbox_env();
    }

    #[test]
    fn test_scratch_root_default() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_sandbox_env();
        let result = resolve_scratch_root(None, Some("my-gremlin"));
        assert!(result.to_str().unwrap().contains("gremlins-scratch"));
        assert!(result.to_str().unwrap().contains("my-gremlin"));
        assert!(result.exists());
    }

    #[test]
    fn test_scratch_root_no_id() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_sandbox_env();
        let result = resolve_scratch_root(None, None);
        assert!(result.to_str().unwrap().contains("direct"));
        assert!(result.exists());
    }

    #[test]
    fn test_precedence_env_over_config() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_sandbox_env();
        let env_dir = tempfile::tempdir().unwrap();
        let cfg_dir = tempfile::tempdir().unwrap();
        std::env::set_var("GREMLINS_SANDBOX_ROOT", env_dir.path());

        let overrides = PathOverrides {
            state_root: Some(cfg_dir.path().join("cfg-state")),
            ..Default::default()
        };
        let result = resolve_state_root(Some(&overrides));
        // Env var wins
        assert_eq!(result, env_dir.path().join("state"));
        clear_sandbox_env();
    }

    #[test]
    fn test_global_singleton() {
        // Init / clear
        clear_global();
        assert!(get_global().is_none());
        init_global().unwrap();
        assert!(get_global().is_some());
        clear_global();
        assert!(get_global().is_none());

        // Lazy load only happens once — values are identical
        let cfg1 = global_config().unwrap();
        let cfg2 = global_config().unwrap();
        assert_eq!(cfg1.raw(), cfg2.raw());

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
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_sandbox_env();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("GREMLINS_SANDBOX_ROOT", dir.path());
        let keys = ApiKeys::load();
        assert!(keys.get("openai").is_none());
        clear_sandbox_env();
    }

    #[test]
    fn test_api_keys_load_valid() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_sandbox_env();
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("providers.json"),
            r#"{"openai": {"api-key": "sk-test"}, "xai": {"api-key": "xai-test"}}"#,
        )
        .unwrap();
        std::env::set_var("GREMLINS_SANDBOX_ROOT", dir.path());
        let keys = ApiKeys::load();
        assert_eq!(keys.get("openai"), Some("sk-test"));
        assert_eq!(keys.get("xai"), Some("xai-test"));
        clear_sandbox_env();
    }

    #[test]
    fn test_api_keys_object_empty_api_key_ignored() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_sandbox_env();
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("providers.json"),
            r#"{"openai": {"api-key": ""}}"#,
        )
        .unwrap();
        std::env::set_var("GREMLINS_SANDBOX_ROOT", dir.path());
        let keys = ApiKeys::load();
        assert!(keys.get("openai").is_none());
        clear_sandbox_env();
    }

    #[test]
    fn test_api_keys_object_whitespace_api_key_ignored() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_sandbox_env();
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("providers.json"),
            r#"{"openai": {"api-key": "   "}}"#,
        )
        .unwrap();
        std::env::set_var("GREMLINS_SANDBOX_ROOT", dir.path());
        let keys = ApiKeys::load();
        assert!(keys.get("openai").is_none());
        clear_sandbox_env();
    }

    #[test]
    fn test_api_keys_object_non_string_api_key_ignored() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_sandbox_env();
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("providers.json"),
            r#"{"openai": {"api-key": 42}}"#,
        )
        .unwrap();
        std::env::set_var("GREMLINS_SANDBOX_ROOT", dir.path());
        let keys = ApiKeys::load();
        assert!(keys.get("openai").is_none());
        clear_sandbox_env();
    }

    #[test]
    fn test_api_keys_malformed_json() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_sandbox_env();
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("providers.json"), "{bad").unwrap();
        std::env::set_var("GREMLINS_SANDBOX_ROOT", dir.path());
        let keys = ApiKeys::load();
        assert!(keys.get("openai").is_none());
        clear_sandbox_env();
    }

    #[test]
    fn test_api_keys_not_an_object() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_sandbox_env();
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("providers.json"), "[1, 2, 3]").unwrap();
        std::env::set_var("GREMLINS_SANDBOX_ROOT", dir.path());
        let keys = ApiKeys::load();
        assert!(keys.get("openai").is_none());
        clear_sandbox_env();
    }

    #[test]
    fn test_api_keys_string_value_ignored() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_sandbox_env();
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("providers.json"),
            r#"{"openai": "sk-test"}"#,
        )
        .unwrap();
        std::env::set_var("GREMLINS_SANDBOX_ROOT", dir.path());
        let keys = ApiKeys::load();
        assert!(keys.get("openai").is_none());
        clear_sandbox_env();
    }

    #[test]
    fn test_api_keys_object_missing_api_key() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_sandbox_env();
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("providers.json"), r#"{"openai": {}}"#).unwrap();
        std::env::set_var("GREMLINS_SANDBOX_ROOT", dir.path());
        let keys = ApiKeys::load();
        assert!(keys.get("openai").is_none());
        clear_sandbox_env();
    }

    #[test]
    fn test_api_keys_unknown_provider() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_sandbox_env();
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("providers.json"), r#"{"foo": "bar"}"#).unwrap();
        std::env::set_var("GREMLINS_SANDBOX_ROOT", dir.path());
        let keys = ApiKeys::load();
        assert!(keys.get("openai").is_none());
        clear_sandbox_env();
    }
}
