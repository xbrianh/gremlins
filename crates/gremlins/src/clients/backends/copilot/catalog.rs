use std::collections::HashSet;

use futures::future::BoxFuture;
use http::{header::AUTHORIZATION, HeaderValue, Request};
use rig_core::http_client::{DynHttpClient, HttpClientExt, NoBody};
use rig_core::providers::copilot::DIALECT;
use serde::Deserialize;

use super::{resolve_catalog_session, CopilotTokenRefreshMiddleware};

#[derive(Deserialize)]
struct Catalog { models: Vec<Entry> }
#[derive(Deserialize)]
struct Entry { id: String, capabilities: Option<Capabilities> }
#[derive(Deserialize)]
struct Capabilities { r#type: Option<String> }

pub(in crate::clients::backends) fn list_models() -> BoxFuture<'static, Result<Vec<String>, String>> {
    Box::pin(async move { list_models_with(rig_reqwest::shared()).await })
}

async fn list_models_with(http: impl rig_core::http_client::HttpClientExt + 'static) -> Result<Vec<String>, String> {
    let (session, middleware) = tokio::task::spawn_blocking(resolve_catalog_session)
        .await.map_err(|_| "Copilot credential resolution task failed".to_string())??;
    list_models_for_session(http, session, middleware).await
}

async fn list_models_for_session(
    http: impl rig_core::http_client::HttpClientExt + 'static,
    session: super::CopilotSession,
    middleware: CopilotTokenRefreshMiddleware,
) -> Result<Vec<String>, String> {
    let transport = copilot_transport_with(http, middleware);
    let base = session.api_base.unwrap_or_else(|| DIALECT.base_url.to_string());
    let request = Request::builder().method(http::Method::GET)
        .uri(format!("{}/models", base.trim_end_matches('/')))
        .header(AUTHORIZATION, bearer(&session.token)?)
        .header("editor-version", super::COPILOT_EDITOR_VERSION)
        .header("editor-plugin-version", super::COPILOT_EDITOR_PLUGIN_VERSION)
        .header("user-agent", super::COPILOT_USER_AGENT)
        .body(NoBody).map_err(|_| "could not build the Copilot catalog request".to_string())?;
    let response = transport.send::<_, Vec<u8>>(request).await
        .map_err(|_| "Copilot catalog request failed; check your network connection".to_string())?;
    let status = response.status();
    if !status.is_success() { return Err(format!("Copilot catalog request failed with status {status}")); }
    let body = response.into_body().await.map_err(|_| "could not read the Copilot catalog response".to_string())?;
    let catalog: Catalog = serde_json::from_slice(&body).map_err(|_| "Copilot returned a catalog that could not be decoded".to_string())?;
    let mut seen = HashSet::new();
    Ok(catalog.models.into_iter().filter(|m| m.capabilities.as_ref().and_then(|c| c.r#type.as_deref()) == Some("chat"))
        .filter(|m| !m.id.trim().is_empty() && seen.insert(m.id.clone())).map(|m| m.id).collect())
}

fn bearer(token: &str) -> Result<HeaderValue, String> {
    let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| "Copilot session credential is not a valid header value".to_string())?;
    value.set_sensitive(true);
    Ok(value)
}

fn copilot_transport_with(http: impl rig_core::http_client::HttpClientExt + 'static, middleware: CopilotTokenRefreshMiddleware) -> DynHttpClient {
    DynHttpClient::new(http).with_middleware(middleware)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clients::backends::copilot::{
        CopilotSession, CopilotSessionProvider, CopilotTokenExchanger,
        StaticTokenProvider,
    };
    use std::sync::Arc;
    use rig_core::test_utils::{MockHttpResponse, SequencedHttpClient};

    struct NoExchange;

    impl CopilotTokenExchanger for NoExchange {
        fn exchange(&self, _: &str) -> Result<CopilotSession, String> {
            panic!("static session must not be exchanged")
        }
    }

    fn middleware(token: &str) -> CopilotTokenRefreshMiddleware {
        let session = CopilotSession {
            token: token.to_string(),
            api_base: None,
            expires_at: Some(i64::MAX),
        };
        CopilotTokenRefreshMiddleware::new(Arc::new(CopilotSessionProvider::with_session(
            Arc::new(StaticTokenProvider { token: String::new() }),
            Arc::new(NoExchange),
            session,
        )))
    }

    #[test]
    fn parses_mixed_catalog_entries_in_order_and_deduplicates_chat_models() {
        let body = br#"{"models":[
            {"id":"chat-b","capabilities":{"type":"chat"},"extra":true},
            {"id":"completion","capabilities":{"type":"completion"}},
            {"id":"missing"},
            {"id":"chat-a","capabilities":{"type":"chat"}},
            {"id":"chat-b","capabilities":{"type":"chat"}},
            {"id":" ","capabilities":{"type":"chat"}},
            {"id":"no-type","capabilities":{}}
        ]}"#;
        let catalog: Catalog = serde_json::from_slice(body).unwrap();
        let mut seen = HashSet::new();
        let models: Vec<_> = catalog.models.into_iter()
            .filter(|m| m.capabilities.as_ref().and_then(|c| c.r#type.as_deref()) == Some("chat"))
            .filter(|m| !m.id.trim().is_empty() && seen.insert(m.id.clone()))
            .map(|m| m.id).collect();
        assert_eq!(models, ["chat-b", "chat-a"]);
    }

    #[test]
    fn bearer_rejects_invalid_header_and_is_sensitive() {
        let header = bearer("static-token").unwrap();
        assert_eq!(header.to_str().unwrap(), "Bearer static-token");
        assert!(header.is_sensitive());
        assert!(bearer("bad\ntoken").is_err());
    }

    #[tokio::test]
    async fn static_bearer_catalog_request_uses_mock_transport() {
        let body = r#"{"models":[{"id":"model","capabilities":{"type":"chat"}}]}"#;
        let mock = SequencedHttpClient::new([MockHttpResponse::success(body)]);
        let session = CopilotSession { token: "static-token".into(), api_base: Some("https://assigned.example".into()), expires_at: Some(i64::MAX) };
        let result = list_models_for_session(mock.clone(), session, middleware("static-token")).await.unwrap();
        assert_eq!(result, ["model"]);
        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.uri, "https://assigned.example/models");
        assert_eq!(request.headers["authorization"].to_str().unwrap(), "Bearer static-token");
        assert_eq!(request.headers["editor-version"].to_str().unwrap(), super::super::COPILOT_EDITOR_VERSION);
        assert_eq!(request.headers["editor-plugin-version"].to_str().unwrap(), super::super::COPILOT_EDITOR_PLUGIN_VERSION);
        assert_eq!(request.headers["user-agent"].to_str().unwrap(), super::super::COPILOT_USER_AGENT);
    }

    #[tokio::test]
    async fn malformed_catalog_is_rejected() {
        let mock = SequencedHttpClient::new([MockHttpResponse::success("not json")]);
        let transport = copilot_transport_with(mock, middleware("token"));
        let request = Request::builder().uri("https://example.test/models").body(NoBody).unwrap();
        let response = transport.send::<_, Vec<u8>>(request).await.unwrap();
        assert!(serde_json::from_slice::<Catalog>(&response.into_body().await.unwrap()).is_err());
    }
}
