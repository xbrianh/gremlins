use std::collections::HashSet;

use futures::future::BoxFuture;
use rig_core::http_client::{DynHttpClient, HttpClientExt};
use rig_core::operation::Listing;
use rig_core::providers::copilot::{model_listing::Models, DIALECT};
use rig_core::providers::openai::OpenAIConfig;

use super::{resolve_catalog_session, CopilotSession, CopilotTokenRefreshMiddleware};

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
    let response = Models::new(catalog_config(&session)).send(&client).await.map_err(|_| {
        "Copilot catalog request failed; check your Copilot session credential and GitHub account Copilot access"
            .to_string()
    })?;
    let mut seen = HashSet::new();
    Ok(response
        .into_iter()
        .filter(|model| model.capabilities.r#type.as_deref() == Some("chat"))
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
        // Transport construction is shared with completions; operation-level
        // request behavior and decoding are provided by Rig's Models type.
        let config = catalog_config(&CopilotSession {
            token: "test".into(),
            api_base: Some("https://assigned.example".into()),
            expires_at: None,
        });
        assert_eq!(config.base_url, "https://assigned.example");
    }
}
