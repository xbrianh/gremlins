//! Live Copilot model catalog via rig's model-listing wire.

use std::collections::HashSet;

#[cfg(test)]
use std::sync::Arc;

use futures::future::BoxFuture;
use rig_core::driver::Model;
use rig_core::error::ProviderError;
use rig_core::http_client::{DynHttpClient, HttpClientExt};
use rig_core::model::ModelList;
use rig_core::providers::copilot::model_listing::Models;
use rig_core::providers::copilot::DIALECT;
use rig_core::providers::openai::OpenAIConfig;

use super::{
    resolve_catalog_session, CopilotSession, CopilotTokenRefreshMiddleware,
    NO_COPILOT_CREDENTIALS, COPILOT_APP_REFRESH_ERROR,
};
#[cfg(test)]
use super::{COPILOT_EDITOR_PLUGIN_VERSION, COPILOT_EDITOR_VERSION, COPILOT_USER_AGENT};

/// Copilot advertises chat, embedding and completion models together.
const CHAT_TYPE: &str = "chat";

/// The catalog request never reached Copilot.
const CATALOG_TRANSPORT_ERROR: &str =
    "Copilot catalog request failed; check your network connection";
/// Copilot answered, refusing the credential.
const CATALOG_REJECTED_ERROR: &str =
    "Copilot catalog rejected your credential; check your Copilot API key or OAuth credentials";
/// Copilot answered with a body this client cannot read.
const CATALOG_DECODE_ERROR: &str =
    "Copilot returned a catalog that could not be decoded";
/// Credential resolution on the blocking thread failed to complete.
const CATALOG_CREDENTIAL_ERROR: &str = "Copilot credential resolution task failed";
/// No credential source matched, so nothing could be sent.
const CREDENTIALS_MISSING_ERROR: &str =
    "no Copilot credentials: check GITHUB_COPILOT_API_KEY, COPILOT_API_KEY, \
     GITHUB_TOKEN, GH_TOKEN, or providers.yaml";
/// A stored-app (`client-id` + `refresh-token`) credential refresh failed.
const APP_REFRESH_ERROR: &str =
    "Copilot app credential refresh failed; check your client-id and refresh-token";
/// A GitHub credential was found, but the Copilot session exchange refused it.
const TOKEN_EXCHANGE_ERROR: &str =
    "Copilot token exchange failed; check your GitHub credential and Copilot access";

pub(in crate::clients::backends) fn list_models() -> BoxFuture<'static, Result<Vec<String>, String>> {
    Box::pin(async move { list_models_with(rig_reqwest::shared()).await })
}

async fn list_models_with(http: impl HttpClientExt + 'static) -> Result<Vec<String>, String> {
    let (session, middleware) = tokio::task::spawn_blocking(resolve_catalog_session)
        .await
        .map_err(|_| CATALOG_CREDENTIAL_ERROR.to_string())?
        .map_err(catalog_credential_message)?;
    list_models_for_session(http, session, middleware).await
}

/// Translate a credential-resolution failure into a locally-authored
/// diagnostic: absent credentials and a refused credential are different
/// remedies, and neither echoes the resolver's own text or any secret.
fn catalog_credential_message(error: String) -> String {
    if error.starts_with(NO_COPILOT_CREDENTIALS) {
        CREDENTIALS_MISSING_ERROR.to_string()
    } else if error.starts_with(COPILOT_APP_REFRESH_ERROR) {
        APP_REFRESH_ERROR.to_string()
    } else {
        TOKEN_EXCHANGE_ERROR.to_string()
    }
}

async fn list_models_for_session(
    http: impl HttpClientExt + 'static,
    session: CopilotSession,
    middleware: CopilotTokenRefreshMiddleware,
) -> Result<Vec<String>, String> {
    let transport = catalog_client(http, middleware);
    let config = catalog_config(&session);
    let list = Model::new(Models::new(config), transport)
        .list()
        .await
        .map_err(catalog_error_message)?;
    Ok(chat_model_ids(list))
}

/// Unique ids of chat models, in catalog order.
fn chat_model_ids(list: ModelList) -> Vec<String> {
    let mut seen = HashSet::new();
    list.data
        .into_iter()
        .filter(|model| model.r#type.as_deref() == Some(CHAT_TYPE))
        .map(|model| model.id)
        .filter(|id| !id.trim().is_empty() && seen.insert(id.clone()))
        .collect()
}

/// Translate a rig provider failure into a locally-authored diagnostic.
///
/// The credential never leaves the wire and the provider's own body or
/// description is never echoed: each branch names only what the operator can
/// act on.
fn catalog_error_message(error: ProviderError) -> String {
    if let Some(status) = error.provider_response_status() {
        return catalog_status_message(status);
    }
    match error {
        // `Json` is an encode/decode failure; `Response` is what the wire
        // reports for a reply whose document did not parse. Both carry the
        // provider's own diagnostic, which is never echoed.
        ProviderError::Json(_) | ProviderError::Response(_) => CATALOG_DECODE_ERROR.to_string(),
        ProviderError::InvalidAuthentication(_) => CATALOG_REJECTED_ERROR.to_string(),
        // A transport failure that still names a status: classify it as a
        // status reply rather than as a bare connection failure.
        ProviderError::Http(error) => match error.non_success_status() {
            Some(status) => catalog_status_message(status),
            None => CATALOG_TRANSPORT_ERROR.to_string(),
        },
        _ => CATALOG_TRANSPORT_ERROR.to_string(),
    }
}

fn catalog_status_message(status: http::StatusCode) -> String {
    match status {
        http::StatusCode::UNAUTHORIZED | http::StatusCode::FORBIDDEN => {
            CATALOG_REJECTED_ERROR.to_string()
        }
        other => format!("Copilot catalog request failed with status {}", other.as_u16()),
    }
}

fn catalog_client(
    http: impl HttpClientExt + 'static,
    middleware: CopilotTokenRefreshMiddleware,
) -> DynHttpClient {
    DynHttpClient::new(http).with_middleware(middleware)
}

fn catalog_config(session: &CopilotSession) -> OpenAIConfig {
    let config = OpenAIConfig::with_key(&DIALECT, session.token.clone());
    match &session.api_base {
        Some(api_base) => config.with_base_url(api_base.clone()),
        None => config,
    }
}

// ── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    use super::super::{
        CopilotTokenExchanger, NO_COPILOT_CREDENTIALS as RESOLVER_NO_CREDENTIALS,
        COPILOT_APP_REFRESH_ERROR as RESOLVER_APP_REFRESH_ERROR,
    };
    use super::*;
    use crate::test_support::EnvGuard;
    use rig_core::model::{ModelInfo, ModelList};
    use rig_core::test_utils::{CapturedHttpRequest, MockHttpResponse, SequencedHttpClient};

    const FAKE_SESSION_TOKEN: &str = "tid=1;exp=9999999999";

    fn session() -> CopilotSession {
        CopilotSession {
            token: FAKE_SESSION_TOKEN.into(),
            api_base: Some("https://assigned.example".into()),
            expires_at: Some(i64::MAX),
        }
    }

    fn chat(id: &str) -> ModelInfo {
        let mut model = ModelInfo::from_id(id);
        model.r#type = Some("chat".into());
        model
    }

    fn typed(id: &str, kind: &str) -> ModelInfo {
        let mut model = ModelInfo::from_id(id);
        model.r#type = Some(kind.into());
        model
    }

    /// Build a refresh middleware for a static session — a test-only helper
    /// so catalog tests never reach two levels up to
    /// `super::super::static_session_middleware`.
    fn test_static_middleware(session: CopilotSession) -> CopilotTokenRefreshMiddleware {
        super::super::static_session_middleware(session)
    }

    fn scrub_copilot_env(guard: &mut EnvGuard) {
        guard.remove("GITHUB_COPILOT_API_KEY");
        guard.remove("COPILOT_API_KEY");
        guard.remove("GITHUB_TOKEN");
        guard.remove("GH_TOKEN");
        guard.remove("XDG_CONFIG_HOME");
    }

    fn isolated_env() -> (EnvGuard, tempfile::TempDir) {
        let mut guard = EnvGuard::lock();
        scrub_copilot_env(&mut guard);
        let tmp = tempfile::tempdir().unwrap();
        guard.set("GREMLINS_SANDBOX_ROOT", tmp.path());
        guard.set("HOME", tmp.path());
        (guard, tmp)
    }

    fn request_uri(request: &CapturedHttpRequest) -> &str {
        request.uri.as_str()
    }

    fn auth_header(request: &CapturedHttpRequest) -> &str {
        request.headers["authorization"].to_str().unwrap()
    }

    // ── unit tests ────────────────────────────────────────────────────

    #[test]
    fn catalog_config_honors_assigned_api_base() {
        let config = catalog_config(&session());
        assert_eq!(config.base_url, "https://assigned.example");
    }

    #[test]
    fn catalog_config_falls_back_to_default_base() {
        let s = CopilotSession {
            token: FAKE_SESSION_TOKEN.into(),
            api_base: None,
            expires_at: Some(i64::MAX),
        };
        let config = catalog_config(&s);
        assert_eq!(config.base_url, DIALECT.base_url);
    }

    #[test]
    fn chat_model_ids_keeps_only_unique_chat_ids_in_order() {
        let list = ModelList::new(vec![
            chat("b"),
            typed("embed", "embedding"),
            chat("a"),
            chat("b"),
            chat("   "),
            ModelInfo::from_id("untyped"),
        ]);
        assert_eq!(chat_model_ids(list), ["b", "a"]);
        assert!(chat_model_ids(ModelList::new(Vec::new())).is_empty());
    }

    #[test]
    fn chat_model_ids_skips_missing_capabilities() {
        let list = ModelList::new(vec![
            ModelInfo::from_id("gpt-4o"),
            chat("claude-sonnet"),
            typed("text-embedding", "embedding"),
        ]);
        assert_eq!(chat_model_ids(list), ["claude-sonnet"]);
    }

    #[test]
    fn catalog_with_only_non_chat_models_is_empty() {
        let list = ModelList::new(vec![
            typed("embed", "embedding"),
            typed("complete", "completion"),
        ]);
        assert!(chat_model_ids(list).is_empty());
    }

    #[test]
    fn credential_failures_map_to_distinct_remedies() {
        assert_eq!(
            catalog_credential_message(format!("{NO_COPILOT_CREDENTIALS}: set GITHUB_TOKEN")),
            CREDENTIALS_MISSING_ERROR
        );
        assert_eq!(
            catalog_credential_message(
                format!("{RESOLVER_APP_REFRESH_ERROR}: GitHub App refresh failed")
            ),
            APP_REFRESH_ERROR
        );
        assert_eq!(
            catalog_credential_message(
                "GitHub Copilot token exchange failed with HTTP 401".into()
            ),
            TOKEN_EXCHANGE_ERROR
        );
        assert!(CREDENTIALS_MISSING_ERROR.contains("GITHUB_COPILOT_API_KEY"));
        assert!(CREDENTIALS_MISSING_ERROR.contains("GITHUB_TOKEN"));
        assert!(CREDENTIALS_MISSING_ERROR.contains("GH_TOKEN"));
        assert!(CREDENTIALS_MISSING_ERROR.contains("providers.yaml"));
        assert!(APP_REFRESH_ERROR.contains("client-id"), "{}", APP_REFRESH_ERROR);
        assert!(!APP_REFRESH_ERROR.contains("token exchange"));
    }

    #[test]
    fn exchange_failure_drops_raw_message() {
        let error = catalog_credential_message(
            "GitHub Copilot token exchange failed with HTTP 403: secret-oauth-error".into(),
        );
        assert_eq!(error, TOKEN_EXCHANGE_ERROR);
        assert!(!error.contains("secret-oauth-error"));
        assert!(!error.contains("403"));
    }

    // ── async listing tests ───────────────────────────────────────────

    #[tokio::test]
    async fn lists_chat_models_from_the_wire() {
        let s = session();
        let middleware = test_static_middleware(s.clone());
        let mock = SequencedHttpClient::new([MockHttpResponse::success(
            r#"{"data":[
                {"id":"gpt-4o","capabilities":{"type":"chat"}},
                {"id":"text-embedding-3-small","capabilities":{"type":"embedding"}},
                {"id":"gpt-4o","capabilities":{"type":"chat"}}]}"#,
        )]);

        let ids = list_models_for_session(DynHttpClient::new(mock.clone()), s, middleware)
            .await
            .unwrap();

        assert_eq!(ids, ["gpt-4o"]);
        assert_eq!(mock.requests()[0].uri, "https://assigned.example/models");
    }

    #[tokio::test]
    async fn catalog_request_includes_copilot_editor_headers() {
        let s = session();
        let middleware = test_static_middleware(s.clone());
        let mock =
            SequencedHttpClient::new([MockHttpResponse::success(r#"{"data":[]}"#)]);

        list_models_for_session(DynHttpClient::new(mock.clone()), s, middleware)
            .await
            .unwrap();

        let request = &mock.requests()[0];
        assert_eq!(
            auth_header(request),
            format!("Bearer {FAKE_SESSION_TOKEN}")
        );
        assert_eq!(
            request.headers["editor-version"].to_str().unwrap(),
            COPILOT_EDITOR_VERSION
        );
        assert_eq!(
            request.headers["editor-plugin-version"].to_str().unwrap(),
            COPILOT_EDITOR_PLUGIN_VERSION
        );
        assert_eq!(
            request.headers["user-agent"].to_str().unwrap(),
            COPILOT_USER_AGENT
        );
    }

    #[tokio::test]
    async fn catalog_request_bears_only_the_session_token() {
        let s = session();
        let middleware = test_static_middleware(s.clone());
        let mock =
            SequencedHttpClient::new([MockHttpResponse::success(r#"{"data":[]}"#)]);

        list_models_for_session(DynHttpClient::new(mock.clone()), s, middleware)
            .await
            .unwrap();

        assert_eq!(
            auth_header(&mock.requests()[0]),
            format!("Bearer {FAKE_SESSION_TOKEN}")
        );
    }

    #[tokio::test]
    async fn default_base_when_no_api_base_assigned() {
        let s = CopilotSession {
            token: FAKE_SESSION_TOKEN.into(),
            api_base: None,
            expires_at: Some(i64::MAX),
        };
        let middleware = test_static_middleware(s.clone());
        let mock =
            SequencedHttpClient::new([MockHttpResponse::success(r#"{"data":[]}"#)]);

        list_models_for_session(DynHttpClient::new(mock.clone()), s, middleware)
            .await
            .unwrap();

        assert!(request_uri(&mock.requests()[0]).ends_with("/models"));
    }

    // ── credential-selection tests ────────────────────────────────────

    #[tokio::test]
    async fn static_session_beats_github_token() {
        let (mut env, _tmp) = isolated_env();
        env.set("GITHUB_COPILOT_API_KEY", FAKE_SESSION_TOKEN);
        env.set("GITHUB_TOKEN", "github-token-secret");

        let calls = Arc::new(AtomicUsize::new(0));
        let exchanger: Arc<dyn CopilotTokenExchanger> =
            Arc::new(RecordExchange {
                calls: calls.clone(),
            });

        let (session, middleware) =
            super::super::resolve_session_for_test(|| Ok(exchanger.clone()))
                .unwrap();

        let mock = SequencedHttpClient::new([MockHttpResponse::success(
            r#"{"data":[{"id":"gpt-4o","capabilities":{"type":"chat"}}]}"#,
        )]);

        let ids = list_models_for_session(
            DynHttpClient::new(mock.clone()),
            session,
            middleware,
        )
        .await
        .unwrap();

        assert_eq!(ids, ["gpt-4o"]);
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(
            auth_header(&mock.requests()[0]),
            format!("Bearer {FAKE_SESSION_TOKEN}")
        );
        assert!(!auth_header(&mock.requests()[0]).contains("github-token"));
    }

    #[tokio::test]
    async fn github_token_exchanges_for_session_bearer() {
        let (mut env, _tmp) = isolated_env();
        env.set("GITHUB_TOKEN", "github-token-secret");

        let exchanger: Arc<dyn CopilotTokenExchanger> =
            Arc::new(FakeExchange {
                session_token: "exchanged-session-token".into(),
                api_base: Some("https://assigned.example".into()),
            });

        let (session, middleware) =
            super::super::resolve_session_for_test(|| Ok(exchanger.clone()))
                .unwrap();

        let mock = SequencedHttpClient::new([MockHttpResponse::success(
            r#"{"data":[{"id":"gpt-4o","capabilities":{"type":"chat"}}]}"#,
        )]);

        let ids = list_models_for_session(
            DynHttpClient::new(mock.clone()),
            session,
            middleware,
        )
        .await
        .unwrap();

        assert_eq!(ids, ["gpt-4o"]);
        assert_eq!(
            auth_header(&mock.requests()[0]),
            "Bearer exchanged-session-token"
        );
        assert!(!auth_header(&mock.requests()[0]).contains("github-token"));
        assert_eq!(
            request_uri(&mock.requests()[0]),
            "https://assigned.example/models"
        );
    }

    #[tokio::test]
    async fn default_base_when_exchange_has_no_api_base() {
        let (mut env, _tmp) = isolated_env();
        env.set("GITHUB_TOKEN", "github-token-secret");

        let exchanger: Arc<dyn CopilotTokenExchanger> =
            Arc::new(FakeExchange {
                session_token: "exchanged-session-token".into(),
                api_base: None,
            });

        let (session, middleware) =
            super::super::resolve_session_for_test(|| Ok(exchanger.clone()))
                .unwrap();

        let mock = SequencedHttpClient::new([MockHttpResponse::success(
            r#"{"data":[{"id":"gpt-4o","capabilities":{"type":"chat"}}]}"#,
        )]);

        list_models_for_session(DynHttpClient::new(mock.clone()), session, middleware)
            .await
            .unwrap();

        assert!(
            request_uri(&mock.requests()[0])
                .starts_with(DIALECT.base_url.trim_end_matches('/')),
            "{}",
            request_uri(&mock.requests()[0])
        );
    }

    // ── error-path tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn missing_credentials_exit_nonzero() {
        let (_env, _tmp) = isolated_env();

        let result = super::super::resolve_session_for_test(|| {
            Ok(Arc::new(FakeExchange {
                session_token: "unused".into(),
                api_base: None,
            }) as Arc<dyn CopilotTokenExchanger>)
        });

        let error = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        assert!(error.starts_with(RESOLVER_NO_CREDENTIALS));
        let sanitized = catalog_credential_message(error);
        assert_eq!(sanitized, CREDENTIALS_MISSING_ERROR);
        assert!(!sanitized.contains("secret"));
    }

    #[tokio::test]
    async fn rejected_exchange_drops_secrets() {
        let (mut env, _tmp) = isolated_env();
        env.set("GITHUB_TOKEN", "github-token-secret");

        let exchanger: Arc<dyn CopilotTokenExchanger> =
            Arc::new(FailingExchange {
                message:
                    "GitHub Copilot token exchange failed with HTTP 403: secret-denied".into(),
            });

        let result =
            super::super::resolve_session_for_test(|| Ok(exchanger));

        let error = match result {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        let sanitized = catalog_credential_message(error);
        assert_eq!(sanitized, TOKEN_EXCHANGE_ERROR);
        assert!(!sanitized.contains("secret-denied"));
        assert!(!sanitized.contains("403"));
        assert!(!sanitized.contains("github-token-secret"));
    }

    #[tokio::test]
    async fn rejected_credential_names_no_secret_or_body() {
        let s = session();
        let middleware = test_static_middleware(s.clone());
        let mock = SequencedHttpClient::new([MockHttpResponse::ErrorResponse(
            http::StatusCode::UNAUTHORIZED,
            "secret-body".into(),
        )]);

        let error =
            list_models_for_session(DynHttpClient::new(mock.clone()), s, middleware)
                .await
                .unwrap_err();

        assert_eq!(error, CATALOG_REJECTED_ERROR);
        assert!(!error.contains("secret-body"));
        assert!(!error.contains("tid="));
    }

    #[tokio::test]
    async fn forbidden_reports_credential_remedy() {
        let s = session();
        let middleware = test_static_middleware(s.clone());
        let mock = SequencedHttpClient::new([MockHttpResponse::ErrorResponse(
            http::StatusCode::FORBIDDEN,
            "secret-body".into(),
        )]);

        let error =
            list_models_for_session(DynHttpClient::new(mock.clone()), s, middleware)
                .await
                .unwrap_err();

        assert_eq!(error, CATALOG_REJECTED_ERROR);
        assert!(!error.contains("secret-body"));
        assert!(!error.contains("FORBIDDEN"));
    }

    #[tokio::test]
    async fn other_status_reports_the_code_only() {
        let s = session();
        let middleware = test_static_middleware(s.clone());
        let mock = SequencedHttpClient::new([MockHttpResponse::ErrorResponse(
            http::StatusCode::INTERNAL_SERVER_ERROR,
            "secret-body".into(),
        )]);

        let error =
            list_models_for_session(DynHttpClient::new(mock.clone()), s, middleware)
                .await
                .unwrap_err();

        assert_eq!(error, "Copilot catalog request failed with status 500");
        assert!(!error.contains("secret-body"));
    }

    #[tokio::test]
    async fn undecodable_catalog_reports_a_local_message() {
        let s = session();
        let middleware = test_static_middleware(s.clone());
        let mock =
            SequencedHttpClient::new([MockHttpResponse::success("not-json")]);

        let error =
            list_models_for_session(DynHttpClient::new(mock.clone()), s, middleware)
                .await
                .unwrap_err();

        assert_eq!(error, CATALOG_DECODE_ERROR);
    }

    // ── mixed-catalog tests ───────────────────────────────────────────

    #[tokio::test]
    async fn mixed_catalog_yields_only_chat_ids_in_order() {
        let s = session();
        let middleware = test_static_middleware(s.clone());
        let catalog = serde_json::json!({
            "data": [
                {"id": "claude-3.5-sonnet",    "name": "Claude 3.5 Sonnet",  "vendor": "anthropic",  "capabilities": {"type": "chat"}},
                {"id": "gpt-4o",               "name": "GPT-4o",              "vendor": "openai",     "capabilities": {"type": "chat"}},
                {"id": "text-embedding-3",     "name": "Text Embedding 3",   "vendor": "openai",     "capabilities": {"type": "embedding"}},
                {"id": "gpt-4o",               "name": "GPT-4o",              "vendor": "openai",     "capabilities": {"type": "chat"}},
                {"id": "   ",                  "name": "Whitespace",          "vendor": "",            "capabilities": {"type": "chat"}},
                {"id": "o3-mini",              "name": "o3-mini",             "vendor": "openai",     "capabilities": null},
                {"id": "gpt-4.1",              "capabilities": {}},
                {"id": "gpt-5-sonnet",         "capabilities": {"type": "chat"}}
            ]
        });
        let mock = SequencedHttpClient::new([MockHttpResponse::success(
            catalog.to_string(),
        )]);

        let ids =
            list_models_for_session(DynHttpClient::new(mock), s, middleware)
                .await
                .unwrap();

        assert_eq!(ids, ["claude-3.5-sonnet", "gpt-4o", "gpt-5-sonnet"]);
    }

    #[tokio::test]
    async fn valid_catalog_with_no_chat_models_succeeds_empty() {
        let s = session();
        let middleware = test_static_middleware(s.clone());
        let mock = SequencedHttpClient::new([MockHttpResponse::success(
            r#"{"data":[{"id":"embed","capabilities":{"type":"embedding"}}]}"#,
        )]);

        let ids =
            list_models_for_session(DynHttpClient::new(mock), s, middleware)
                .await
                .unwrap();

        assert!(ids.is_empty());
    }

    // ── test-double implementations ───────────────────────────────────

    struct RecordExchange {
        calls: Arc<AtomicUsize>,
    }

    impl CopilotTokenExchanger for RecordExchange {
        fn exchange(&self, _access_token: &str) -> Result<CopilotSession, String> {
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            Err("should not be called".into())
        }
    }

    struct FakeExchange {
        session_token: String,
        api_base: Option<String>,
    }

    impl CopilotTokenExchanger for FakeExchange {
        fn exchange(&self, access_token: &str) -> Result<CopilotSession, String> {
            assert!(!access_token.is_empty());
            Ok(CopilotSession {
                token: self.session_token.clone(),
                api_base: self.api_base.clone(),
                expires_at: Some(i64::MAX),
            })
        }
    }

    struct FailingExchange {
        message: String,
    }

    impl CopilotTokenExchanger for FailingExchange {
        fn exchange(&self, _access_token: &str) -> Result<CopilotSession, String> {
            Err(self.message.clone())
        }
    }
}
