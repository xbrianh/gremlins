//! Live Copilot model catalog via rig's model-listing wire.

use std::collections::HashSet;



use futures::future::BoxFuture;
use rig_core::driver::Model;
use rig_core::error::ProviderError;
use rig_core::http_client::HttpClientExt;
use rig_core::model::ModelList;
use rig_core::providers::copilot::model_listing::Models;

use super::{
    resolve_catalog_session, CopilotSession, CopilotTokenRefreshMiddleware,
    COPILOT_APP_REFRESH_ERROR, NO_COPILOT_CREDENTIALS,
};

/// Copilot advertises chat, embedding and completion models together.
const CHAT_TYPE: &str = "chat";

/// The catalog request never reached Copilot.
const CATALOG_TRANSPORT_ERROR: &str =
    "Copilot catalog request failed; check your network connection";
/// Copilot answered, refusing the credential. The status code is rendered
/// inline by [`catalog_status_message`]; this text supplies the remedy hint.
const CATALOG_REJECTED_REMEDY: &str = "check your Copilot API key or OAuth credentials";
/// Copilot answered, refusing the credential (status unknown).
const CATALOG_REJECTED_ERROR: &str =
    "Copilot catalog rejected your credential; check your Copilot API key or OAuth credentials";
/// Copilot answered with a body this client cannot read.
const CATALOG_DECODE_ERROR: &str = "Copilot returned a catalog that could not be decoded";
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

pub(in crate::clients::backends) fn list_models() -> BoxFuture<'static, Result<Vec<String>, String>>
{
    Box::pin(async {
        let (session, middleware) = tokio::task::spawn_blocking(resolve_catalog_session)
            .await
            .map_err(|_| CATALOG_CREDENTIAL_ERROR.to_string())?
            .map_err(catalog_credential_message)?;
        list_models_for_session(rig_reqwest::shared(), session, middleware).await
    })
}

/// Translate a credential-resolution failure into a locally-authored
/// diagnostic: absent credentials, a failed app refresh, and a rejected
/// token exchange are different remedies, and the HTTP status from the
/// exchange is preserved so the operator can distinguish 401/403 from
/// transient server errors.  No provider description or body is ever echoed.
fn catalog_credential_message(error: String) -> String {
    if error.starts_with(NO_COPILOT_CREDENTIALS) {
        return CREDENTIALS_MISSING_ERROR.to_string();
    }
    if let Some(suffix) = error.strip_prefix(COPILOT_APP_REFRESH_ERROR) {
        if let Some(status) = suffix
            .strip_prefix(": ")
            .and_then(|value| http::StatusCode::from_bytes(value.as_bytes()).ok())
        {
            return format!("{APP_REFRESH_ERROR} (HTTP {})", status.as_u16());
        }
        return APP_REFRESH_ERROR.to_string();
    }
    // Token-exchange failures carry an HTTP status from the blocking
    // exchanger; parse it out so the diagnostic names the code.
    if let Some(status) = exchange_error_status(&error) {
        return format!(
            "Copilot token exchange failed with HTTP {status}; \
             check your GitHub credential and Copilot access"
        );
    }
    TOKEN_EXCHANGE_ERROR.to_string()
}

/// Extract the HTTP status code embedded in an exchange error string.
///
/// The production exchanger renders errors as
/// `"GitHub Copilot token exchange failed with HTTP {status}…"`.
fn exchange_error_status(error: &str) -> Option<u16> {
    let prefix = "GitHub Copilot token exchange failed with HTTP ";
    let rest = error.strip_prefix(prefix)?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

async fn list_models_for_session(
    http: impl HttpClientExt + 'static,
    session: CopilotSession,
    middleware: CopilotTokenRefreshMiddleware,
) -> Result<Vec<String>, String> {
    #[cfg(feature = "copilot-test-support")]
    let session = CopilotSession {
        api_base: session
            .api_base
            .or_else(crate::clients::copilot_test_catalog_base),
        ..session
    };
    let (config, transport) = super::authenticated_copilot_parts(http, session, middleware);
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
        ProviderError::Http(error) => {
            if let rig_core::http_client::Error::Instance(source) = error.as_ref() {
                return match source.downcast_ref::<super::RenewalError>() {
                    Some(super::RenewalError::AppRefresh(Some(status))) => {
                        format!("{APP_REFRESH_ERROR} (HTTP {})", status.as_u16())
                    }
                    Some(super::RenewalError::AppRefresh(None)) => APP_REFRESH_ERROR.to_owned(),
                    Some(super::RenewalError::Exchange(message)) => {
                        catalog_credential_message(message.clone())
                    }
                    None => CATALOG_TRANSPORT_ERROR.to_owned(),
                };
            }
            match error.non_success_status() {
                Some(status) => catalog_status_message(status),
                None => CATALOG_TRANSPORT_ERROR.to_string(),
            }
        }
        _ => CATALOG_TRANSPORT_ERROR.to_string(),
    }
}

fn catalog_status_message(status: http::StatusCode) -> String {
    match status {
        http::StatusCode::UNAUTHORIZED | http::StatusCode::FORBIDDEN => {
            format!(
                "Copilot catalog rejected your credential (HTTP {}); {CATALOG_REJECTED_REMEDY}",
                status.as_u16()
            )
        }
        other => format!(
            "Copilot catalog request failed with status {}",
            other.as_u16()
        ),
    }
}

// ── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::{CopilotSessionProvider, CopilotTokenExchanger, StaticTokenProvider, static_session, static_session_middleware};
    use std::sync::Arc;
    use rig_core::http_client::DynHttpClient;
    use rig_core::providers::copilot::DIALECT;
    use rig_core::test_utils::{MockHttpResponse, SequencedHttpClient};

    #[tokio::test]
    async fn unassigned_session_requests_the_real_default_url() {
        let session = static_session("static".into());
        let http = SequencedHttpClient::new([MockHttpResponse::success(r#"{"data":[]}"#)]);
        list_models_for_session(DynHttpClient::new(http.clone()), session.clone(), static_session_middleware(session)).await.unwrap();
        assert_eq!(http.requests()[0].uri, format!("{}/models", DIALECT.base_url.trim_end_matches('/')));
    }

    struct FailingExchange;
    impl CopilotTokenExchanger for FailingExchange {
        fn exchange(&self, _: &str) -> Result<CopilotSession, String> {
            Err("GitHub Copilot token exchange failed with HTTP 403: secret-body".into())
        }
    }

    #[tokio::test]
    async fn renewal_rejection_is_an_exchange_error_not_a_catalog_failure() {
        let expired = CopilotSession { expires_at: Some(0), ..static_session("expired".into()) };
        let provider = CopilotSessionProvider::with_session(
            Arc::new(StaticTokenProvider {
                token: "gh-secret".into(),
            }),
            Arc::new(FailingExchange),
            expired.clone(),
        );
        let mock = SequencedHttpClient::new([]);
        let err = list_models_for_session(
            DynHttpClient::new(mock.clone()),
            expired,
            CopilotTokenRefreshMiddleware::new(Arc::new(provider)),
        )
        .await
        .unwrap_err();
        assert_eq!(err, "Copilot token exchange failed with HTTP 403; check your GitHub credential and Copilot access");
        assert!(mock.requests().is_empty());
    }
}
