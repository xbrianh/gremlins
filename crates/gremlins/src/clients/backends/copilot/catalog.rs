use std::collections::HashSet;

use futures::future::BoxFuture;
use rig_core::http_client::{HttpClientExt, NoBody};
use http::{Method, Request};
use serde::Deserialize;

use super::resolve_catalog_session;

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
    #[serde(rename = "type")]
    kind: Option<String>,
}

async fn list_models_with() -> Result<Vec<String>, String> {
    let (session, _middleware) = tokio::task::spawn_blocking(resolve_catalog_session)
        .await
        .map_err(|_| "Copilot credential resolution task failed".to_string())??;
    let base = session
        .api_base
        .as_deref()
        .unwrap_or(rig_core::providers::copilot::DIALECT.base_url)
        .trim_end_matches('/');
    let request = Request::builder()
        .method(Method::GET)
        .uri(format!("{base}/models"))
        .header(http::header::AUTHORIZATION, format!("Bearer {}", session.token))
        .body(NoBody)
        .map_err(|_| "could not build the Copilot catalog request".to_string())?;
    let http = rig_reqwest::shared();
    let response = http
        .send::<_, Vec<u8>>(request)
        .await
        .map_err(|_| "Copilot catalog request failed; check your session credential and Copilot access".to_string())?;
    if !response.status().is_success() {
        return Err("Copilot catalog request failed; check your session credential and Copilot access".into());
    }
    let body = response
        .into_body()
        .await
        .map_err(|_| "could not read the Copilot catalog response".to_string())?;
    let catalog: Catalog = serde_json::from_slice(&body)
        .map_err(|_| "Copilot returned a catalog that could not be decoded".to_string())?;
    let mut seen = HashSet::new();
    Ok(catalog.data.into_iter()
        .filter(|model| model.kind.as_deref() == Some("chat"))
        .map(|model| model.id)
        .filter(|id| !id.trim().is_empty() && seen.insert(id.clone()))
        .collect())
}

