//! HTTP client wrapper that rewrites `x-api-key` → `Authorization: Bearer`.
//!
//! Azure's Anthropic-compatible API (cognitive services) authenticates Entra ID
//! tokens via `Authorization: Bearer`.  Rig's `AnthropicKey` always emits
//! `x-api-key`, so this wrapper intercepts every outgoing request, strips the
//! `x-api-key` header, and injects `Authorization: Bearer <token>`.
//!
//! This module is self-contained.  When rig adds native bearer support, delete
//! this file and revert the one call site in `backends/anthropic.rs`.

use std::future::Future;
use std::sync::Arc;

use bytes::Bytes;
use http::{HeaderValue, Request, Response};
use rig_core::http_client::{
    HttpClientExt, LazyBody, MultipartForm, ReqwestClient, Result, StreamingResponse,
};
use rig_core::wasm_compat::WasmCompatSend;

/// Wraps a [`ReqwestClient`] and rewrites auth headers on every request.
///
/// Cheaply cloneable — the inner client and bearer token are both `Arc`-held.
#[derive(Clone)]
pub(crate) struct BearerHttpClient {
    inner: ReqwestClient,
    token: Arc<String>,
}

impl Default for BearerHttpClient {
    fn default() -> Self {
        Self {
            inner: ReqwestClient::new(),
            token: Arc::new(String::new()),
        }
    }
}

impl BearerHttpClient {
    /// Wrap `inner` so every request carries `Authorization: Bearer <token>`
    /// instead of whatever API-key header rig's `AnthropicKey` injects.
    pub(crate) fn new(inner: ReqwestClient, token: String) -> Self {
        Self {
            inner,
            token: Arc::new(token),
        }
    }

    /// Strip `x-api-key` and insert `Authorization: Bearer <token>`.
    fn rewrite_request<T>(&self, mut req: Request<T>) -> Request<T> {
        let headers = req.headers_mut();
        headers.remove("x-api-key");
        let bearer = format!("Bearer {}", self.token.as_str());
        headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_str(&bearer).expect("bearer token must be a valid header value"),
        );
        req
    }
}

impl HttpClientExt for BearerHttpClient {
    fn send<T, U>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        T: Into<Bytes>,
        T: WasmCompatSend,
        U: From<Bytes>,
        U: WasmCompatSend + 'static,
    {
        let req = self.rewrite_request(req);
        self.inner.send(req)
    }

    fn send_multipart<U>(
        &self,
        req: Request<MultipartForm>,
    ) -> impl Future<Output = Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        U: From<Bytes>,
        U: WasmCompatSend + 'static,
    {
        // Pass-through: Anthropic completions don't use multipart.
        self.inner.send_multipart(req)
    }

    fn send_streaming<T>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = Result<StreamingResponse>> + WasmCompatSend
    where
        T: Into<Bytes> + WasmCompatSend,
    {
        let req = self.rewrite_request(req);
        self.inner.send_streaming(req)
    }
}

// ── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use http::Request;

    #[test]
    fn rewrites_x_api_key_to_bearer() {
        let client = ReqwestClient::new();
        let wrapper = BearerHttpClient::new(client, "test-token".into());

        let req = Request::builder()
            .uri("https://example.com/v1/messages")
            .header("x-api-key", "should-be-removed")
            .header("content-type", "application/json")
            .body("{}")
            .unwrap();

        let rewritten = wrapper.rewrite_request(req);

        assert!(
            rewritten.headers().get("x-api-key").is_none(),
            "x-api-key must be stripped"
        );
        assert_eq!(
            rewritten
                .headers()
                .get("Authorization")
                .expect("Authorization header must be present")
                .to_str()
                .unwrap(),
            "Bearer test-token"
        );
        assert!(
            rewritten.headers().get("content-type").is_some(),
            "unrelated headers must survive"
        );
    }

    #[test]
    fn bearer_http_client_is_cloneable() {
        let client = ReqwestClient::new();
        let wrapper = BearerHttpClient::new(client, "cloned-token".into());
        let _clone = wrapper.clone();
        // If this compiles, the test passes — Clone is the assertion.
    }
}
