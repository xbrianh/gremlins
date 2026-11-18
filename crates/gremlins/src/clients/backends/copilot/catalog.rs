use std::collections::HashSet;

use futures::future::BoxFuture;
use http::{header::AUTHORIZATION, Request};
use rig_core::http_client::{DynHttpClient, HttpClientExt, NoBody};
use rig_core::providers::copilot::DIALECT;
use rig_core::providers::openai::OpenAIConfig;
use serde::Deserialize;

use super::{resolve_catalog_session, CopilotSession, CopilotTokenRefreshMiddleware};

#[derive(Deserialize)]
struct Catalog {
    data: Vec<Entry>,
}

#[derive(Deserialize)]
struct Entry {
    id: String,
    #[serde(default)]
    capabilities: Capabilities,
}

#[derive(Default, Deserialize)]
struct Capabilities {
    #[serde(rename = "type")]
    kind: Option<String>,
}

pub(in crate::clients::backends) fn list_models() -> BoxFuture<'static, Result<Vec<String>, String>> {
    Box::pin(async move { list_models_with(rig_reqwest::shared()).await })
}

async fn list_models_with(
    http: impl HttpClientExt + 'static,
) -> Result<Vec<String>, String> {
    let (session, middleware) = tokio::task::spawn_blocking(resolve_catalog_session)
        .await
        .map_err(|_| "Copilot credential resolution task failed".to_string())??;
    list_models_for_session(http, session, middleware).await
}

async fn list_models_for_session(
    http: impl HttpClientExt + 'static,
    session: CopilotSession,
    middleware: CopilotTokenRefreshMiddleware,
) -> Result<Vec<String>, String> {
    let client = catalog_client(http, session.clone(), middleware);
    let base_url = catalog_config(&session).base_url;
    let request = Request::builder()
        .method(http::Method::GET)
        .uri(format!("{}/models", base_url.trim_end_matches('/')))
        .header(AUTHORIZATION, format!("Bearer {}", session.token))
        .body(NoBody)
        .map_err(|_| "could not build the Copilot catalog request".to_string())?;
    let response = client
        .send::<_, Vec<u8>>(request)
        .await
        .map_err(|_| "Copilot catalog request failed; check your Copilot session credential and GitHub account Copilot access".to_string())?;
    if !response.status().is_success() {
        return Err(format!("Copilot catalog request failed with status {}", response.status()));
    }
    let body = response
        .into_body()
        .await
        .map_err(|_| "could not read the Copilot catalog response".to_string())?;
    let catalog: Catalog = serde_json::from_slice(&body)
        .map_err(|_| "Copilot returned a catalog that could not be decoded".to_string())?;
    let mut seen = HashSet::new();
    Ok(catalog
        .data
        .into_iter()
        .filter(|model| model.capabilities.kind.as_deref() == Some("chat"))
        .map(|model| model.id)
        .filter(|id| !id.trim().is_empty() && seen.insert(id.clone()))
        .collect())
}

fn catalog_client(
    http: impl HttpClientExt + 'static,
    _session: CopilotSession,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_client_uses_shared_transport_constructor() {
        // Transport construction is shared with completions; the model
        // catalog request uses the Copilot OpenAI-compatible endpoint.
        let config = catalog_config(&CopilotSession {
            token: "test".into(),
            api_base: Some("https://assigned.example".into()),
            expires_at: None,
        });
        assert_eq!(config.base_url, "https://assigned.example");
    }
}
