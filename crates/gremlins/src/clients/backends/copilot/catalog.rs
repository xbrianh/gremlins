use std::collections::HashSet;

use futures::future::BoxFuture;
use rig_core::providers::copilot::model_listing::Models;
use rig_core::providers::openai::OpenAIConfig;
use rig_core::providers::copilot::DIALECT;

use super::{copilot_transport, resolve_catalog_session};

pub(in crate::clients::backends) fn list_models() -> BoxFuture<'static, Result<Vec<String>, String>> {
    Box::pin(async move { list_models_with().await })
}

async fn list_models_with() -> Result<Vec<String>, String> {
    let (session, middleware) = tokio::task::spawn_blocking(resolve_catalog_session)
        .await
        .map_err(|_| "Copilot credential resolution task failed".to_string())??;
    let base = session
        .api_base
        .as_deref()
        .unwrap_or(rig_core::providers::copilot::DIALECT.base_url)
        .trim_end_matches('/');
    let transport = copilot_transport(middleware);
    let config = OpenAIConfig::with_key(&DIALECT, session.token)
        .with_base_url(base.to_string())
        .with_http_client(transport);
    let models = Models::new(config);
    let catalog = models
        .list()
        .await
        .map_err(|_| "Copilot catalog request failed".to_string())?;
    let mut seen = HashSet::new();
    Ok(catalog
        .data
        .into_iter()
        .filter(|model| {
            model
                .capabilities
                .as_ref()
                .and_then(|capabilities| capabilities.kind.as_deref())
                == Some("chat")
        })
        .map(|model| model.id)
        .filter(|id| !id.trim().is_empty() && seen.insert(id.clone()))
        .collect())
}

