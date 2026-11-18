use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use rig_core::http_client::middleware::HttpMiddleware;
use rig_core::http_client::{
    Error as HttpError, HeaderMap, HeaderValue, Result as HttpResult,
};
use rig_core::providers::copilot::DIALECT as COPILOT_DIALECT;
use rig_core::providers::openai::{OpenAI, OpenAIConfig};

use crate::clients::backend::{Backend, ClientError, RunParams};
use crate::clients::cancel::CancelToken;
use crate::clients::github_auth;
use crate::clients::interactive::InteractiveSession;
use crate::clients::native::NativeRunState;
use crate::clients::params::{self, ResolvedParams};
use crate::clients::protocol::CompletedRun;
use crate::clients::token_provider::{TokenProvider, TokenProviderError};
use rig_core::driver::DynModel;
use rig_core::operation::Completion;
use rig_core::wasm_compat::WasmBoxedFuture;
use serde::Deserialize;

const PROVIDER_NAME: &str = "copilot";
const DEFAULT_MODEL: &str = "gpt-4o";

mod catalog;
pub(super) use catalog::list_models;

const GITHUB_COPILOT_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";
const GITHUB_COPILOT_TOKEN_TIMEOUT: Duration = Duration::from_secs(10);
pub(super) const COPILOT_EDITOR_VERSION: &str = "vscode/1.107.0";
pub(super) const COPILOT_EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.35.0";
pub(super) const COPILOT_USER_AGENT: &str = "GitHubCopilotChat/0.35.0";
pub(super) const COPILOT_TOKEN_EXPIRY_SKEW_SECONDS: i64 = 30;

/// Sentinel prefix for the shared resolver's missing-credential error, which
/// [`resolve_auth`] rewrites into completion-path guidance.
pub(super) const NO_COPILOT_CREDENTIALS: &str = "no Copilot credentials";

/// Sentinel prefix for a stored-app (`client-id` + `refresh-token`) credential
/// refresh failure, so the catalog layer can distinguish it from a session
/// exchange failure.
pub(super) const COPILOT_APP_REFRESH_ERROR: &str = "Copilot app credential refresh failed";

#[derive(Debug, Deserialize)]
struct CopilotTokenResponse {
    token: Option<String>,
    expires_at: Option<i64>,
    endpoints: Option<CopilotTokenEndpoints>,
}

#[derive(Debug, Deserialize)]
struct CopilotTokenEndpoints {
    api: Option<String>,
}

/// A validated Copilot session credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CopilotSession {
    pub(super) token: String,
    pub(super) api_base: Option<String>,
    expires_at: Option<i64>,
}

/// Resolve credentials and build a Copilot client.
/// OAuth device-code flow is disabled — gremlins run unattended.
///
/// Credential resolution is delegated to [`resolve_session`] — the same
/// resolver the model catalog uses — so completions and discovery share one
/// authenticated configuration and transport builder.  This includes the
/// pre-exchanged static session tokens, which are wrapped in the refresh
/// middleware whose `i64::MAX` expiry never triggers a refresh.
///
/// `make_exchanger` is threaded through to [`resolve_session`] and only invoked
/// once the session-token sources miss and a GitHub access token is found, so
/// the blocking client is never built for a pre-exchanged session token.
fn resolve_auth(
    make_exchanger: impl FnOnce() -> Result<Arc<dyn CopilotTokenExchanger>, String>,
) -> Result<OpenAI, String> {
    let (session, middleware) = resolve_session(make_exchanger).map_err(map_auth_error)?;
    Ok(copilot_client_with_session(session, middleware))
}

/// Rewrite the shared resolver's terse missing-credential sentinel into the
/// completion-path guidance, which names every supported source and the
/// `providers.yaml` location.  Other errors pass through unchanged.
fn map_auth_error(err: String) -> String {
    if err.starts_with(NO_COPILOT_CREDENTIALS) {
        format!(
            "no credentials for provider '{PROVIDER_NAME}': set GITHUB_COPILOT_API_KEY, \
             COPILOT_API_KEY, GITHUB_TOKEN, or GH_TOKEN; or add an entry with \
             \"api-key\", \"client-id\" + \"refresh-token\", or \"github-token\" in {}",
            crate::config::user_config_root()
                .join("providers.yaml")
                .display(),
        )
    } else {
        err
    }
}

/// Result of validating a Copilot OAuth credential end-to-end.
#[derive(Debug, Clone)]
pub struct CopilotCredentialStatus {
    /// Unix timestamp when the Copilot session expires, if the server
    /// returned one.
    pub expires_at: Option<i64>,
}

/// Validate a Copilot OAuth credential end-to-end: refresh the GitHub access
/// token, exchange it for a Copilot session, and return the session status.
///
/// Uses the persisting provider so the rotated refresh token is saved to
/// `providers.yaml` — GitHub App refresh tokens are single-use, and
/// discarding the replacement would break subsequent authentication.
pub fn check_copilot_oauth_credential(
    client_id: &str,
    refresh_token: &str,
) -> Result<CopilotCredentialStatus, String> {
    let provider =
        github_auth::GitHubAppTokenProvider::new(client_id, refresh_token);
    let access_token = provider.access_token_sync()?;
    status_from_exchange(&BlockingCopilotTokenExchanger::new(), &access_token)
}

/// Validate a static GitHub access token by exchanging it for a Copilot
/// session token and returning the session status.
pub fn check_copilot_github_token(token: &str) -> Result<CopilotCredentialStatus, String> {
    status_from_exchange(&BlockingCopilotTokenExchanger::new(), token)
}

/// Exchange an access token and wrap the resulting session status.
fn status_from_exchange(
    exchanger: &dyn CopilotTokenExchanger,
    access_token: &str,
) -> Result<CopilotCredentialStatus, String> {
    let session = exchanger.exchange(access_token)?;
    Ok(CopilotCredentialStatus {
        expires_at: session.expires_at,
    })
}

/// Read a nonblank GitHub access token from the environment.
fn github_access_token_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Read a nonblank GitHub access token from GITHUB_TOKEN or GH_TOKEN env.
fn github_access_token() -> Option<String> {
    github_access_token_env("GITHUB_TOKEN").or_else(|| github_access_token_env("GH_TOKEN"))
}

/// Exchange a GitHub access token for a Copilot session token.
///
/// This mirrors rig's synchronous-free GitHub-token auth request shape: a `GET`
/// to GitHub Copilot's token endpoint with the Copilot editor identity headers
/// and the GitHub token in `Authorization: token ...`.
///
/// The blocking request runs on a fresh thread: `reqwest::blocking` uses an
/// internal runtime and panics if it is entered from a tokio worker, so the
/// exchange itself must never run on `CopilotBackend::build`'s calling thread.
fn exchange_github_token_sync(
    exchanger: Arc<dyn CopilotTokenExchanger>,
    access_token: &str,
) -> Result<CopilotSession, String> {
    let access_token = access_token.to_string();
    std::thread::spawn(move || exchanger.exchange(&access_token))
        .join()
        .map_err(|_| "GitHub Copilot token exchange panicked".to_string())?
}

/// Exchange a GitHub access token for a Copilot session token without blocking
/// a tokio worker.
async fn exchange_github_token_async(
    exchanger: Arc<dyn CopilotTokenExchanger>,
    access_token: String,
) -> Result<CopilotSession, String> {
    tokio::task::spawn_blocking(move || exchanger.exchange(&access_token))
        .await
        .map_err(|err| format!("GitHub Copilot token exchange task failed: {err}"))?
}

/// A swappable token exchange, so tests can drive auth without a network.
pub(super) trait CopilotTokenExchanger: Send + Sync {
    fn exchange(&self, access_token: &str) -> Result<CopilotSession, String>;
}

/// The production token exchange: a short-timeout blocking HTTPS request.
///
/// The blocking client is created inside [`exchange`] rather than stored so
/// its internal runtime is always constructed and dropped on the blocking
/// thread, never on a tokio worker.
#[derive(Clone)]
pub(super) struct BlockingCopilotTokenExchanger {
    token_url: String,
}

impl BlockingCopilotTokenExchanger {
    pub(super) fn new() -> Self {
        Self::with_url(GITHUB_COPILOT_TOKEN_URL)
    }

    fn with_url(token_url: &str) -> Self {
        Self {
            token_url: token_url.to_string(),
        }
    }
}

impl CopilotTokenExchanger for BlockingCopilotTokenExchanger {
    fn exchange(&self, access_token: &str) -> Result<CopilotSession, String> {
        let client = reqwest::blocking::Client::builder()
            .timeout(GITHUB_COPILOT_TOKEN_TIMEOUT)
            .build()
            .map_err(|err| {
                format!("failed to build GitHub Copilot token exchange client: {err}")
            })?;

        let response = client
            .get(&self.token_url)
            .header(reqwest::header::ACCEPT, "application/json")
            .header("editor-version", COPILOT_EDITOR_VERSION)
            .header("editor-plugin-version", COPILOT_EDITOR_PLUGIN_VERSION)
            .header("user-agent", COPILOT_USER_AGENT)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("token {access_token}"),
            )
            .send()
            .map_err(|err| format!("GitHub Copilot token exchange failed: {err}"))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().unwrap_or_default();
            return Err(format!(
                "GitHub Copilot token exchange failed with HTTP {status}{}",
                token_exchange_error_detail(&body)
            ));
        }

        let record: CopilotTokenResponse = response.json().map_err(|err| {
            format!("GitHub Copilot token exchange response was invalid: {err}")
        })?;
        session_from_response(record)
    }
}

/// Validate an exchange response and extract the session credential.
fn session_from_response(record: CopilotTokenResponse) -> Result<CopilotSession, String> {
    let CopilotTokenResponse {
        token,
        expires_at,
        endpoints,
    } = record;
    let api_base = endpoints
        .and_then(|endpoints| endpoints.api)
        .map(|api| api.trim().to_string())
        .filter(|api| !api.is_empty());
    let session_token = token
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            "GitHub Copilot token exchange response did not include a session token".to_string()
        })?;
    let expires_at = expires_at.or_else(|| parse_expires_at(&session_token));
    Ok(CopilotSession {
        token: session_token,
        api_base,
        expires_at,
    })
}

/// Parse the `exp=` epoch-second segment of a Copilot session token.
fn parse_expires_at(token: &str) -> Option<i64> {
    token.split(';').find_map(|part| {
        let part = part.trim();
        part.strip_prefix("exp=")?.parse::<i64>().ok()
    })
}

/// True when a session has no expiry or is at (or past) the renewal skew.
fn session_expired(session: &CopilotSession) -> bool {
    let Some(expires_at) = session.expires_at else {
        return true;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(i64::MAX);

    now >= expires_at.saturating_sub(COPILOT_TOKEN_EXPIRY_SKEW_SECONDS)
}

/// Resolves a Copilot session credential from a GitHub access-token provider,
/// refreshing the exchanged Copilot session when it expires.
pub(super) struct CopilotSessionProvider {
    token_provider: Arc<dyn TokenProvider>,
    exchanger: Arc<dyn CopilotTokenExchanger>,
    session: tokio::sync::Mutex<Option<CopilotSession>>,
}

impl CopilotSessionProvider {
    fn with_session(
        token_provider: Arc<dyn TokenProvider>,
        exchanger: Arc<dyn CopilotTokenExchanger>,
        session: CopilotSession,
    ) -> Self {
        Self {
            token_provider,
            exchanger,
            session: tokio::sync::Mutex::new(Some(session)),
        }
    }

    async fn ensure_fresh(&self) -> Result<CopilotSession, String> {
        let mut guard = self.session.lock().await;
        if let Some(session) = guard.as_ref() {
            if !session_expired(session) {
                return Ok(session.clone());
            }
        }

        let access_token = self
            .token_provider
            .get_token(github_auth::GITHUB_SCOPE_COPILOT)
            .await
            .map_err(|e| format!("failed to refresh GitHub access token: {e}"))?;
        let fresh =
            exchange_github_token_async(self.exchanger.clone(), access_token).await?;
        *guard = Some(fresh.clone());
        Ok(fresh)
    }
}

/// A [`TokenProvider`] that always yields a fixed GitHub access token.
struct StaticTokenProvider {
    token: String,
}

#[async_trait]
impl TokenProvider for StaticTokenProvider {
    async fn get_token(&self, _scope: &str) -> Result<String, TokenProviderError> {
        Ok(self.token.clone())
    }
}

/// Replaces the bearer credential before every Copilot request, refreshing the
/// GitHub-token-backed session when it expires.
#[derive(Clone)]
pub(super) struct CopilotTokenRefreshMiddleware {
    sessions: Arc<CopilotSessionProvider>,
}

impl CopilotTokenRefreshMiddleware {
    pub(super) fn new(sessions: Arc<CopilotSessionProvider>) -> Self {
        Self { sessions }
    }
}

impl HttpMiddleware for CopilotTokenRefreshMiddleware {
    fn before_request_headers<'a>(
        &'a self,
        _exchange: rig_core::http_client::Exchange<'a>,
        headers: &'a mut HeaderMap,
    ) -> WasmBoxedFuture<'a, HttpResult<()>> {
        Box::pin(async move {
            let session = self
                .sessions
                .ensure_fresh()
                .await
                .map_err(|message| HttpError::instance(std::io::Error::other(message)))?;
            let value = HeaderValue::from_str(&format!("Bearer {}", session.token))?;
            headers.insert(http::header::AUTHORIZATION, value);
            Ok(())
        })
    }
}

/// Render a small, non-empty response body snippet for exchange errors.
fn token_exchange_error_detail(body: &str) -> String {
    const MAX_BODY_BYTES: usize = 512;

    let body = body.trim();
    if body.is_empty() {
        return String::new();
    }

    let mut end = body.len().min(MAX_BODY_BYTES);
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    let snippet = &body[..end];
    if end < body.len() {
        format!(": {snippet}...")
    } else {
        format!(": {snippet}")
    }
}

pub(super) fn copilot_transport(
    middleware: CopilotTokenRefreshMiddleware,
) -> rig_core::http_client::DynHttpClient {
    rig_core::http_client::DynHttpClient::new(rig_reqwest::shared()).with_middleware(middleware)
}

/// The single credential resolver shared by completions and discovery.
///
/// Precedence: `GITHUB_COPILOT_API_KEY` / `COPILOT_API_KEY` → `providers.yaml`
/// `copilot.api-key` (both pre-exchanged static session tokens) → `providers.yaml`
/// `copilot.client-id` + `copilot.refresh-token` → `providers.yaml`
/// `copilot.github-token` → `GITHUB_TOKEN` / `GH_TOKEN` (both exchanged for a
/// session token).
///
/// `make_exchanger` is only invoked once a GitHub access token is found, so the
/// blocking client is never built for a pre-exchanged session token.
fn resolve_session(
    make_exchanger: impl FnOnce() -> Result<Arc<dyn CopilotTokenExchanger>, String>,
) -> Result<(CopilotSession, CopilotTokenRefreshMiddleware), String> {
    // Pre-exchanged session credentials always take precedence over GitHub
    // credentials; static sessions never refresh (`i64::MAX` expiry).
    if let Some(token) = crate::config::copilot_api_key() {
        let session = static_session(token);
        return Ok((session.clone(), static_session_middleware(session)));
    }

    let providers = crate::clients::config::Providers::load();
    if let Some(token) = providers.get(PROVIDER_NAME) {
        let session = static_session(token.to_string());
        return Ok((session.clone(), static_session_middleware(session)));
    }

    let (provider, github_token) = if let (Some(id), Some(refresh)) = (
        providers.client_id(PROVIDER_NAME),
        providers.refresh_token(PROVIDER_NAME),
    ) {
        // access_token_sync refreshes stored credentials only; unlike the
        // device-flow methods, it never starts interactive login.
        let provider = Arc::new(github_auth::GitHubAppTokenProvider::new(id, refresh));
        let token = provider
            .access_token_sync()
            .map_err(|e| format!("{COPILOT_APP_REFRESH_ERROR}: {e}"))?;
        (provider as Arc<dyn TokenProvider>, token)
    } else {
        let token = providers
            .github_token(PROVIDER_NAME)
            .map(str::to_string)
            .or_else(github_access_token)
            .ok_or_else(|| {
                format!(
                    "{NO_COPILOT_CREDENTIALS}: set GITHUB_COPILOT_API_KEY or COPILOT_API_KEY, \
                     configure copilot.api-key, copilot.client-id plus copilot.refresh-token, \
                     or copilot.github-token in providers.yaml, or set GITHUB_TOKEN or GH_TOKEN"
                )
            })?;
        (
            Arc::new(StaticTokenProvider { token: token.clone() }) as Arc<dyn TokenProvider>,
            token,
        )
    };

    let exchanger = make_exchanger()?;
    let session = exchange_github_token_sync(exchanger.clone(), &github_token)?;
    let sessions = Arc::new(CopilotSessionProvider::with_session(
        provider,
        exchanger,
        session.clone(),
    ));
    Ok((session, CopilotTokenRefreshMiddleware::new(sessions)))
}

/// Resolve a Copilot session for the model catalog through the shared resolver,
/// using the production blocking exchanger.
pub(super) fn resolve_catalog_session(
) -> Result<(CopilotSession, CopilotTokenRefreshMiddleware), String> {
    resolve_session(|| {
        Ok(Arc::new(BlockingCopilotTokenExchanger::new()) as Arc<dyn CopilotTokenExchanger>)
    })
}

/// Resolve a session credential through the shared resolver, exposing
/// [`resolve_session`] to the catalog's integration tests.
#[cfg(test)]
#[allow(private_bounds, private_interfaces)]
pub(crate) fn resolve_session_for_test(
    make_exchanger: impl FnOnce() -> Result<Arc<dyn CopilotTokenExchanger>, String>,
) -> Result<(CopilotSession, CopilotTokenRefreshMiddleware), String> {
    resolve_session(make_exchanger)
}

fn static_session(token: String) -> CopilotSession {
    CopilotSession {
        token,
        api_base: None,
        // A static session credential is pre-exchanged and does not expire
        // through the GitHub token renewal flow.
        expires_at: Some(i64::MAX),
    }
}

pub(super) fn static_session_middleware(session: CopilotSession) -> CopilotTokenRefreshMiddleware {
    let provider = Arc::new(CopilotSessionProvider::with_session(
        Arc::new(StaticTokenProvider { token: String::new() }),
        Arc::new(BlockingCopilotTokenExchanger::new()),
        session,
    ));
    CopilotTokenRefreshMiddleware::new(provider)
}

fn copilot_client_with_session(
    session: CopilotSession,
    middleware: CopilotTokenRefreshMiddleware,
) -> OpenAI {
    let transport = copilot_transport(middleware);
    let config = OpenAIConfig::with_key(&COPILOT_DIALECT, session.token);
    let config = match session.api_base {
        Some(api_base) => config.with_base_url(api_base),
        None => config,
    };
    config.connect(transport)
}

// ── CopilotRunState ──────────────────────────────────────────────────────

struct CopilotRunState {
    client: OpenAI,
    strict_tools: bool,
    tool_result_array_content: bool,
    native: NativeRunState<()>,
}

/// Parse Copilot-specific typed flags from the model-spec parameter map.
///
/// `strict_tools` and `tool_result_array_content` are strict bools.  A
/// malformed value is a build-time error, never a silent fallback.
///
/// The Copilot `intent` (edits/panel) concept was removed in rig 0.46:
/// Copilot is now configured as an `OpenAI` provider with the copilot dialect.
fn parse_extra_params(
    extra_params: &indexmap::IndexMap<String, String>,
) -> Result<(bool, bool), String> {
    if let Some(v) = extra_params.get("intent") {
        return Err(format!(
            "copilot: the \"intent\" client param ({v:?}) was removed in rig 0.46; \
             Copilot is now configured as an OpenAI provider with the copilot dialect"
        ));
    }
    let strict_tools = match extra_params.get("strict_tools") {
        Some(v) => params::parse_bool("copilot", "strict_tools", v)?,
        None => false,
    };
    let tool_result_array_content = match extra_params.get("tool_result_array_content") {
        Some(v) => params::parse_bool("copilot", "tool_result_array_content", v)?,
        None => false,
    };
    Ok((strict_tools, tool_result_array_content))
}

// ── CopilotBackend ───────────────────────────────────────────────────────

pub struct CopilotBackend {
    state: CopilotRunState,
}

impl CopilotBackend {
    /// Build a Copilot backend.
    ///
    /// Auth precedence: `GITHUB_COPILOT_API_KEY` → `COPILOT_API_KEY` →
    /// `providers.yaml` `"copilot"` entry → `GITHUB_TOKEN` → `GH_TOKEN` →
    /// error.
    pub fn build(
        model: &str,
        native_block: &HashMap<String, Vec<String>>,
        extra_params: &indexmap::IndexMap<String, String>,
    ) -> Result<Arc<dyn Backend>, String> {
        Self::build_with_exchanger(
            model,
            native_block,
            extra_params,
            || {
                Ok(Arc::new(BlockingCopilotTokenExchanger::new())
                    as Arc<dyn CopilotTokenExchanger>)
            },
        )
    }

    /// Shared construction path; `make_exchanger` is a parameter so tests can
    /// drive the backend without network access.
    fn build_with_exchanger(
        model: &str,
        native_block: &HashMap<String, Vec<String>>,
        extra_params: &indexmap::IndexMap<String, String>,
        make_exchanger: impl FnOnce() -> Result<Arc<dyn CopilotTokenExchanger>, String>,
    ) -> Result<Arc<dyn Backend>, String> {
        let client = resolve_auth(make_exchanger)?;

        let model = if model.is_empty() {
            DEFAULT_MODEL.to_string()
        } else {
            model.to_string()
        };

        let tool_filter = crate::clients::config::tool_filter(native_block);

        let resolved = Self::resolve_params(extra_params)?;
        let (strict_tools, tool_result_array_content) = parse_extra_params(extra_params)?;

        Ok(Arc::new(Self {
            state: CopilotRunState {
                client,
                strict_tools,
                tool_result_array_content,
                native: NativeRunState::new(
                    (),
                    model,
                    tool_filter,
                    resolved,
                    "CopilotBackend",
                ),
            },
        }))
    }
}

#[async_trait]
impl Backend for CopilotBackend {
    fn build_model(&self, spec: &str) -> Option<DynModel<Completion>> {
        let (provider, model) = crate::model_uri::provider_and_model(spec)?;
        if provider == PROVIDER_NAME {
            let mut mdl = self.state.client.completion(&model);
            if self.state.strict_tools {
                mdl.wire = mdl.wire.with_strict_tools();
            }
            if self.state.tool_result_array_content {
                mdl.wire = mdl.wire.with_tool_result_array_content();
            }
            Some(mdl.erase())
        } else {
            None
        }
    }

    async fn run(
        &self,
        params: RunParams,
        interactive: InteractiveSession,
    ) -> Result<CompletedRun, ClientError> {
        let ctx = self.state.native.prepare_context(params.clone());
        let cancel = params.cancel_token.clone().unwrap_or_else(CancelToken::new);
        let model_name = self.state.native.effective_model(ctx.params.model.as_deref());

        let client = &self.state.client;
        let strict_tools = self.state.strict_tools;
        let tool_result_array_content = self.state.tool_result_array_content;

        let make_model = async move {
            let mut mdl = client.completion(&model_name);
            if strict_tools {
                mdl.wire = mdl.wire.with_strict_tools();
            }
            if tool_result_array_content {
                mdl.wire = mdl.wire.with_tool_result_array_content();
            }
            Ok(mdl.erase())
        };

        self.state
            .native
            .single_attempt(
                &params.prompt,
                ctx,
                cancel,
                interactive,
                crate::clients::native::PreserveReasoning::No,
                make_model,
            )
            .await
    }

    super::delegate_native_run_state!(state.native);

    fn model(&self) -> &str {
        &self.state.native.model
    }

    fn name(&self) -> &'static str {
        "copilot"
    }

    fn resolve_params(raw: &indexmap::IndexMap<String, String>) -> Result<ResolvedParams, String> {
        // Validate the typed flags strictly (so a malformed value errors at
        // build time), then resolve temperature/max_tokens and passthrough.
        parse_extra_params(raw)?;

        let mut additional = serde_json::Map::new();
        let mut temperature = None;
        let mut max_tokens = None;

        for (k, v) in raw {
            match k.as_str() {
                "temperature" => temperature = Some(params::parse_f64("copilot", k, v)?),
                "max_tokens" => max_tokens = Some(params::parse_u64("copilot", k, v)?),
                "intent" | "strict_tools" | "tool_result_array_content" => {}
                _ => {
                    additional.insert(k.clone(), params::coerce(v));
                }
            }
        }

        Ok(ResolvedParams {
            temperature,
            max_tokens,
            additional,
        })
    }
}

// ── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::clients::cancel::CancelToken;
    use super::*;
    use rig_core::http_client::{HttpClientExt, Method, Uri};
    use crate::test_support::EnvGuard;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    const FAKE_API_KEY: &str = "tid=1;exp=9999999999";

    fn scrub_copilot_env(guard: &mut EnvGuard) {
        guard.remove("GITHUB_COPILOT_API_KEY");
        guard.remove("COPILOT_API_KEY");
        guard.remove("GITHUB_TOKEN");
        guard.remove("GH_TOKEN");
        guard.remove("XDG_CONFIG_HOME");
    }

    /// Set up an isolated sandbox with no providers.yaml and no GitHub
    /// credentials in the environment.
    fn isolated_env() -> EnvGuard {
        let mut guard = EnvGuard::lock();
        scrub_copilot_env(&mut guard);
        let tmp = tempfile::tempdir().unwrap();
        guard.set("GREMLINS_SANDBOX_ROOT", tmp.path());
        guard.set("HOME", tmp.path());
        guard
    }

    fn resolve_auth_unreachable() -> Result<OpenAI, String> {
        resolve_auth(|| Ok(unreachable_exchanger()))
    }

    #[derive(Clone)]
    struct UnreachableExchanger;

    impl CopilotTokenExchanger for UnreachableExchanger {
        fn exchange(&self, _access_token: &str) -> Result<CopilotSession, String> {
            Err("token exchange should not have been called".into())
        }
    }

    fn unreachable_exchanger() -> Arc<dyn CopilotTokenExchanger> {
        Arc::new(UnreachableExchanger)
    }

    fn fresh_session(expires_at: i64) -> CopilotSession {
        CopilotSession {
            token: FAKE_API_KEY.into(),
            api_base: None,
            expires_at: Some(expires_at),
        }
    }

    fn static_token_provider(token: &str) -> Arc<dyn TokenProvider> {
        Arc::new(StaticTokenProvider {
            token: token.to_string(),
        })
    }

    struct CountingExchanger {
        calls: AtomicUsize,
        session: CopilotSession,
    }

    impl CountingExchanger {
        fn new(expires_at: i64) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                session: fresh_session(expires_at),
            }
        }
    }

    impl CopilotTokenExchanger for CountingExchanger {
        fn exchange(&self, _access_token: &str) -> Result<CopilotSession, String> {
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(self.session.clone())
        }
    }

    /// Serve exactly one HTTP request and return the endpoint URL plus a handle
    /// that yields the request head once the test has driven a client call.
    fn serve_once(
        status_line: &'static str,
        body: &'static str,
    ) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/copilot_internal/v2/token");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                let n = stream.read(&mut chunk).unwrap();
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&buf).to_string();
            write!(
                stream,
                "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            stream.flush().unwrap();
            request
        });
        (url, server)
    }

    #[test]
    fn provider_constants() {
        assert_eq!(PROVIDER_NAME, "copilot");
        assert_eq!(DEFAULT_MODEL, "gpt-4o");
    }

    #[test]
    fn reap_all_cancels_only_own_tokens() {
        let client = OpenAIConfig::with_key(&COPILOT_DIALECT, "tid=1;exp=9999999999").client();
        let backend = CopilotBackend {
            state: CopilotRunState {
                client,
                strict_tools: false,
                tool_result_array_content: false,
                native: NativeRunState::new(
                    (),
                    "gpt-4o".into(),
                    None,
                    ResolvedParams::default(),
                    "test",
                ),
            },
        };
        let a = CancelToken::new();
        let b = CancelToken::new();
        let sibling = CancelToken::new();
        let cancels = &backend.state.native.cancels;
        cancels.insert("gr-test", 1, a.clone()).unwrap();
        cancels.insert("gr-test", 2, b.clone()).unwrap();
        cancels.insert("gr-sibling", 3, sibling.clone()).unwrap();
        backend.reap_all("gr-test");
        assert!(a.is_cancelled());
        assert!(b.is_cancelled());
        assert!(!sibling.is_cancelled());
    }

    #[test]
    fn build_rejects_missing_credentials() {
        let _guard = isolated_env();

        let result = CopilotBackend::build_with_exchanger(
            "gpt-4o",
            &HashMap::new(),
            &indexmap::IndexMap::new(),
            || Ok(unreachable_exchanger()),
        );
        let err = result.expect_err("should be an error");
        assert!(err.contains("no credentials for provider"), "got: {err}");
    }

    // ── auth tests ───────────────────────────────────────────────────

    #[test]
    fn auth_github_copilot_api_key() {
        let mut guard = isolated_env();
        guard.set("GITHUB_COPILOT_API_KEY", FAKE_API_KEY);
        assert!(resolve_auth_unreachable().is_ok());
    }

    #[test]
    fn auth_copilot_api_key_fallback() {
        let mut guard = isolated_env();
        guard.set("COPILOT_API_KEY", FAKE_API_KEY);
        assert!(resolve_auth_unreachable().is_ok());
    }

    #[test]
    fn auth_providers_yaml() {
        let mut guard = isolated_env();
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("providers.yaml"),
            format!(r#"{{"copilot": {{"api-key": "{FAKE_API_KEY}"}}}}"#),
        )
        .unwrap();
        guard.set("GREMLINS_SANDBOX_ROOT", tmp.path());
        assert!(resolve_auth_unreachable().is_ok());
    }

    #[test]
    fn auth_missing_credentials_error_names_github_sources() {
        let _guard = isolated_env();

        let err = resolve_auth_unreachable().unwrap_err();
        assert!(err.contains("GITHUB_COPILOT_API_KEY"), "got: {err}");
        assert!(err.contains("GITHUB_TOKEN"), "got: {err}");
        assert!(err.contains("GH_TOKEN"), "got: {err}");
        assert!(err.contains("client-id"), "got: {err}");
        assert!(!err.contains("gh auth"), "error should not mention gh CLI: {err}");
        assert!(!err.contains("gh "), "error should not mention gh CLI: {err}");
    }

    #[test]
    fn auth_session_token_beats_github_token() {
        let mut guard = isolated_env();
        guard.set("GITHUB_COPILOT_API_KEY", FAKE_API_KEY);
        guard.set("GITHUB_TOKEN", "should-not-be-exchanged");
        assert!(resolve_auth_unreachable().is_ok());
    }

    #[test]
    fn auth_providers_yaml_beats_github_token() {
        let mut guard = isolated_env();
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("providers.yaml"),
            format!(r#"{{"copilot": {{"api-key": "{FAKE_API_KEY}"}}}}"#),
        )
        .unwrap();
        guard.set("GREMLINS_SANDBOX_ROOT", tmp.path());
        guard.set("GITHUB_TOKEN", "should-not-be-exchanged");
        assert!(resolve_auth_unreachable().is_ok());
    }

    #[test]
    fn auth_github_token_env_precedence() {
        let mut guard = isolated_env();
        guard.set("GH_TOKEN", "from-gh-token");
        guard.set("GITHUB_TOKEN", "from-github-token");
        assert_eq!(
            github_access_token().as_deref(),
            Some("from-github-token")
        );
    }

    #[test]
    fn auth_gh_token_env_fallback() {
        let mut guard = isolated_env();
        guard.set("GH_TOKEN", "from-gh-token");
        assert_eq!(github_access_token().as_deref(), Some("from-gh-token"));
    }

    // ── token exchange tests ──────────────────────────────────────────

    #[test]
    fn exchange_success_sends_expected_headers_and_honors_api_base() {
        let (url, server) = serve_once(
            "HTTP/1.1 200 OK",
            r#"{"token":"tid=1;exp=9999999999","expires_at":9999999999,"endpoints":{"api":"https://api.example.com"}}"#,
        );
        let exchanger = BlockingCopilotTokenExchanger::with_url(&url);
        let session = exchanger.exchange("gh-token").unwrap();

        assert_eq!(session.token, "tid=1;exp=9999999999");
        assert_eq!(session.api_base.as_deref(), Some("https://api.example.com"));
        assert_eq!(session.expires_at, Some(9999999999));

        let request = server.join().unwrap().to_ascii_lowercase();
        assert!(request.contains("get /copilot_internal/v2/token"), "{request}");
        assert!(
            request.contains("authorization: token gh-token"),
            "{request}"
        );
        assert!(request.contains("accept: application/json"), "{request}");
        assert!(
            request.contains("editor-version: vscode/1.107.0"),
            "{request}"
        );
        assert!(
            request.contains("editor-plugin-version: copilot-chat/0.35.0"),
            "{request}"
        );
        assert!(
            request.contains("user-agent: githubcopilotchat/0.35.0"),
            "{request}"
        );
    }

    #[test]
    fn exchange_success_falls_back_to_token_exp() {
        let (url, server) = serve_once("HTTP/1.1 200 OK", r#"{"token":"tid=1;exp=9999999999"}"#);
        let exchanger = BlockingCopilotTokenExchanger::with_url(&url);
        let session = exchanger.exchange("gh-token").unwrap();

        assert_eq!(session.expires_at, Some(9999999999));
        assert_eq!(session.api_base, None);
        server.join().unwrap();
    }

    #[test]
    fn exchange_success_ignores_blank_api_base() {
        let (url, server) = serve_once(
            "HTTP/1.1 200 OK",
            r#"{"token":"tid=1;exp=9999999999","endpoints":{"api":"   "}}"#,
        );
        let exchanger = BlockingCopilotTokenExchanger::with_url(&url);
        let session = exchanger.exchange("gh-token").unwrap();

        assert_eq!(session.api_base, None);
        server.join().unwrap();
    }

    #[test]
    fn exchange_http_error_includes_status_and_body() {
        let (url, server) = serve_once(
            "HTTP/1.1 500 Internal Server Error",
            r#"{"message":"bad token"}"#,
        );
        let exchanger = BlockingCopilotTokenExchanger::with_url(&url);
        let err = exchanger.exchange("gh-token").unwrap_err();

        assert!(err.contains("HTTP 500"), "{err}");
        assert!(err.contains("bad token"), "{err}");
        server.join().unwrap();
    }

    #[test]
    fn exchange_missing_token_is_error() {
        let (url, server) = serve_once(
            "HTTP/1.1 200 OK",
            r#"{"endpoints":{"api":"https://api.example.com"}}"#,
        );
        let exchanger = BlockingCopilotTokenExchanger::with_url(&url);
        let err = exchanger.exchange("gh-token").unwrap_err();

        assert!(err.contains("did not include a session token"), "{err}");
        server.join().unwrap();
    }

    #[test]
    fn exchange_blank_token_is_error() {
        let (url, server) = serve_once("HTTP/1.1 200 OK", r#"{"token":"   "}"#);
        let exchanger = BlockingCopilotTokenExchanger::with_url(&url);
        let err = exchanger.exchange("gh-token").unwrap_err();

        assert!(err.contains("did not include a session token"), "{err}");
        server.join().unwrap();
    }

    #[test]
    fn exchange_invalid_json_is_error() {
        let (url, server) = serve_once("HTTP/1.1 200 OK", "not-json");
        let exchanger = BlockingCopilotTokenExchanger::with_url(&url);
        let err = exchanger.exchange("gh-token").unwrap_err();

        assert!(err.contains("response was invalid"), "{err}");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn exchange_async_from_tokio_runtime() {
        let (url, server) = serve_once(
            "HTTP/1.1 200 OK",
            r#"{"token":"tid=1;exp=9999999999","expires_at":9999999999}"#,
        );
        let exchanger: Arc<dyn CopilotTokenExchanger> =
            Arc::new(BlockingCopilotTokenExchanger::with_url(&url));
        let session = exchange_github_token_async(exchanger, "gh-token".into())
            .await
            .unwrap();

        assert_eq!(session.token, "tid=1;exp=9999999999");
        let request = server.join().unwrap().to_ascii_lowercase();
        assert!(
            request.contains("authorization: token gh-token"),
            "{request}"
        );
    }

    #[tokio::test]
    async fn session_manager_renews_only_when_expired() {
        let exchanger = Arc::new(CountingExchanger::new(i64::MAX));
        let sessions = CopilotSessionProvider::with_session(
            static_token_provider("gh-token"),
            exchanger.clone(),
            fresh_session(1),
        );

        let first = sessions.ensure_fresh().await.unwrap();
        assert_eq!(first.expires_at, Some(i64::MAX));
        assert_eq!(exchanger.calls.load(AtomicOrdering::SeqCst), 1);

        let second = sessions.ensure_fresh().await.unwrap();
        assert_eq!(second.expires_at, Some(i64::MAX));
        assert_eq!(exchanger.calls.load(AtomicOrdering::SeqCst), 1);
    }

    #[tokio::test]
    async fn middleware_replaces_authorization_header() {
        let exchanger = Arc::new(CountingExchanger::new(i64::MAX));
        let sessions = CopilotSessionProvider::with_session(
            static_token_provider("gh-token"),
            exchanger.clone(),
            fresh_session(i64::MAX),
        );
        let middleware = CopilotTokenRefreshMiddleware::new(Arc::new(sessions));
        let inner = rig_core::test_utils::RecordingHttpClient::new("");
        let client =
            rig_core::http_client::DynHttpClient::new(inner.clone()).with_middleware(middleware);
        let request = rig_core::http_client::Request::builder()
            .method(Method::GET)
            .uri(Uri::from_static("https://api.githubcopilot.com"))
            .body(rig_core::http_client::NoBody)
            .unwrap();

        client.send::<_, bytes::Bytes>(request).await.unwrap();

        let requests = inner.requests();
        assert_eq!(
            requests[0]
                .headers
                .get(http::header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer tid=1;exp=9999999999"
        );
    }

    #[tokio::test]
    async fn middleware_refreshes_expired_session() {
        let exchanger = Arc::new(CountingExchanger::new(i64::MAX));
        let sessions = CopilotSessionProvider::with_session(
            static_token_provider("gh-token"),
            exchanger.clone(),
            fresh_session(1),
        );
        let middleware = CopilotTokenRefreshMiddleware::new(Arc::new(sessions));
        let inner = rig_core::test_utils::RecordingHttpClient::new("");
        let client =
            rig_core::http_client::DynHttpClient::new(inner.clone()).with_middleware(middleware);
        let request = rig_core::http_client::Request::builder()
            .method(Method::GET)
            .uri(Uri::from_static("https://api.githubcopilot.com"))
            .body(rig_core::http_client::NoBody)
            .unwrap();

        client.send::<_, bytes::Bytes>(request).await.unwrap();

        assert_eq!(exchanger.calls.load(AtomicOrdering::SeqCst), 1);
        let requests = inner.requests();
        assert_eq!(
            requests[0]
                .headers
                .get(http::header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer tid=1;exp=9999999999"
        );
    }


    // ── parse_extra_params tests ─────────────────────────────────────

    #[test]
    fn extra_params_strict_tools_true() {
        let mut ep: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
        ep.insert("strict_tools".into(), "true".into());
        let (strict_tools, _) = parse_extra_params(&ep).unwrap();
        assert!(strict_tools);
    }

    #[test]
    fn extra_params_strict_tools_one() {
        let mut ep: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
        ep.insert("strict_tools".into(), "1".into());
        let (strict_tools, _) = parse_extra_params(&ep).unwrap();
        assert!(strict_tools);
    }

    #[test]
    fn extra_params_strict_tools_malformed_is_error() {
        let mut ep: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
        ep.insert("strict_tools".into(), "yes".into());
        let err = parse_extra_params(&ep).unwrap_err();
        assert!(err.contains("strict_tools"), "{err}");
    }

    #[test]
    fn extra_params_strict_tools_false_by_default() {
        let (strict_tools, _) = parse_extra_params(&indexmap::IndexMap::new()).unwrap();
        assert!(!strict_tools);
    }

    #[test]
    fn extra_params_tool_result_array_content_true() {
        let mut ep: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
        ep.insert("tool_result_array_content".into(), "true".into());
        let (_, tool_result_array_content) = parse_extra_params(&ep).unwrap();
        assert!(tool_result_array_content);
    }

    #[test]
    fn extra_params_tool_result_array_content_one() {
        let mut ep: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
        ep.insert("tool_result_array_content".into(), "1".into());
        let (_, tool_result_array_content) = parse_extra_params(&ep).unwrap();
        assert!(tool_result_array_content);
    }

    #[test]
    fn extra_params_tool_result_array_content_false_by_default() {
        let (_, tool_result_array_content) =
            parse_extra_params(&indexmap::IndexMap::new()).unwrap();
        assert!(!tool_result_array_content);
    }

    #[test]
    fn extra_params_intent_rejected_with_helpful_error() {
        let mut ep: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
        ep.insert("intent".into(), "edits".into());
        let err = parse_extra_params(&ep).unwrap_err();
        assert!(err.contains("intent"), "{err}");
        assert!(err.contains("removed in rig 0.46"), "{err}");
    }
}
