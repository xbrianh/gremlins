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
}
