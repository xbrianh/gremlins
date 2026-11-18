use std::collections::HashSet;

use futures::future::BoxFuture;
use rig_core::providers::copilot::model_listing::Models;

use super::resolve_catalog_session;

pub(in crate::clients::backends) fn list_models() -> BoxFuture<'static, Result<Vec<String>, String>> {
    Box::pin(async move { list_models_with().await })
}

async fn list_models_with() -> Result<Vec<String>, String> {
    let (session, middleware) = tokio::task::spawn_blocking(resolve_catalog_session)
        .await
        .map_err(|_| "Copilot credential resolution task failed".to_string())??;
    let mut config = rig_core::providers::openai::OpenAIConfig::with_key(
        &rig_core::providers::copilot::DIALECT,
        session.token.clone(),
    );
    if let Some(base) = session.api_base.as_deref() {
        config = config.with_base_url(base);
    }
    let models = Models::new(config)
        .list_models()
        .await
        .map_err(|_| "Copilot catalog request failed; check your session credential and Copilot access".to_string())?;
    let mut seen = HashSet::new();
    Ok(models.data
        .into_iter()
        .filter(|model| model.r#type.as_deref() == Some("chat"))
        .map(|model| model.id)
        .filter(|id| !id.trim().is_empty() && seen.insert(id.clone()))
        .collect())
}

