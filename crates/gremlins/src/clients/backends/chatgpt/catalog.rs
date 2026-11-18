//! Live ChatGPT subscription model catalog.

use std::collections::HashSet;

use futures::future::BoxFuture;
use http::{header::AUTHORIZATION, HeaderValue, Request, StatusCode};
use rig_core::http_client::{DynHttpClient, HttpClientExt, NoBody};
use rig_core::providers::chatgpt;
use serde::Deserialize;

use crate::clients::chatgpt_auth::{resolve_stored, static_source, stored_oauth};

/// Codex release whose catalog protocol this request follows
/// (<https://github.com/openai/codex/blob/main/codex-rs/codex-api/src/endpoint/models.rs>).
const CLIENT_VERSION: &str = "0.98.0";

/// Models with this `visibility` are offered in pickers.
const LISTED: &str = "list";

#[derive(Deserialize)]
struct Catalog {
    models: Vec<Entry>,
}

#[derive(Deserialize)]
struct Entry {
    slug: String,
    visibility: Option<String>,
}

/// Credentials resolved for one catalog request.
struct Access {
    /// Sensitive `Authorization` header; the raw token never leaves construction.
    authorization: HeaderValue,
    account_id: Option<String>,
    api_base: Option<String>,
    /// Remedy shown when the provider rejects the credential.
    rejected: String,
}

pub(crate) fn list_models() -> BoxFuture<'static, Result<Vec<String>, String>> {
    Box::pin(list_models_with(rig_reqwest::shared()))
}

pub(super) async fn list_models_with(http: DynHttpClient) -> Result<Vec<String>, String> {
    let access = resolve_access(&http).await?;
    let request = Request::builder()
        .method(http::Method::GET)
        .uri(format!(
            "{}/models?client_version={CLIENT_VERSION}",
            effective_base(access.api_base)
        ))
        .header(AUTHORIZATION, access.authorization)
        .body(NoBody)
        .map_err(|_| "could not build the ChatGPT catalog request".to_string())?;
    let request = match access.account_id {
        Some(id) => with_account(request, &id)?,
        None => request,
    };
    let response = http
        .send::<_, Vec<u8>>(request)
        .await
        .map_err(|_| "ChatGPT catalog request failed; check your network connection".to_string())?;
    let status = response.status();
    if status == StatusCode::UNAUTHORIZED {
        return Err(access.rejected);
    }
    if !status.is_success() {
        return Err(format!("ChatGPT catalog request failed with status {status}"));
    }
    let body = response
        .into_body()
        .await
        .map_err(|_| "could not read the ChatGPT catalog response".to_string())?;
    visible_slugs(&body)
}

async fn resolve_access(http: &DynHttpClient) -> Result<Access, String> {
    if let Some(found) = static_source() {
        return Ok(Access {
            authorization: bearer(&found.secret)?,
            account_id: None,
            api_base: None,
            rejected: format!(
                "ChatGPT rejected the credential from {}; replace or remove it",
                found.source
            ),
        });
    }
    let store = stored_oauth().map_err(|error| error.to_string())?;
    let context = resolve_stored(&store, http)
        .await
        .map_err(|error| error.to_string())?;
    Ok(Access {
        authorization: bearer(context.api_key.expose())?,
        account_id: context.account_id,
        api_base: context.api_base,
        rejected: "ChatGPT rejected the sign-in: run `gremlins auth chatgpt login`".into(),
    })
}

/// The Codex API base: the sign-in override, else the default (as the run path).
fn effective_base(api_base: Option<String>) -> String {
    api_base
        .unwrap_or_else(|| chatgpt::DIALECT.base_url.to_string())
        .trim_end_matches('/')
        .to_string()
}

fn bearer(token: &str) -> Result<HeaderValue, String> {
    let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| "the ChatGPT credential is not a valid header value".to_string())?;
    value.set_sensitive(true);
    Ok(value)
}

fn with_account(mut request: Request<NoBody>, id: &str) -> Result<Request<NoBody>, String> {
    let value = HeaderValue::from_str(id)
        .map_err(|_| "the ChatGPT account id is not a valid header value".to_string())?;
    request.headers_mut().insert("chatgpt-account-id", value);
    Ok(request)
}

/// Unique slugs of listed models, in catalog order.
fn visible_slugs(body: &[u8]) -> Result<Vec<String>, String> {
    let catalog: Catalog = serde_json::from_slice(body)
        .map_err(|_| "ChatGPT returned a catalog that could not be decoded".to_string())?;
    let mut seen = HashSet::new();
    Ok(catalog
        .models
        .into_iter()
        .filter(|entry| entry.visibility.as_deref() == Some(LISTED))
        .filter(|entry| seen.insert(entry.slug.clone()))
        .map(|entry| entry.slug)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::chatgpt_auth::ACCESS_TOKEN_ENV;
    use crate::test_support::EnvGuard;
    use rig_core::test_utils::{MockHttpResponse, SequencedHttpClient};

    #[test]
    fn keeps_unique_listed_slugs_in_order() {
        let body = br#"{"models":[
            {"slug":"b","visibility":"list","extra":1},
            {"slug":"hidden","visibility":"hide"},
            {"slug":"a","visibility":"list"},
            {"slug":"b","visibility":"list"}]}"#;
        assert_eq!(visible_slugs(body).unwrap(), ["b", "a"]);
        assert!(visible_slugs(br#"{"models":[]}"#).unwrap().is_empty());
        assert!(visible_slugs(b"{}").is_err());
    }

    #[tokio::test]
    async fn static_token_requests_catalog() {
        let mut env = EnvGuard::lock();
        let root = tempfile::tempdir().unwrap();
        env.set("GREMLINS_SANDBOX_ROOT", root.path());
        env.set(ACCESS_TOKEN_ENV, "tok");
        let mock = SequencedHttpClient::new([MockHttpResponse::success(
            r#"{"models":[{"slug":"m","visibility":"list"}]}"#,
        )]);
        let slugs = list_models_with(DynHttpClient::new(mock.clone())).await.unwrap();
        assert_eq!(slugs, ["m"]);
        let request = &mock.requests()[0];
        assert_eq!(
            request.uri,
            format!(
                "{}/models?client_version={CLIENT_VERSION}",
                chatgpt::DIALECT.base_url
            )
        );
        assert_eq!(request.headers["authorization"].to_str().unwrap(), "Bearer tok");
    }

    const BODY: &str = r#"{"models":[{"slug":"m","visibility":"list"}]}"#;

    fn isolated(root: &std::path::Path) -> EnvGuard {
        let mut env = EnvGuard::lock();
        env.set("GREMLINS_SANDBOX_ROOT", root);
        env.remove(ACCESS_TOKEN_ENV);
        env
    }

    fn seed(record: serde_json::Value) {
        let dir = crate::clients::chatgpt_auth::store_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("chatgpt.json"), record.to_string()).unwrap();
    }

    fn header<'a>(request: &'a rig_core::test_utils::CapturedHttpRequest, name: &str) -> &'a str {
        request.headers[name].to_str().unwrap()
    }

    #[tokio::test]
    async fn live_oauth_sends_bearer_and_account() {
        let root = tempfile::tempdir().unwrap();
        let _env = isolated(root.path());
        seed(serde_json::json!({"access_token":"live","account_id":"acct","expires_at":i64::MAX}));
        let mock = SequencedHttpClient::new([MockHttpResponse::success(BODY)]);
        let slugs = list_models_with(DynHttpClient::new(mock.clone())).await.unwrap();
        assert_eq!(slugs, ["m"]);
        let request = &mock.requests()[0];
        assert_eq!(
            request.uri,
            format!("{}/models?client_version={CLIENT_VERSION}", chatgpt::DIALECT.base_url)
        );
        assert_eq!(header(request, "authorization"), "Bearer live");
        assert_eq!(header(request, "chatgpt-account-id"), "acct");
    }

    #[tokio::test]
    async fn expired_oauth_refreshes_before_request() {
        let root = tempfile::tempdir().unwrap();
        let _env = isolated(root.path());
        seed(serde_json::json!({
            "access_token":"old","refresh_token":"r","account_id":"acct","expires_at":0
        }));
        let mock = SequencedHttpClient::new([
            MockHttpResponse::success(r#"{"access_token":"fresh","refresh_token":"r2"}"#),
            MockHttpResponse::success(BODY),
        ]);
        list_models_with(DynHttpClient::new(mock.clone())).await.unwrap();
        let requests = mock.requests();
        assert_eq!(requests[0].uri, "https://auth.openai.com/oauth/token");
        assert_eq!(header(&requests[1], "authorization"), "Bearer fresh");
    }

    #[tokio::test]
    async fn env_token_beats_stored_oauth() {
        let root = tempfile::tempdir().unwrap();
        let mut env = isolated(root.path());
        seed(serde_json::json!({"access_token":"live","account_id":"acct","expires_at":i64::MAX}));
        env.set(ACCESS_TOKEN_ENV, "env-tok");
        let mock = SequencedHttpClient::new([MockHttpResponse::success(BODY)]);
        list_models_with(DynHttpClient::new(mock.clone())).await.unwrap();
        let request = &mock.requests()[0];
        assert_eq!(header(request, "authorization"), "Bearer env-tok");
        assert!(!request.headers.contains_key("chatgpt-account-id"));
    }

    #[tokio::test]
    async fn configured_token_beats_stored_oauth() {
        let root = tempfile::tempdir().unwrap();
        let _env = isolated(root.path());
        seed(serde_json::json!({"access_token":"live","expires_at":i64::MAX}));
        std::fs::write(
            root.path().join("config/providers.yaml"),
            "chatgpt:\n  token: cfg-tok\n",
        )
        .unwrap();
        let mock = SequencedHttpClient::new([MockHttpResponse::success(BODY)]);
        list_models_with(DynHttpClient::new(mock.clone())).await.unwrap();
        assert_eq!(header(&mock.requests()[0], "authorization"), "Bearer cfg-tok");
    }

    #[tokio::test]
    async fn rejected_credentials_name_their_own_remedy() {
        let root = tempfile::tempdir().unwrap();
        let mut env = isolated(root.path());
        seed(serde_json::json!({"access_token":"stored-secret","expires_at":i64::MAX}));
        let rejected = || SequencedHttpClient::new([MockHttpResponse::ErrorResponse(StatusCode::UNAUTHORIZED, "raw-body".into())]);

        let error = list_models_with(DynHttpClient::new(rejected())).await.unwrap_err();
        assert!(error.contains("gremlins auth chatgpt login"), "{error}");
        assert!(!error.contains("stored-secret") && !error.contains("raw-body"));

        env.set(ACCESS_TOKEN_ENV, "env-secret");
        let error = list_models_with(DynHttpClient::new(rejected())).await.unwrap_err();
        assert!(error.contains(ACCESS_TOKEN_ENV) && !error.contains("login"), "{error}");
        assert!(!error.contains("env-secret") && !error.contains("raw-body"));
    }
}
