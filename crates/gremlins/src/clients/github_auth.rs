//! GitHub App OAuth device-flow login and user-to-server token refresh.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use log::warn;
use serde::Deserialize;

use crate::clients::token_provider::{TokenProvider, TokenProviderError};

// ── Constants ────────────────────────────────────────────────────────────

/// OAuth scope requested for Copilot user-to-server tokens.
pub const GITHUB_SCOPE_COPILOT: &str = "copilot";

pub(crate) const DEFAULT_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
pub(crate) const DEFAULT_ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";

const GITHUB_OAUTH_TIMEOUT: Duration = Duration::from_secs(10);
const GITHUB_ACCESS_TOKEN_EXPIRY_SKEW_SECONDS: u64 = 30;

// ── Public types ─────────────────────────────────────────────────────────

/// Device-code initiation response: what the operator needs to authorize the
/// app in a browser.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub interval: Option<u64>,
}

/// Credentials produced by a completed device-flow login.
#[derive(Debug, Clone)]
pub struct DeviceFlowCredentials {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: Option<u64>,
}

// ── Internal OAuth types ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct OAuthTokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    #[serde(default)]
    error: Option<String>,
    #[serde(rename = "error_description", default)]
    error_description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OAuthErrorResponse {
    #[serde(default)]
    error: Option<String>,
    #[serde(rename = "error_description", default)]
    error_description: Option<String>,
}

// ── Cached access token ──────────────────────────────────────────────────

struct CachedAccessToken {
    token: String,
    expires_at: Instant,
}

// ── RefreshTokenPersister ────────────────────────────────────────────────

/// Callback for persisting a refreshed token so it survives restarts.
pub trait RefreshTokenPersister: Send + Sync {
    fn persist(&self, refresh_token: &str) -> Result<(), String>;
}

// ── Cross-process advisory lock ──────────────────────────────────────────

/// Acquire an exclusive advisory flock on `providers.yaml.lock`.
///
/// Blocks until the lock is acquired.  The lock is released when the
/// returned [`File`] is dropped.
#[cfg(unix)]
fn acquire_providers_lock() -> Result<std::fs::File, String> {
    use std::os::fd::AsRawFd;
    let path = crate::clients::config::providers_lock_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create dir for providers lock: {e}"))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("failed to open providers lock {}: {e}", path.display()))?;
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if ret != 0 {
        return Err(format!(
            "failed to lock providers.lock: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(file)
}

#[cfg(not(unix))]
fn acquire_providers_lock() -> Result<(), String> {
    // Advisory lock not available; rely on atomic writes for best-effort
    // safety.
    Ok(())
}

// ── GitHubAppTokenProvider ───────────────────────────────────────────────

/// Manages a GitHub App OAuth user-to-server token lifecycle.
///
/// Holds a cached access token with expiry and a persistent refresh token.
/// The first `access_token()` after the cached token expires refreshes via the
/// GitHub OAuth token endpoint and writes the rotated refresh token back
/// through the configured [`RefreshTokenPersister`].
#[derive(Clone)]
pub struct GitHubAppTokenProvider {
    inner: Arc<ProviderInner>,
}

/// Both the refresh token and cached access token are guarded by a single
/// mutex so the cache check + refresh sequence is atomic — two concurrent
/// callers cannot both observe an empty cache and race to rotate the same
/// single-use refresh token.
struct ProviderState {
    refresh_token: String,
    access_token: Option<CachedAccessToken>,
}

struct ProviderInner {
    client_id: String,
    state: Mutex<ProviderState>,
    token_url: String,
    persist: Option<Arc<dyn RefreshTokenPersister>>,
}

impl GitHubAppTokenProvider {
    /// Create a provider that persists refresh tokens to `providers.yaml`.
    pub fn new(client_id: impl Into<String>, refresh_token: impl Into<String>) -> Self {
        Self::with_parts(
            client_id.into(),
            refresh_token.into(),
            Some(Arc::new(ProvidersFilePersister::new("copilot"))),
            DEFAULT_ACCESS_TOKEN_URL.to_string(),
        )
    }

    /// Create a provider that does NOT persist refreshed tokens — useful for
    /// read-only validation commands.
    pub fn new_without_persister(
        client_id: impl Into<String>,
        refresh_token: impl Into<String>,
    ) -> Self {
        Self::with_parts(
            client_id.into(),
            refresh_token.into(),
            None,
            DEFAULT_ACCESS_TOKEN_URL.to_string(),
        )
    }

    /// Full constructor for tests and internal wiring.
    pub(crate) fn with_parts(
        client_id: String,
        refresh_token: String,
        persist: Option<Arc<dyn RefreshTokenPersister>>,
        token_url: String,
    ) -> Self {
        Self {
            inner: Arc::new(ProviderInner {
                client_id,
                state: Mutex::new(ProviderState {
                    refresh_token,
                    access_token: None,
                }),
                token_url,
                persist,
            }),
        }
    }

    /// Blocking refresh returning a fresh access token.
    ///
    /// Performs blocking HTTP, so it must run on a blocking thread (see
    /// [`Self::access_token`]) rather than directly on a tokio worker.
    ///
    /// Before refreshing, acquires a cross-process advisory lock on
    /// `providers.yaml.lock` and re-reads the latest refresh token from
    /// `providers.yaml` so that multiple backends or processes sharing the
    /// same credential do not race to consume the single-use refresh token.
    pub(crate) fn access_token_sync(&self) -> Result<String, String> {
        let mut state = self.inner.state.lock().unwrap();

        // Check the cache while we hold the lock.
        if let Some(cached) = &state.access_token {
            if cached.expires_at > Instant::now() {
                return Ok(cached.token.clone());
            }
        }

        // Acquire cross-process advisory lock before refreshing.
        #[cfg(unix)]
        let _lock = acquire_providers_lock()?;

        // Re-read the latest refresh token from disk — another process or
        // backend may have rotated it since we loaded our in-memory
        // snapshot.
        let refresh_token = match crate::clients::config::read_oauth_credential("copilot") {
            Ok((_, Some(rt))) => rt,
            _ => state.refresh_token.clone(),
        };
        state.refresh_token = refresh_token;

        // Cache miss or expired — refresh, still holding the lock so no
        // other caller can race us on the single-use refresh token.
        let response = refresh_access_token(
            &self.inner.token_url,
            &self.inner.client_id,
            &state.refresh_token,
        )?;

        let rotating = response
            .refresh_token
            .unwrap_or(state.refresh_token.clone());
        state.refresh_token = rotating.clone();
        if let Some(persist) = &self.inner.persist {
            if let Err(err) = persist.persist(&rotating) {
                warn!("failed to persist GitHub refresh token: {err}");
            }
        }

        let access_token = response
            .access_token
            .ok_or_else(|| "token refresh returned no access token".to_string())?;
        state.access_token = Some(CachedAccessToken {
            token: access_token.clone(),
            expires_at: access_token_expires_at(response.expires_in),
        });
        Ok(access_token)
    }

    /// Async access for external consumers (e.g., CLI status).
    pub async fn access_token(&self) -> Result<String, String> {
        // Check the cache first on the async path to avoid a pointless
        // spawn when the token is still fresh.
        {
            let state = self.inner.state.lock().unwrap();
            if let Some(cached) = &state.access_token {
                if cached.expires_at > Instant::now() {
                    return Ok(cached.token.clone());
                }
            }
        }
        let provider = self.clone();
        tokio::task::spawn_blocking(move || provider.access_token_sync())
            .await
            .map_err(|err| format!("GitHub token refresh task failed: {err}"))?
    }
}

/// Expiry instant for an access token, shortened by a renewal skew so a token
/// is refreshed before GitHub rejects it. Without an `expires_in` the cache is
/// always expired.
fn access_token_expires_at(expires_in: Option<u64>) -> Instant {
    match expires_in {
        Some(secs) if secs > GITHUB_ACCESS_TOKEN_EXPIRY_SKEW_SECONDS => {
            Instant::now() + Duration::from_secs(secs - GITHUB_ACCESS_TOKEN_EXPIRY_SKEW_SECONDS)
        }
        _ => Instant::now(),
    }
}

// ── TokenProvider impl ───────────────────────────────────────────────────

#[async_trait]
impl TokenProvider for GitHubAppTokenProvider {
    async fn get_token(&self, _scope: &str) -> Result<String, TokenProviderError> {
        self.access_token()
            .await
            .map_err(TokenProviderError::Provider)
    }
}

// ── Default persister ────────────────────────────────────────────────────

struct ProvidersFilePersister {
    provider: String,
}

impl ProvidersFilePersister {
    fn new(provider: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
        }
    }
}

impl RefreshTokenPersister for ProvidersFilePersister {
    fn persist(&self, refresh_token: &str) -> Result<(), String> {
        crate::clients::config::persist_refresh_token(&self.provider, refresh_token)
    }
}

// ── Public device-flow API ───────────────────────────────────────────────

/// Start the GitHub device-code flow. Returns the codes the operator uses to
/// authorize the app in a browser.
pub fn start_device_flow(client_id: &str) -> Result<DeviceCodeResponse, String> {
    start_device_flow_with_url(DEFAULT_DEVICE_CODE_URL, client_id)
}

/// [`start_device_flow`] with an injectable URL (for tests).
pub(crate) fn start_device_flow_with_url(
    device_code_url: &str,
    client_id: &str,
) -> Result<DeviceCodeResponse, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(GITHUB_OAUTH_TIMEOUT)
        .build()
        .map_err(|err| format!("failed to build HTTP client: {err}"))?;

    let response = client
        .post(device_code_url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form_body(&[
            ("client_id", client_id),
            ("scope", GITHUB_SCOPE_COPILOT),
        ]))
        .send()
        .map_err(|err| format!("device code request failed: {err}"))?;

    let status = response.status();
    let body = response
        .text()
        .map_err(|err| format!("failed to read device code response: {err}"))?;

    if !status.is_success() {
        let err: OAuthErrorResponse = serde_json::from_str(&body).unwrap_or(OAuthErrorResponse {
            error: None,
            error_description: None,
        });
        let msg = err
            .error_description
            .unwrap_or_else(|| err.error.unwrap_or_else(|| format!("HTTP {status}")));
        return Err(format!("device code request failed: {msg}"));
    }

    serde_json::from_str(&body).map_err(|err| format!("invalid device code response: {err}"))
}

/// Poll for the OAuth access token. Blocks until success, denial, or the
/// deadline passes.
pub fn complete_device_flow(
    client_id: &str,
    device_code: &str,
    interval: Duration,
    deadline: Instant,
) -> Result<DeviceFlowCredentials, String> {
    complete_device_flow_with_url(
        DEFAULT_ACCESS_TOKEN_URL,
        client_id,
        device_code,
        interval,
        deadline,
    )
}

/// [`complete_device_flow`] with an injectable URL (for tests).
pub(crate) fn complete_device_flow_with_url(
    token_url: &str,
    client_id: &str,
    device_code: &str,
    mut interval: Duration,
    deadline: Instant,
) -> Result<DeviceFlowCredentials, String> {
    loop {
        if Instant::now() >= deadline {
            return Err("device flow timed out before authorization completed".to_string());
        }

        let response = request_oauth_token(
            token_url,
            &[
                ("client_id", client_id),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", device_code),
            ],
        )?;

        match response.error.as_deref() {
            Some("authorization_pending") | None if response.access_token.is_none() => {
                std::thread::sleep(interval);
            }
            Some("slow_down") => {
                interval += Duration::from_secs(5);
                std::thread::sleep(interval);
            }
            Some(err) => {
                let detail = response
                    .error_description
                    .unwrap_or_else(|| err.to_string());
                return Err(format!("device flow authorization failed: {detail}"));
            }
            None => {
                let access_token = response.access_token.ok_or_else(|| {
                    "device flow completed but no access token returned".to_string()
                })?;
                let refresh_token = response.refresh_token.ok_or_else(|| {
                    "device flow completed but no refresh token returned".to_string()
                })?;
                return Ok(DeviceFlowCredentials {
                    access_token,
                    refresh_token,
                    expires_in: response.expires_in,
                });
            }
        }
    }
}

// ── Low-level HTTP helpers ───────────────────────────────────────────────

/// Refresh an access token using a stored refresh token.
fn refresh_access_token(
    token_url: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<OAuthTokenResponse, String> {
    let response = request_oauth_token(
        token_url,
        &[
            ("client_id", client_id),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ],
    )?;

    if let Some(err) = response.error {
        let detail = response.error_description.unwrap_or(err);
        return Err(format!("token refresh failed: {detail}"));
    }
    if response.access_token.is_none() {
        return Err("token refresh returned no access token".to_string());
    }
    Ok(response)
}

/// Low-level OAuth token endpoint POST, returning the parsed response
/// including any error fields the endpoint reports with a 200.
///
/// Error messages never include the raw response body — only the standard
/// `error` / `error_description` fields returned by GitHub's OAuth endpoint
/// and the HTTP status code. The token response body may contain credentials.
fn request_oauth_token(
    token_url: &str,
    form: &[(&str, &str)],
) -> Result<OAuthTokenResponse, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(GITHUB_OAUTH_TIMEOUT)
        .build()
        .map_err(|err| format!("failed to build HTTP client: {err}"))?;

    let response = client
        .post(token_url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form_body(form))
        .send()
        .map_err(|err| format!("OAuth token request failed: {err}"))?;

    let status = response.status();
    let body = response
        .text()
        .map_err(|err| format!("failed to read OAuth token response: {err}"))?;

    match serde_json::from_str::<OAuthTokenResponse>(&body) {
        Ok(parsed) => {
            if !status.is_success() && parsed.error.is_none() {
                return Err(format!(
                    "OAuth token request failed with HTTP {status} (no error detail)"
                ));
            }
            Ok(parsed)
        }
        Err(_) => {
            if !status.is_success() {
                return Err(format!(
                    "OAuth token request failed with HTTP {status} (unparseable response)"
                ));
            }
            Err("invalid OAuth token response (unparseable)".to_string())
        }
    }
}

// ── Form encoding ────────────────────────────────────────────────────────

/// Encode `fields` as an `application/x-www-form-urlencoded` request body.
///
/// `reqwest`'s `.form()` sits behind a feature this crate does not enable, so
/// the OAuth endpoints' bodies are encoded here instead.
fn form_body(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(name, value)| {
            format!(
                "{}={}",
                encode_form_component(name),
                encode_form_component(value)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encode one form component: unreserved characters pass through,
/// spaces become `+`, and every other byte is percent-escaped.
fn encode_form_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            b' ' => encoded.push('+'),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}
