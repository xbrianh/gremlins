use http::{header::AUTHORIZATION, HeaderMap, HeaderValue};
use rig_core::http_client::middleware::HttpMiddleware;
use rig_core::http_client::{DynHttpClient, Error, Exchange, Result};
use rig_core::providers::chatgpt::auth::{AuthContext, FileTokenStore};
use rig_core::wasm_compat::WasmBoxedFuture;

use crate::clients::chatgpt_auth::{resolve_stored, SignInError};

const ACCOUNT_ID: &str = "chatgpt-account-id";

#[derive(Clone)]
pub(super) struct StoredOAuth {
    pub(super) store: FileTokenStore,
    /// The underlying transport, without this middleware, for refresh requests.
    pub(super) http: DynHttpClient,
}

impl StoredOAuth {
    pub(super) async fn resolve(&self) -> std::result::Result<AuthContext, SignInError> {
        resolve_stored(&self.store, &self.http).await
    }
}

impl HttpMiddleware for StoredOAuth {
    fn before_request_headers<'a>(
        &'a self,
        _exchange: Exchange<'a>,
        headers: &'a mut HeaderMap,
    ) -> WasmBoxedFuture<'a, Result<()>> {
        Box::pin(async move {
            let context = self.resolve().await.map_err(Error::instance)?;
            let mut bearer =
                HeaderValue::from_str(&format!("Bearer {}", context.api_key.expose()))?;
            bearer.set_sensitive(true);
            headers.insert(AUTHORIZATION, bearer);
            if let Some(id) = context.account_id {
                headers.insert(ACCOUNT_ID, HeaderValue::from_str(&id)?);
            } else {
                headers.remove(ACCOUNT_ID);
            }
            Ok(())
        })
    }
}
