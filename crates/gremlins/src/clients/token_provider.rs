use async_trait::async_trait;
use azure_core::credentials::{Secret, TokenCredential};
use std::sync::{Arc, Mutex};

// ── TokenProviderError ────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub(crate) enum TokenProviderError {
    #[error("provider error: {0}")]
    Provider(String),
    #[error("credential error: {0}")]
    Credential(String),
    #[error("{0}")]
    Other(String),
}

// ── TokenProvider trait ───────────────────────────────────────────────────

/// Abstraction over Azure identity credential types.
///
/// Written as if it were a `rig-core` submodule so it could be upstreamed.
#[async_trait]
pub(crate) trait TokenProvider: Send + Sync {
    /// Acquire a bare bearer token for `scope`.
    async fn get_token(&self, scope: &str) -> Result<String, TokenProviderError>;
}

// ── ClientSecretProvider ──────────────────────────────────────────────────

/// Service-principal auth via `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET`,
/// `AZURE_TENANT_ID`.
///
/// Environment-variable validation is deferred to [`get_token`] so that
/// configuration errors surface at first use rather than at startup.
/// The credential is lazily initialised and cached so the Azure SDK's
/// internal token cache is reused across calls.
pub(crate) struct ClientSecretProvider {
    credential: Mutex<Option<Arc<azure_identity::ClientSecretCredential>>>,
}

impl ClientSecretProvider {
    pub(crate) fn new() -> Self {
        Self {
            credential: Mutex::new(None),
        }
    }

    fn build_credential(
        &self,
    ) -> Result<Arc<azure_identity::ClientSecretCredential>, TokenProviderError> {
        let tenant_id = std::env::var("AZURE_TENANT_ID")
            .map_err(|_| TokenProviderError::Provider("AZURE_TENANT_ID not set".into()))?;
        let client_id = std::env::var("AZURE_CLIENT_ID")
            .map_err(|_| TokenProviderError::Provider("AZURE_CLIENT_ID not set".into()))?;
        let client_secret = std::env::var("AZURE_CLIENT_SECRET")
            .map_err(|_| TokenProviderError::Provider("AZURE_CLIENT_SECRET not set".into()))?;
        azure_identity::ClientSecretCredential::new(
            &tenant_id,
            client_id,
            Secret::new(client_secret),
            None,
        )
        .map_err(|e| TokenProviderError::Credential(e.to_string()))
    }

    fn get_or_init_credential(
        &self,
    ) -> Result<Arc<azure_identity::ClientSecretCredential>, TokenProviderError> {
        let mut guard = self.credential.lock().unwrap();
        if let Some(ref cred) = *guard {
            return Ok(Arc::clone(cred));
        }
        let cred = self.build_credential()?;
        *guard = Some(Arc::clone(&cred));
        Ok(cred)
    }
}

#[async_trait]
impl TokenProvider for ClientSecretProvider {
    async fn get_token(&self, scope: &str) -> Result<String, TokenProviderError> {
        let credential = self.get_or_init_credential()?;
        let token_response = credential
            .get_token(&[scope], None)
            .await
            .map_err(|e| TokenProviderError::Credential(e.to_string()))?;
        Ok(token_response.token.secret().to_string())
    }
}

// ── AzureCliProvider ──────────────────────────────────────────────────────

/// Auth via the Azure CLI (`az account get-access-token`).
///
/// The credential is lazily initialised and cached so the Azure SDK's
/// internal token cache is reused across calls.
pub(crate) struct AzureCliProvider {
    credential: Mutex<Option<Arc<azure_identity::AzureCliCredential>>>,
}

impl AzureCliProvider {
    pub(crate) fn new() -> Self {
        Self {
            credential: Mutex::new(None),
        }
    }

    fn get_or_init_credential(
        &self,
    ) -> Result<Arc<azure_identity::AzureCliCredential>, TokenProviderError> {
        let mut guard = self.credential.lock().unwrap();
        if let Some(ref cred) = *guard {
            return Ok(Arc::clone(cred));
        }
        let cred = azure_identity::AzureCliCredential::new(None)
            .map_err(|e| TokenProviderError::Credential(e.to_string()))?;
        *guard = Some(Arc::clone(&cred));
        Ok(cred)
    }
}

#[async_trait]
impl TokenProvider for AzureCliProvider {
    async fn get_token(&self, scope: &str) -> Result<String, TokenProviderError> {
        let credential = self.get_or_init_credential()?;
        let token_response = credential
            .get_token(&[scope], None)
            .await
            .map_err(|e| TokenProviderError::Credential(e.to_string()))?;
        Ok(token_response.token.secret().to_string())
    }
}

// ── ManagedIdentityProvider ───────────────────────────────────────────────

/// Auth via Azure managed identity (IMDS endpoint).
///
/// The credential is lazily initialised and cached so the Azure SDK's
/// internal token cache is reused across calls.
pub(crate) struct ManagedIdentityProvider {
    credential: Mutex<Option<Arc<azure_identity::ManagedIdentityCredential>>>,
}

impl ManagedIdentityProvider {
    pub(crate) fn new() -> Self {
        Self {
            credential: Mutex::new(None),
        }
    }

    fn get_or_init_credential(
        &self,
    ) -> Result<Arc<azure_identity::ManagedIdentityCredential>, TokenProviderError> {
        let mut guard = self.credential.lock().unwrap();
        if let Some(ref cred) = *guard {
            return Ok(Arc::clone(cred));
        }
        let cred = azure_identity::ManagedIdentityCredential::new(None)
            .map_err(|e| TokenProviderError::Credential(e.to_string()))?;
        *guard = Some(Arc::clone(&cred));
        Ok(cred)
    }
}

#[async_trait]
impl TokenProvider for ManagedIdentityProvider {
    async fn get_token(&self, scope: &str) -> Result<String, TokenProviderError> {
        let credential = self.get_or_init_credential()?;
        let token_response = credential
            .get_token(&[scope], None)
            .await
            .map_err(|e| TokenProviderError::Credential(e.to_string()))?;
        Ok(token_response.token.secret().to_string())
    }
}

// ── DefaultAzureProvider ──────────────────────────────────────────────────

/// Chains `ClientSecretProvider` → `AzureCliProvider` →
/// `ManagedIdentityProvider`, matching Azure SDK precedence.
///
/// Stops at the first successful token.  Does **not** fall back to static
/// credentials — if all three identity providers fail it returns an error.
///
/// The provider list is injectable via [`DefaultAzureProvider::with_providers`]
/// so tests can exercise the real chaining logic with mock providers.
pub(crate) struct DefaultAzureProvider {
    providers: Vec<Box<dyn TokenProvider>>,
}

impl DefaultAzureProvider {
    /// Create with the standard Azure SDK precedence order.
    pub(crate) fn new() -> Self {
        Self {
            providers: vec![
                Box::new(ClientSecretProvider::new()),
                Box::new(AzureCliProvider::new()),
                Box::new(ManagedIdentityProvider::new()),
            ],
        }
    }

    /// Create with an explicit ordered provider list (for testing).
    #[allow(dead_code)]
    pub(crate) fn with_providers(providers: Vec<Box<dyn TokenProvider>>) -> Self {
        Self { providers }
    }
}

#[async_trait]
impl TokenProvider for DefaultAzureProvider {
    async fn get_token(&self, scope: &str) -> Result<String, TokenProviderError> {
        let mut last_err: Option<TokenProviderError> = None;
        for provider in &self.providers {
            match provider.get_token(scope).await {
                Ok(token) => return Ok(token),
                Err(e) => {
                    last_err = Some(e);
                    continue;
                }
            }
        }
        Err(last_err.unwrap_or_else(|| {
            TokenProviderError::Other("no identity providers configured".into())
        }))
    }
}

// ── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::EnvGuard;
    use std::sync::Mutex;

    // ── mock provider for chaining tests ──────────────────────────────

    struct MockProvider {
        name: &'static str,
        results: Mutex<Vec<Result<String, TokenProviderError>>>,
    }

    impl MockProvider {
        fn new(name: &'static str, results: Vec<Result<String, TokenProviderError>>) -> Self {
            Self {
                name,
                results: Mutex::new(results),
            }
        }
    }

    #[async_trait]
    impl TokenProvider for MockProvider {
        async fn get_token(&self, _scope: &str) -> Result<String, TokenProviderError> {
            let mut results = self.results.lock().unwrap();
            if results.is_empty() {
                return Err(TokenProviderError::Other(format!(
                    "{} exhausted",
                    self.name
                )));
            }
            results.remove(0)
        }
    }

    // ── DefaultAzureProvider chaining ─────────────────────────────────

    #[tokio::test]
    async fn default_azure_stops_at_first_success() {
        let provider = DefaultAzureProvider::with_providers(vec![
            Box::new(MockProvider::new("first", vec![Ok("token-1".into())])),
            Box::new(MockProvider::new("second", vec![Ok("token-2".into())])),
        ]);
        let token = provider.get_token("test-scope").await.unwrap();
        assert_eq!(token, "token-1");
    }

    #[tokio::test]
    async fn default_azure_falls_through_on_error() {
        let provider = DefaultAzureProvider::with_providers(vec![
            Box::new(MockProvider::new(
                "first",
                vec![Err(TokenProviderError::Credential("fail".into()))],
            )),
            Box::new(MockProvider::new("second", vec![Ok("token-2".into())])),
        ]);
        let token = provider.get_token("test-scope").await.unwrap();
        assert_eq!(token, "token-2");
    }

    #[tokio::test]
    async fn default_azure_errors_when_all_fail() {
        let provider = DefaultAzureProvider::with_providers(vec![
            Box::new(MockProvider::new(
                "first",
                vec![Err(TokenProviderError::Credential("fail-1".into()))],
            )),
            Box::new(MockProvider::new(
                "second",
                vec![Err(TokenProviderError::Credential("fail-2".into()))],
            )),
        ]);
        let err = provider.get_token("test-scope").await.unwrap_err();
        assert!(
            matches!(err, TokenProviderError::Credential(_)),
            "should return last error"
        );
    }

    #[tokio::test]
    async fn default_azure_empty_providers_is_error() {
        let provider = DefaultAzureProvider::with_providers(vec![]);
        let err = provider.get_token("test-scope").await.unwrap_err();
        assert!(
            matches!(err, TokenProviderError::Other(_)),
            "empty providers should be an error"
        );
    }

    // ── ClientSecretProvider env-var checks ───────────────────────────

    #[tokio::test]
    async fn client_secret_missing_env_vars() {
        let mut guard = EnvGuard::lock();
        guard.remove("AZURE_TENANT_ID");
        guard.remove("AZURE_CLIENT_ID");
        guard.remove("AZURE_CLIENT_SECRET");
        let provider = ClientSecretProvider::new();
        let result = provider.get_token("test-scope").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn client_secret_partial_env_vars() {
        let mut guard = EnvGuard::lock();
        guard.remove("AZURE_TENANT_ID");
        guard.remove("AZURE_CLIENT_ID");
        guard.remove("AZURE_CLIENT_SECRET");
        guard.set("AZURE_TENANT_ID", "tid");
        // Missing CLIENT_ID and CLIENT_SECRET
        let provider = ClientSecretProvider::new();
        let result = provider.get_token("test-scope").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn client_secret_all_env_vars_present() {
        let mut guard = EnvGuard::lock();
        guard.remove("AZURE_TENANT_ID");
        guard.remove("AZURE_CLIENT_ID");
        guard.remove("AZURE_CLIENT_SECRET");
        guard.set("AZURE_TENANT_ID", "tid");
        guard.set("AZURE_CLIENT_ID", "cid");
        guard.set("AZURE_CLIENT_SECRET", "secret");
        let provider = ClientSecretProvider::new();
        // build_credential succeeds with all env vars, but get_token will
        // fail at runtime (no real Entra ID). That's fine — we just want to
        // confirm it doesn't fail at the env-var stage.
        let result = provider.get_token("test-scope").await;
        // Will fail with a credential error (can't reach Entra ID), but
        // not a Provider error about missing env vars.
        assert!(result.is_err());
        assert!(
            !result.unwrap_err().to_string().contains("not set"),
            "should not be a missing-env-var error"
        );
    }
}
