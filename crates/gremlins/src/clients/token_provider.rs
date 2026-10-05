use async_trait::async_trait;
use azure_core::credentials::{Secret, TokenCredential};
use std::sync::Arc;

/// The Azure resource scope for cognitive services.
pub(crate) const AZURE_SCOPE: &str = "https://cognitiveservices.azure.com/.default";

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
pub(crate) struct ClientSecretProvider {
    credential: Arc<azure_identity::ClientSecretCredential>,
}

impl ClientSecretProvider {
    pub(crate) fn from_env() -> Result<Self, TokenProviderError> {
        let tenant_id = std::env::var("AZURE_TENANT_ID")
            .map_err(|_| TokenProviderError::Provider("AZURE_TENANT_ID not set".into()))?;
        let client_id = std::env::var("AZURE_CLIENT_ID")
            .map_err(|_| TokenProviderError::Provider("AZURE_CLIENT_ID not set".into()))?;
        let client_secret = std::env::var("AZURE_CLIENT_SECRET")
            .map_err(|_| TokenProviderError::Provider("AZURE_CLIENT_SECRET not set".into()))?;
        let credential = azure_identity::ClientSecretCredential::new(
            &tenant_id,
            client_id,
            Secret::new(client_secret),
            None,
        )
        .map_err(|e| TokenProviderError::Credential(e.to_string()))?;
        Ok(Self { credential })
    }
}

#[async_trait]
impl TokenProvider for ClientSecretProvider {
    async fn get_token(&self, scope: &str) -> Result<String, TokenProviderError> {
        let token_response = self
            .credential
            .get_token(&[scope], None)
            .await
            .map_err(|e| TokenProviderError::Credential(e.to_string()))?;
        Ok(token_response.token.secret().to_string())
    }
}

// ── AzureCliProvider ──────────────────────────────────────────────────────

/// Auth via the Azure CLI (`az account get-access-token`).
pub(crate) struct AzureCliProvider {
    credential: Arc<azure_identity::AzureCliCredential>,
}

impl AzureCliProvider {
    pub(crate) fn new() -> Self {
        let credential = azure_identity::AzureCliCredential::new(None)
            .expect("AzureCliCredential::new should not fail with default options");
        Self { credential }
    }
}

#[async_trait]
impl TokenProvider for AzureCliProvider {
    async fn get_token(&self, scope: &str) -> Result<String, TokenProviderError> {
        let token_response = self
            .credential
            .get_token(&[scope], None)
            .await
            .map_err(|e| TokenProviderError::Credential(e.to_string()))?;
        Ok(token_response.token.secret().to_string())
    }
}

// ── ManagedIdentityProvider ───────────────────────────────────────────────

/// Auth via Azure managed identity (IMDS endpoint).
pub(crate) struct ManagedIdentityProvider {
    credential: Arc<azure_identity::ManagedIdentityCredential>,
}

impl ManagedIdentityProvider {
    pub(crate) fn new() -> Self {
        let credential = azure_identity::ManagedIdentityCredential::new(None)
            .expect("ManagedIdentityCredential::new should not fail with default options");
        Self { credential }
    }
}

#[async_trait]
impl TokenProvider for ManagedIdentityProvider {
    async fn get_token(&self, scope: &str) -> Result<String, TokenProviderError> {
        let token_response = self
            .credential
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
pub(crate) struct DefaultAzureProvider {
    providers: Vec<Box<dyn TokenProvider>>,
}

impl DefaultAzureProvider {
    pub(crate) fn new() -> Self {
        let mut providers: Vec<Box<dyn TokenProvider>> = Vec::new();
        // Only include ClientSecretProvider when its env vars are set.
        if let Ok(p) = ClientSecretProvider::from_env() {
            providers.push(Box::new(p));
        }
        providers.push(Box::new(AzureCliProvider::new()));
        providers.push(Box::new(ManagedIdentityProvider::new()));
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
        let provider = DefaultAzureProvider {
            providers: vec![
                Box::new(MockProvider::new("first", vec![Ok("token-1".into())])),
                Box::new(MockProvider::new("second", vec![Ok("token-2".into())])),
            ],
        };
        let token = provider.get_token("test-scope").await.unwrap();
        assert_eq!(token, "token-1");
    }

    #[tokio::test]
    async fn default_azure_falls_through_on_error() {
        let provider = DefaultAzureProvider {
            providers: vec![
                Box::new(MockProvider::new(
                    "first",
                    vec![Err(TokenProviderError::Credential("fail".into()))],
                )),
                Box::new(MockProvider::new("second", vec![Ok("token-2".into())])),
            ],
        };
        let token = provider.get_token("test-scope").await.unwrap();
        assert_eq!(token, "token-2");
    }

    #[tokio::test]
    async fn default_azure_errors_when_all_fail() {
        let provider = DefaultAzureProvider {
            providers: vec![
                Box::new(MockProvider::new(
                    "first",
                    vec![Err(TokenProviderError::Credential("fail-1".into()))],
                )),
                Box::new(MockProvider::new(
                    "second",
                    vec![Err(TokenProviderError::Credential("fail-2".into()))],
                )),
            ],
        };
        let err = provider.get_token("test-scope").await.unwrap_err();
        assert!(
            matches!(err, TokenProviderError::Credential(_)),
            "should return last error"
        );
    }

    #[tokio::test]
    async fn default_azure_empty_providers_is_error() {
        let provider = DefaultAzureProvider {
            providers: Vec::new(),
        };
        let err = provider.get_token("test-scope").await.unwrap_err();
        assert!(
            matches!(err, TokenProviderError::Other(_)),
            "empty providers should be an error"
        );
    }

    // ── ClientSecretProvider env-var checks ───────────────────────────

    #[test]
    fn client_secret_missing_env_vars() {
        let _guard = EnvGuard::lock();
        std::env::remove_var("AZURE_TENANT_ID");
        std::env::remove_var("AZURE_CLIENT_ID");
        std::env::remove_var("AZURE_CLIENT_SECRET");
        let result = ClientSecretProvider::from_env();
        assert!(result.is_err());
    }

    #[test]
    fn client_secret_partial_env_vars() {
        let mut guard = EnvGuard::lock();
        guard.remove("AZURE_TENANT_ID");
        guard.remove("AZURE_CLIENT_ID");
        guard.remove("AZURE_CLIENT_SECRET");
        guard.set("AZURE_TENANT_ID", "tid");
        // Missing CLIENT_ID and CLIENT_SECRET
        let result = ClientSecretProvider::from_env();
        assert!(result.is_err());
    }

    #[test]
    fn client_secret_all_env_vars_present() {
        let mut guard = EnvGuard::lock();
        guard.remove("AZURE_TENANT_ID");
        guard.remove("AZURE_CLIENT_ID");
        guard.remove("AZURE_CLIENT_SECRET");
        guard.set("AZURE_TENANT_ID", "tid");
        guard.set("AZURE_CLIENT_ID", "cid");
        guard.set("AZURE_CLIENT_SECRET", "secret");
        let result = ClientSecretProvider::from_env();
        assert!(result.is_ok());
    }
}
