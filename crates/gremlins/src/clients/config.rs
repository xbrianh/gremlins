use std::collections::HashMap;
use std::path::Path;

use log::warn;
use serde::{Deserialize, Serialize};

use crate::config::StrictString;

// ---------------------------------------------------------------------------
// ProviderAuth
// ---------------------------------------------------------------------------

/// Resolved provider authentication method.
#[derive(Debug, Clone)]
pub(crate) enum ProviderAuth {
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

// ---------------------------------------------------------------------------
// Azure entry (nested under a provider in providers.yaml)
// ---------------------------------------------------------------------------

/// The `azure:` block nested under a provider entry in providers.yaml.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AzureEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auth: Option<StrictString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scope: Option<StrictString>,
}

// ---------------------------------------------------------------------------
// Providers — loaded from providers.yaml
// ---------------------------------------------------------------------------

/// Parsed content of providers.yaml.
#[derive(Debug, Clone, Default)]
pub struct Providers {
    api_keys: HashMap<String, String>,
    base_urls: HashMap<String, String>,
    tokens: HashMap<String, String>,
    endpoints: HashMap<String, String>,
    api_versions: HashMap<String, String>,
    azure_auth_methods: HashMap<String, String>,
    azure_auth_scopes: HashMap<String, String>,
    client_ids: HashMap<String, String>,
    refresh_tokens: HashMap<String, String>,
}

/// Typed structure for providers.yaml — a newtype over the provider map.
///
/// Uses `StrictString` for credential fields so non-string values (e.g.
/// integers) are rejected at the deserialization layer rather than silently
/// coerced by serde_yaml.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct ProvidersFile(pub HashMap<String, ProviderEntry>);

/// Typed structure for a single provider entry in providers.yaml.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct ProviderEntry {
    #[serde(rename = "api-key", default, skip_serializing_if = "Option::is_none")]
    api_key: Option<StrictString>,
    #[serde(rename = "base-url", default, skip_serializing_if = "Option::is_none")]
    base_url: Option<StrictString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    token: Option<StrictString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    endpoint: Option<StrictString>,
    #[serde(
        rename = "api-version",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    api_version: Option<StrictString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    azure: Option<AzureEntry>,
    #[serde(rename = "client-id", default, skip_serializing_if = "Option::is_none")]
    client_id: Option<StrictString>,
    #[serde(
        rename = "refresh-token",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    refresh_token: Option<StrictString>,
}

impl ProviderEntry {
    /// The provider's API key, if set.
    pub fn api_key(&self) -> Option<&str> {
        self.api_key.as_ref().map(|s| s.0.as_str())
    }

    /// The provider's OAuth client id, if set.
    pub fn client_id(&self) -> Option<&str> {
        self.client_id.as_ref().map(|s| s.0.as_str())
    }

    /// The provider's OAuth refresh token, if set.
    pub fn refresh_token(&self) -> Option<&str> {
        self.refresh_token.as_ref().map(|s| s.0.as_str())
    }

    /// The provider's base URL, if set.
    pub fn base_url(&self) -> Option<&str> {
        self.base_url.as_ref().map(|s| s.0.as_str())
    }

    /// Set the provider's API key.
    pub fn set_api_key(&mut self, v: String) {
        self.api_key = Some(StrictString(v));
    }

    /// Set the provider's OAuth client id.
    pub fn set_client_id(&mut self, v: String) {
        self.client_id = Some(StrictString(v));
    }

    /// Set the provider's OAuth refresh token.
    pub fn set_refresh_token(&mut self, v: String) {
        self.refresh_token = Some(StrictString(v));
    }

    /// Clear the provider's OAuth credentials (client id + refresh token).
    pub fn clear_oauth(&mut self) {
        self.client_id = None;
        self.refresh_token = None;
    }

    /// Whether every field is unset.
    pub fn is_empty(&self) -> bool {
        self.api_key.is_none()
            && self.base_url.is_none()
            && self.token.is_none()
            && self.endpoint.is_none()
            && self.api_version.is_none()
            && self.azure.is_none()
            && self.client_id.is_none()
            && self.refresh_token.is_none()
    }
}

impl Providers {
    /// Load from `user_config_root() / "providers.yaml"`.
    pub(crate) fn load() -> Self {
        let path = crate::config::user_config_root().join("providers.yaml");
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

    /// Get the base_url for a provider name.
    pub(crate) fn base_url(&self, provider: &str) -> Option<&str> {
        self.base_urls
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

    /// Get the endpoint for a provider name.
    pub(crate) fn endpoint(&self, provider: &str) -> Option<&str> {
        self.endpoints
            .get(provider)
            .map(|s| s.as_str())
            .filter(|s| !s.trim().is_empty())
    }

    /// Get the api-version for a provider name.
    pub(crate) fn api_version(&self, provider: &str) -> Option<&str> {
        self.api_versions
            .get(provider)
            .map(|s| s.as_str())
            .filter(|s| !s.trim().is_empty())
    }

    /// Get the Azure auth method from the nested `azure.auth` for a provider.
    pub(crate) fn azure_auth_method(&self, provider: &str) -> Option<&str> {
        self.azure_auth_methods
            .get(provider)
            .map(|s| s.as_str())
            .filter(|s| !s.trim().is_empty())
    }

    /// Get the Azure auth scope from the nested `azure.scope` for a provider.
    pub(crate) fn azure_auth_scope(&self, provider: &str) -> Option<&str> {
        self.azure_auth_scopes
            .get(provider)
            .map(|s| s.as_str())
            .filter(|s| !s.trim().is_empty())
    }

    /// Get the OAuth client id for a provider name.
    pub fn client_id(&self, provider: &str) -> Option<&str> {
        self.client_ids
            .get(provider)
            .map(|s| s.as_str())
            .filter(|s| !s.trim().is_empty())
    }

    /// Get the OAuth refresh token for a provider name.
    pub fn refresh_token(&self, provider: &str) -> Option<&str> {
        self.refresh_tokens
            .get(provider)
            .map(|s| s.as_str())
            .filter(|s| !s.trim().is_empty())
    }
}

/// Normalize a [`StrictString`] config value: trim surrounding whitespace
/// and drop values that become empty.
fn normalize(value: Option<StrictString>) -> Option<String> {
    value
        .map(|s| s.0)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Read an env var, trimming surrounding whitespace and ignoring empty values.
fn env_value(env_var_name: &str) -> Option<String> {
    std::env::var(env_var_name)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn parse_api_keys(path: &Path) -> Result<Providers, ProvidersError> {
    let content = std::fs::read_to_string(path)?;
    let providers_file: ProvidersFile = serde_yaml::from_str(&content)?;
    let mut api_keys = HashMap::new();
    let mut base_urls = HashMap::new();
    let mut tokens = HashMap::new();
    let mut endpoints = HashMap::new();
    let mut api_versions = HashMap::new();
    let mut azure_auth_methods = HashMap::new();
    let mut azure_auth_scopes = HashMap::new();
    let mut client_ids = HashMap::new();
    let mut refresh_tokens = HashMap::new();
    for (k, v) in providers_file.0 {
        if let Some(api_key) = normalize(v.api_key) {
            api_keys.insert(k.clone(), api_key);
        }
        if let Some(base_url) = normalize(v.base_url) {
            base_urls.insert(k.clone(), base_url);
        }
        if let Some(token) = normalize(v.token) {
            tokens.insert(k.clone(), token);
        }
        if let Some(endpoint) = normalize(v.endpoint) {
            endpoints.insert(k.clone(), endpoint);
        }
        if let Some(api_version) = normalize(v.api_version) {
            api_versions.insert(k.clone(), api_version);
        }
        if let Some(azure) = v.azure {
            if let Some(auth) = normalize(azure.auth) {
                azure_auth_methods.insert(k.clone(), auth);
            }
            if let Some(scope) = normalize(azure.scope) {
                azure_auth_scopes.insert(k.clone(), scope);
            }
        }
        // Validate: refresh-token without client-id is an error → skip the entry
        let client_id = normalize(v.client_id);
        let refresh_token = normalize(v.refresh_token);
        if refresh_token.is_some() && client_id.is_none() {
            warn!(
                "providers.yaml entry {k:?} has refresh-token without client-id — skipping entry"
            );
            continue;
        }
        if let Some(cid) = client_id {
            client_ids.insert(k.clone(), cid);
        }
        if let Some(rt) = refresh_token {
            refresh_tokens.insert(k.clone(), rt);
        }
        if !api_keys.contains_key(&k)
            && !base_urls.contains_key(&k)
            && !tokens.contains_key(&k)
            && !endpoints.contains_key(&k)
            && !api_versions.contains_key(&k)
            && !azure_auth_methods.contains_key(&k)
            && !azure_auth_scopes.contains_key(&k)
            && !client_ids.contains_key(&k)
            && !refresh_tokens.contains_key(&k)
        {
            warn!("providers.yaml entry {k:?} has no non-empty fields — skipping");
        }
    }
    Ok(Providers {
        api_keys,
        base_urls,
        tokens,
        endpoints,
        api_versions,
        azure_auth_methods,
        azure_auth_scopes,
        client_ids,
        refresh_tokens,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum ProvidersError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Yaml(#[from] serde_yaml::Error),
}

/// Resolve an API key for `provider`. Checks the named env var first,
/// then falls back to `providers.yaml`. Returns None if neither is set.
pub(crate) fn api_key(env_var_name: &str, provider_name: &str) -> Option<String> {
    if let Some(key) = env_value(env_var_name) {
        return Some(key);
    }
    Providers::load()
        .get(provider_name)
        .map(|s| s.trim().to_string())
}

/// Resolve base_url for a provider. Checks env var first, then providers.yaml, then default.
pub(crate) fn base_url(env_var_name: &str, provider_name: &str, default: &str) -> String {
    if let Some(val) = env_value(env_var_name) {
        return val;
    }
    Providers::load()
        .base_url(provider_name)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| default.to_string())
}

/// Resolve auth token for a provider. Checks env var first, then providers.yaml.
pub(crate) fn auth_token(env_var_name: &str, provider_name: &str) -> Option<String> {
    if let Some(val) = env_value(env_var_name) {
        return Some(val);
    }
    Providers::load()
        .token(provider_name)
        .map(|s| s.trim().to_string())
}

/// Resolve endpoint for a provider. Checks env var first, then providers.yaml.
pub(crate) fn endpoint(env_var_name: &str, provider_name: &str) -> Option<String> {
    if let Some(val) = env_value(env_var_name) {
        return Some(val);
    }
    Providers::load()
        .endpoint(provider_name)
        .map(|s| s.trim().to_string())
}

/// Resolve api-version for a provider. Checks env var first, then providers.yaml, then default.
pub(crate) fn api_version(env_var_name: &str, provider_name: &str, default: &str) -> String {
    if let Some(val) = env_value(env_var_name) {
        return val;
    }
    Providers::load()
        .api_version(provider_name)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| default.to_string())
}

/// Resolve the Azure auth method for a provider from the nested `azure:` key.
///
/// Precedence: env var → providers.yaml `provider.azure.auth` → fallback to
/// token → api-key.
pub(crate) fn azure_auth_method(
    env_var_name: &str,
    provider_name: &str,
    token_env_var_name: &str,
    api_key_env_var_name: &str,
) -> Result<ProviderAuth, String> {
    // 1. env var
    if let Some(env_val) = env_value(env_var_name) {
        return parse_provider_auth(&env_val);
    }

    // 2. providers.yaml provider.azure.auth
    if let Some(auth) = Providers::load().azure_auth_method(provider_name) {
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
         or add {provider_name}.azure.auth / {provider_name}.token / {provider_name}.api-key in providers.yaml"
    ))
}

/// Resolve the Azure auth scope for a provider from the nested `azure:` key.
///
/// Precedence: env var → providers.yaml `provider.azure.scope` → default.
pub(crate) fn azure_auth_scope(env_var_name: &str, provider_name: &str, default: &str) -> String {
    if let Some(val) = env_value(env_var_name) {
        return val;
    }
    Providers::load()
        .azure_auth_scope(provider_name)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| default.to_string())
}

// ---------------------------------------------------------------------------
// providers.yaml read/write helpers
// ---------------------------------------------------------------------------

/// Path to `providers.yaml` under the user config root.
pub fn providers_path() -> std::path::PathBuf {
    crate::config::user_config_root().join("providers.yaml")
}

/// Load and parse `providers.yaml`, returning an empty file when it is absent.
pub fn load_providers_file() -> Result<ProvidersFile, ProvidersError> {
    let path = providers_path();
    match std::fs::read_to_string(&path) {
        Ok(content) => Ok(serde_yaml::from_str(&content)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ProvidersFile::default()),
        Err(e) => Err(ProvidersError::Io(e)),
    }
}

/// Serialize `file` and write it to `providers.yaml`, creating the parent dir.
pub fn save_providers_file(file: &ProvidersFile) -> Result<(), ProvidersError> {
    let path = providers_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let yaml = serde_yaml::to_string(file)?;
    std::fs::write(&path, yaml)?;
    Ok(())
}

/// Persist a provider's OAuth refresh token to `providers.yaml`.
pub(crate) fn persist_refresh_token(provider: &str, refresh_token: &str) -> Result<(), String> {
    let path = providers_path();
    let mut file = match load_providers_file() {
        Ok(f) => f,
        Err(e) => return Err(format!("failed to load {}: {e}", path.display())),
    };
    let entry = file.0.entry(provider.to_string()).or_default();
    entry.set_refresh_token(refresh_token.to_string());
    if let Err(e) = save_providers_file(&file) {
        return Err(format!("failed to save {}: {e}", path.display()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{EnvGuard, Sandbox};

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
    fn test_providers_azure_auth_method() {
        let _sandbox = Sandbox::with_providers(
            r#"{"anthropic": {"azure": {"auth": "cli", "scope": "https://example.com/.default"}}}"#,
        );
        let keys = Providers::load();
        assert_eq!(keys.azure_auth_method("anthropic"), Some("cli"));
        assert_eq!(
            keys.azure_auth_scope("anthropic"),
            Some("https://example.com/.default")
        );
    }

    #[test]
    fn test_providers_azure_auth_empty_ignored() {
        let _sandbox =
            Sandbox::with_providers(r#"{"anthropic": {"azure": {"auth": "", "scope": ""}}}"#);
        let keys = Providers::load();
        assert!(keys.azure_auth_method("anthropic").is_none());
        assert!(keys.azure_auth_scope("anthropic").is_none());
    }

    #[test]
    fn test_providers_azure_auth_missing() {
        let _sandbox = Sandbox::with_providers(r#"{"anthropic": {"api-key": "sk-test"}}"#);
        let keys = Providers::load();
        assert!(keys.azure_auth_method("anthropic").is_none());
        assert!(keys.azure_auth_scope("anthropic").is_none());
    }

    #[test]
    fn test_api_key_env_precedence() {
        let mut env = EnvGuard::lock();
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("providers.yaml"),
            r#"{"openai": {"api-key": "from-file"}}"#,
        )
        .unwrap();
        env.set("GREMLINS_SANDBOX_ROOT", dir.path());
        env.set("TEST_API_KEY", "from-env");
        let result = api_key("TEST_API_KEY", "openai");
        assert_eq!(result, Some("from-env".to_string()));
    }

    #[test]
    fn test_api_key_fallback_to_file() {
        let mut env = EnvGuard::lock();
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("providers.yaml"),
            r#"{"openai": {"api-key": "from-file"}}"#,
        )
        .unwrap();
        env.set("GREMLINS_SANDBOX_ROOT", dir.path());
        let result = api_key("TEST_API_KEY_NONEXISTENT", "openai");
        assert_eq!(result, Some("from-file".to_string()));
    }

    #[test]
    fn test_api_key_none_when_missing() {
        let _env = EnvGuard::lock();
        let result = api_key("TEST_API_KEY_NONEXISTENT", "openai");
        assert!(result.is_none());
    }

    #[test]
    fn test_base_url_env_precedence() {
        let mut env = EnvGuard::lock();
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("providers.yaml"),
            r#"{"openai": {"base-url": "https://file.example.com"}}"#,
        )
        .unwrap();
        env.set("GREMLINS_SANDBOX_ROOT", dir.path());
        env.set("TEST_BASE_URL", "https://env.example.com");
        let result = base_url("TEST_BASE_URL", "openai", "https://default.example.com");
        assert_eq!(result, "https://env.example.com");
    }

    #[test]
    fn test_base_url_fallback_to_file() {
        let mut env = EnvGuard::lock();
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("providers.yaml"),
            r#"{"openai": {"base-url": "https://file.example.com"}}"#,
        )
        .unwrap();
        env.set("GREMLINS_SANDBOX_ROOT", dir.path());
        let result = base_url(
            "TEST_BASE_URL_NONEXISTENT",
            "openai",
            "https://default.example.com",
        );
        assert_eq!(result, "https://file.example.com");
    }

    #[test]
    fn test_base_url_fallback_to_default() {
        let _env = EnvGuard::lock();
        let result = base_url(
            "TEST_BASE_URL_NONEXISTENT",
            "openai",
            "https://default.example.com",
        );
        assert_eq!(result, "https://default.example.com");
    }

    #[test]
    fn test_auth_token_env_precedence() {
        let mut env = EnvGuard::lock();
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("providers.yaml"),
            r#"{"anthropic": {"token": "file-token"}}"#,
        )
        .unwrap();
        env.set("GREMLINS_SANDBOX_ROOT", dir.path());
        env.set("TEST_TOKEN", "env-token");
        let result = auth_token("TEST_TOKEN", "anthropic");
        assert_eq!(result, Some("env-token".to_string()));
    }

    #[test]
    fn test_auth_token_fallback_to_file() {
        let mut env = EnvGuard::lock();
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("providers.yaml"),
            r#"{"anthropic": {"token": "file-token"}}"#,
        )
        .unwrap();
        env.set("GREMLINS_SANDBOX_ROOT", dir.path());
        let result = auth_token("TEST_TOKEN_NONEXISTENT", "anthropic");
        assert_eq!(result, Some("file-token".to_string()));
    }

    #[test]
    fn test_auth_token_none_when_missing() {
        let _env = EnvGuard::lock();
        let result = auth_token("TEST_TOKEN_NONEXISTENT", "anthropic");
        assert!(result.is_none());
    }

    #[test]
    fn test_azure_auth_method_env_precedence() {
        let mut env = EnvGuard::lock();
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("providers.yaml"),
            r#"{"anthropic": {"azure": {"auth": "cli"}}}"#,
        )
        .unwrap();
        env.set("GREMLINS_SANDBOX_ROOT", dir.path());
        env.set("TEST_AZURE_AUTH", "managed-identity");
        let result =
            azure_auth_method("TEST_AZURE_AUTH", "anthropic", "TEST_TOKEN", "TEST_API_KEY")
                .unwrap();
        assert!(matches!(result, ProviderAuth::ManagedIdentity));
    }

    #[test]
    fn test_azure_auth_method_fallback_to_file() {
        let mut env = EnvGuard::lock();
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("providers.yaml"),
            r#"{"anthropic": {"azure": {"auth": "client-secret"}}}"#,
        )
        .unwrap();
        env.set("GREMLINS_SANDBOX_ROOT", dir.path());
        let result = azure_auth_method(
            "TEST_AZURE_AUTH_NONEXISTENT",
            "anthropic",
            "TEST_TOKEN_NONEXISTENT",
            "TEST_API_KEY_NONEXISTENT",
        )
        .unwrap();
        assert!(matches!(result, ProviderAuth::ClientSecret));
    }

    #[test]
    fn test_azure_auth_method_fallback_to_token() {
        let mut env = EnvGuard::lock();
        env.set("TEST_TOKEN", "fallback-token");
        let result = azure_auth_method(
            "TEST_AZURE_AUTH_NONEXISTENT",
            "anthropic",
            "TEST_TOKEN",
            "TEST_API_KEY_NONEXISTENT",
        )
        .unwrap();
        assert!(matches!(result, ProviderAuth::Token(t) if t == "fallback-token"));
    }

    #[test]
    fn test_azure_auth_method_fallback_to_token_trims_env_whitespace() {
        let mut env = EnvGuard::lock();
        env.set("TEST_TOKEN", "  fallback-token  ");
        let result = azure_auth_method(
            "TEST_AZURE_AUTH_NONEXISTENT",
            "anthropic",
            "TEST_TOKEN",
            "TEST_API_KEY_NONEXISTENT",
        )
        .unwrap();
        assert!(matches!(result, ProviderAuth::Token(t) if t == "fallback-token"));
    }

    #[test]
    fn test_azure_auth_method_fallback_to_token_trims_yaml_whitespace() {
        let _sandbox = Sandbox::with_providers(r#"{"anthropic": {"token": "  fallback-token  "}}"#);
        let result = azure_auth_method(
            "TEST_AZURE_AUTH_NONEXISTENT",
            "anthropic",
            "TEST_TOKEN_NONEXISTENT",
            "TEST_API_KEY_NONEXISTENT",
        )
        .unwrap();
        assert!(matches!(result, ProviderAuth::Token(t) if t == "fallback-token"));
    }

    #[test]
    fn test_azure_auth_method_fallback_to_api_key() {
        let mut env = EnvGuard::lock();
        env.set("TEST_API_KEY", "fallback-key");
        let result = azure_auth_method(
            "TEST_AZURE_AUTH_NONEXISTENT",
            "anthropic",
            "TEST_TOKEN_NONEXISTENT",
            "TEST_API_KEY",
        )
        .unwrap();
        assert!(matches!(result, ProviderAuth::ApiKey(k) if k == "fallback-key"));
    }

    #[test]
    fn test_azure_auth_method_fallback_to_api_key_trims_env_whitespace() {
        let mut env = EnvGuard::lock();
        env.set("TEST_API_KEY", "  fallback-key  ");
        let result = azure_auth_method(
            "TEST_AZURE_AUTH_NONEXISTENT",
            "anthropic",
            "TEST_TOKEN_NONEXISTENT",
            "TEST_API_KEY",
        )
        .unwrap();
        assert!(matches!(result, ProviderAuth::ApiKey(k) if k == "fallback-key"));
    }

    #[test]
    fn test_azure_auth_method_fallback_to_api_key_trims_yaml_whitespace() {
        let _sandbox = Sandbox::with_providers(r#"{"anthropic": {"api-key": "  fallback-key  "}}"#);
        let result = azure_auth_method(
            "TEST_AZURE_AUTH_NONEXISTENT",
            "anthropic",
            "TEST_TOKEN_NONEXISTENT",
            "TEST_API_KEY_NONEXISTENT",
        )
        .unwrap();
        assert!(matches!(result, ProviderAuth::ApiKey(k) if k == "fallback-key"));
    }

    #[test]
    fn test_azure_auth_method_error_when_nothing_set() {
        let _env = EnvGuard::lock();
        let result = azure_auth_method(
            "TEST_AZURE_AUTH_NONEXISTENT",
            "anthropic",
            "TEST_TOKEN_NONEXISTENT",
            "TEST_API_KEY_NONEXISTENT",
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_azure_auth_scope_env_precedence() {
        let mut env = EnvGuard::lock();
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("providers.yaml"),
            r#"{"anthropic": {"azure": {"scope": "https://file.example.com/.default"}}}"#,
        )
        .unwrap();
        env.set("GREMLINS_SANDBOX_ROOT", dir.path());
        env.set("TEST_AZURE_SCOPE", "https://env.example.com/.default");
        let result = azure_auth_scope(
            "TEST_AZURE_SCOPE",
            "anthropic",
            "https://default.example.com/.default",
        );
        assert_eq!(result, "https://env.example.com/.default");
    }

    #[test]
    fn test_azure_auth_scope_fallback_to_file() {
        let mut env = EnvGuard::lock();
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("providers.yaml"),
            r#"{"anthropic": {"azure": {"scope": "https://file.example.com/.default"}}}"#,
        )
        .unwrap();
        env.set("GREMLINS_SANDBOX_ROOT", dir.path());
        let result = azure_auth_scope(
            "TEST_AZURE_SCOPE_NONEXISTENT",
            "anthropic",
            "https://default.example.com/.default",
        );
        assert_eq!(result, "https://file.example.com/.default");
    }

    #[test]
    fn test_azure_auth_scope_fallback_to_default() {
        let _env = EnvGuard::lock();
        let result = azure_auth_scope(
            "TEST_AZURE_SCOPE_NONEXISTENT",
            "anthropic",
            "https://default.example.com/.default",
        );
        assert_eq!(result, "https://default.example.com/.default");
    }

    #[test]
    fn test_parse_provider_auth_valid() {
        assert!(matches!(
            parse_provider_auth("client-secret").unwrap(),
            ProviderAuth::ClientSecret
        ));
        assert!(matches!(
            parse_provider_auth("cli").unwrap(),
            ProviderAuth::Cli
        ));
        assert!(matches!(
            parse_provider_auth("managed-identity").unwrap(),
            ProviderAuth::ManagedIdentity
        ));
        assert!(matches!(
            parse_provider_auth("default").unwrap(),
            ProviderAuth::DefaultAzure
        ));
    }

    #[test]
    fn test_parse_provider_auth_invalid() {
        assert!(parse_provider_auth("bogus").is_err());
        assert!(parse_provider_auth("").is_err());
    }
}
