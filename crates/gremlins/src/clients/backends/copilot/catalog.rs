use std::collections::HashSet;

use futures::future::BoxFuture;
use http::Request;
use rig_core::http_client::{HttpClientExt, NoBody};
use serde::Deserialize;

use super::{copilot_transport, resolve_catalog_session};

pub(in crate::clients::backends) fn list_models() -> BoxFuture<'static, Result<Vec<String>, String>> {
    Box::pin(async move { list_models_with().await })
}

#[derive(Deserialize)]
struct Catalog {
    data: Vec<Model>,
}

#[derive(Deserialize)]
struct Model {
    id: String,
    capabilities: Option<Capabilities>,
}

#[derive(Deserialize)]
struct Capabilities {
    kind: Option<String>,
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
    let client = copilot_transport(middleware);
    let request = Request::builder()
        .method(http::Method::GET)
        .uri(format!("{base}/models"))
        .body(NoBody)
        .map_err(|_| "could not build the Copilot catalog request".to_string())?;
    let response = client
        .send::<_, Vec<u8>>(request)
        .await
        .map_err(|_| "Copilot catalog request failed".to_string())?;
    if !response.status().is_success() {
        return Err(format!(
            "Copilot catalog request failed with status {}",
            response.status()
        ));
    }
    let body = response
        .into_body()
        .await
        .map_err(|_| "could not read the Copilot catalog response".to_string())?;
    let catalog: Catalog = serde_json::from_slice(&body)
        .map_err(|_| "Copilot catalog response was invalid".to_string())?;
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

