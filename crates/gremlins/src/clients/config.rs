use std::collections::HashMap;
use std::path::Path;

use log::warn;
use serde::Deserialize;

use crate::config::StrictString;

// ---------------------------------------------------------------------------
// System prompts
// ---------------------------------------------------------------------------

/// System prompt injected into every agent stage. Carries the tool roster,
/// directory layout, and pragmatic guidance (e.g. delegation policy).
/// Model-specific guidance will need to live here in the future; for now,
/// some opinionated bits are included as a temporary compromise.
pub(crate) fn agent_system_prompt(work_root: &Path, scratch_root: &Path) -> String {
    format!(
        "\
<important>\n\
Think and write in a terse, to-the-point style. Make brief statements that get to the point. Do this for reasoning and writing \
</important>\n\
<tools>\n\
Read (read files), Write (create files), Edit (targeted \
replacements), Grep (regex search), Glob (find files \
by pattern), Bash (shell commands), Task, Done\n\
</tools>\n\
Call Done(summary) alongside your final message when your work is complete. The summary parameter briefly describes what you accomplished.\n\
<directories>\n\
Work root:     {work}\n\
Scratch root:  {scratch}\n\
(use scratch for test cruft and temporary files)\n\
</directories>\
",
        work = work_root.display(),
        scratch = scratch_root.display(),
    )
}

/// System prompt injected into every nested (Task) agent invocation. Omits any
/// delegation guidance — the tool roster is all a child needs.
pub(crate) fn task_system_prompt(work_root: &Path, scratch_root: &Path) -> String {
    format!(
        "\
<tools>\n\
Read (read files), Write (create files), Edit (targeted \
replacements), Grep (regex search), Glob (find files \
by pattern), Bash (shell commands), Task, Done\n\
</tools>\n\
Call Done(summary) alongside your final message when your work is complete. The summary parameter briefly describes what you accomplished.\n\
<directories>\n\
Work root:     {work}\n\
Scratch root:  {scratch}\n\
(use scratch for test cruft and temporary files)\n\
</directories>\
",
        work = work_root.display(),
        scratch = scratch_root.display(),
    )
}

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
#[derive(Debug, Deserialize)]
struct AzureEntry {
    #[serde(default)]
    auth: Option<StrictString>,
    #[serde(default)]
    scope: Option<StrictString>,
}

// ---------------------------------------------------------------------------
// Providers — loaded from providers.yaml
// ---------------------------------------------------------------------------

/// Parsed content of providers.yaml.
#[derive(Debug, Clone, Default)]
pub(crate) struct Providers {
    api_keys: HashMap<String, String>,
    pats: HashMap<String, String>,
    base_urls: HashMap<String, String>,
    tokens: HashMap<String, String>,
    azure_auth_methods: HashMap<String, String>,
    azure_auth_scopes: HashMap<String, String>,
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
    token: Option<StrictString>,
    #[serde(default)]
    azure: Option<AzureEntry>,
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

    /// Get the auth token for a provider name.
    pub(crate) fn token(&self, provider: &str) -> Option<&str> {
        self.tokens
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
}

fn parse_api_keys(path: &Path) -> Result<Providers, ProvidersError> {
    let content = std::fs::read_to_string(path)?;
    let providers_file: ProvidersFile = serde_yaml::from_str(&content)?;
    let mut api_keys = HashMap::new();
    let mut pats = HashMap::new();
    let mut base_urls = HashMap::new();
    let mut tokens = HashMap::new();
    let mut azure_auth_methods = HashMap::new();
    let mut azure_auth_scopes = HashMap::new();
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
        if let Some(token) = v.token.map(|s| s.0).filter(|s| !s.trim().is_empty()) {
            tokens.insert(k.clone(), token);
        }
        if let Some(azure) = v.azure {
            if let Some(auth) = azure.auth.map(|s| s.0).filter(|s| !s.trim().is_empty()) {
                azure_auth_methods.insert(k.clone(), auth);
            }
            if let Some(scope) = azure.scope.map(|s| s.0).filter(|s| !s.trim().is_empty()) {
                azure_auth_scopes.insert(k.clone(), scope);
            }
        }
        if !api_keys.contains_key(&k)
            && !pats.contains_key(&k)
            && !base_urls.contains_key(&k)
            && !tokens.contains_key(&k)
            && !azure_auth_methods.contains_key(&k)
            && !azure_auth_scopes.contains_key(&k)
        {
            warn!("providers.yaml entry {k:?} has no non-empty fields — skipping");
        }
    }
    Ok(Providers {
        api_keys,
        pats,
        base_urls,
        tokens,
        azure_auth_methods,
        azure_auth_scopes,
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
pub(crate) fn api_key(env_var_name: &str, provider_name: &str) -> Option<String> {
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
pub(crate) fn base_url(env_var_name: &str, provider_name: &str, default: &str) -> String {
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

/// Resolve auth token for a provider. Checks env var first, then providers.yaml.
pub(crate) fn auth_token(env_var_name: &str, provider_name: &str) -> Option<String> {
    if let Ok(val) = std::env::var(env_var_name) {
        if !val.trim().is_empty() {
            return Some(val);
        }
    }
    Providers::load()
        .token(provider_name)
        .map(|s| s.to_string())
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
    if let Ok(env_val) = std::env::var(env_var_name) {
        let v = env_val.trim();
        if !v.is_empty() {
            return parse_provider_auth(v);
        }
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
    if let Ok(val) = std::env::var(env_var_name) {
        if !val.trim().is_empty() {
            return val;
        }
    }
    Providers::load()
        .azure_auth_scope(provider_name)
        .map(|s| s.to_string())
        .unwrap_or_else(|| default.to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use crate::test_support::{EnvGuard, Sandbox};

    // -----------------------------------------------------------------------
    // System prompt tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_agent_system_prompt_renders_directory_paths() {
        let prompt = agent_system_prompt(Path::new("/tmp/gremlins"), Path::new("/tmp/scratch"));
        assert!(prompt.contains("/tmp/gremlins"), "must contain work root");
        assert!(prompt.contains("/tmp/scratch"), "must contain scratch root");
    }

    #[test]
    fn test_agent_system_prompt_includes_delegation_policy() {
        let prompt = agent_system_prompt(Path::new("/work"), Path::new("/scratch"));
        assert!(
            prompt.contains("<tools>"),
            "agent prompt must inject delegation policy; got: {prompt}"
        );
    }

    #[test]
    fn test_task_system_prompt_renders_directory_paths() {
        let prompt = task_system_prompt(Path::new("/tmp/gremlins"), Path::new("/tmp/scratch"));
        assert!(prompt.contains("/tmp/gremlins"), "must contain work root");
        assert!(prompt.contains("/tmp/scratch"), "must contain scratch root");
    }

    #[test]
    fn test_task_system_prompt_omits_delegation_guidance() {
        let prompt = task_system_prompt(Path::new("/work"), Path::new("/scratch"));
        assert!(
            !prompt.contains("<important>") && !prompt.contains("<important>"),
            "child prompt must not inject delegation guidance; got: {prompt}"
        );
    }

    #[test]
    fn test_agent_system_prompt_includes_done() {
        let prompt = agent_system_prompt(Path::new("/work"), Path::new("/scratch"));
        assert!(
            prompt.contains("Task, Done"),
            "agent prompt must include Done in tool roster; got: {prompt}"
        );
        assert!(
            prompt.contains("Call Done(summary) alongside your final message"),
            "agent prompt must include Done instruction; got: {prompt}"
        );
    }

    #[test]
    fn test_task_system_prompt_includes_done() {
        let prompt = task_system_prompt(Path::new("/work"), Path::new("/scratch"));
        assert!(
            prompt.contains("Task, Done"),
            "task prompt must include Done in tool roster; got: {prompt}"
        );
        assert!(
            prompt.contains("Call Done(summary) alongside your final message"),
            "task prompt must include Done instruction; got: {prompt}"
        );
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
