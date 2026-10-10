//! Lazy-auth HTTP client wrappers.
//!
//! When a `Backend::make_model` creates a model for a dynamic-auth backend,
//! token acquisition hasn't happened yet (it's async and happens per-attempt).
//! These wrappers hold a `TokenProvider` + scope and acquire a token on the
//! first outgoing request, caching it for the lifetime of the model.
//!
//! Two variants:
//! - `LazyBearerHttpClient` — Anthropic dynamic auth: strips `x-api-key`,
//!   injects `Authorization: Bearer <token>`.
//! - `LazyApiKeyHttpClient` — Azure OpenAI dynamic auth: injects/overrides
//!   `api-key` with the acquired token.

use std::future::Future;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http::{HeaderValue, Request, Response};
use rig_core::http_client::{
    DynHttpClient, HttpClientExt, LazyBody, MultipartForm, Result, StreamingResponse,
};
use rig_core::wasm_compat::WasmCompatSend;

use crate::clients::token_provider::TokenProvider;

// ── LazyBearerHttpClient ──────────────────────────────────────────────────

/// Wraps a [`DynHttpClient`] and acquires a bearer token from a
/// [`TokenProvider`] on the first request.  Strips `x-api-key` and injects
/// `Authorization: Bearer <token>` — the same rewrite as
/// [`crate::clients::anthropic_bearer_http::BearerHttpClient`], but lazy.
#[derive(Clone)]
pub(crate) struct LazyBearerHttpClient {
    inner: DynHttpClient,
    token_provider: Arc<dyn TokenProvider>,
    scope: String,
    /// Cached token.  A `Mutex` because the underlying `HttpClientExt`
    /// methods take `&self` and are called from async contexts that may be
    /// multi-threaded.
    token: Arc<Mutex<Option<String>>>,
}

impl LazyBearerHttpClient {
    pub(crate) fn new(
        inner: DynHttpClient,
        token_provider: Arc<dyn TokenProvider>,
        scope: String,
    ) -> Self {
        Self {
            inner,
            token_provider,
            scope,
            token: Arc::new(Mutex::new(None)),
        }
    }

    /// Acquire a token (lazily, cached after first call).
    async fn acquire_token(&self) -> std::result::Result<String, String> {
        // Fast path: cached.
        {
            let guard = self.token.lock().unwrap();
            if let Some(ref t) = *guard {
                return Ok(t.clone());
            }
        }
        let token = self
            .token_provider
            .get_token(&self.scope)
            .await
            .map_err(|e| format!("token acquisition failed: {e}"))?;
        // Validate.
        let t = token.trim();
        if t.is_empty() {
            return Err("acquired token is empty".into());
        }
        let mut guard = self.token.lock().unwrap();
        // Double-check: another caller may have beaten us.
        if guard.is_none() {
            *guard = Some(t.to_string());
        }
        Ok(guard.clone().unwrap())
    }

    /// Build `Authorization: Bearer <token>` header value.
    fn bearer_value(token: &str) -> std::result::Result<HeaderValue, String> {
        let mut v = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|e| format!("invalid bearer token: {e}"))?;
        v.set_sensitive(true);
        Ok(v)
    }

    /// Strip `x-api-key` and inject `Authorization: Bearer <token>`.
    fn rewrite_request<T>(req: Request<T>, token: &str) -> std::result::Result<Request<T>, String> {
        let bearer = Self::bearer_value(token)?;
        let (mut parts, body) = req.into_parts();
        parts.headers.remove("x-api-key");
        parts.headers.insert(http::header::AUTHORIZATION, bearer);
        Ok(Request::from_parts(parts, body))
    }
}

impl HttpClientExt for LazyBearerHttpClient {
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
        // Convert to Request<Bytes> before the async block so we don't
        // capture T across an await point — keeps the returned future 'static
        // without requiring T: 'static.
        let req = req.map(|b| b.into());
        let inner = self.inner.clone();
        let this = self.clone();
        async move {
            let token = this.acquire_token().await.map_err(|e| {
                Box::new(std::io::Error::other(e)) as Box<dyn std::error::Error + Send + Sync>
            })?;
            let req = Self::rewrite_request(req, &token).map_err(|e| {
                Box::new(std::io::Error::other(e)) as Box<dyn std::error::Error + Send + Sync>
            })?;
            inner.send(req).await
        }
    }

    fn send_multipart<U>(
        &self,
        req: Request<MultipartForm>,
    ) -> impl Future<Output = Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        U: From<Bytes>,
        U: WasmCompatSend + 'static,
    {
        // Pass-through.
        self.inner.send_multipart(req)
    }

    fn send_streaming<T>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = Result<StreamingResponse>> + WasmCompatSend
    where
        T: Into<Bytes> + WasmCompatSend,
    {
        // Convert to Request<Bytes> before the async block — see send().
        let req = req.map(|b| b.into());
        let inner = self.inner.clone();
        let this = self.clone();
        async move {
            let token = this.acquire_token().await.map_err(|e| {
                Box::new(std::io::Error::other(e)) as Box<dyn std::error::Error + Send + Sync>
            })?;
            let req = Self::rewrite_request(req, &token).map_err(|e| {
                Box::new(std::io::Error::other(e)) as Box<dyn std::error::Error + Send + Sync>
            })?;
            inner.send_streaming(req).await
        }
    }
}

// ── LazyApiKeyHttpClient ──────────────────────────────────────────────────

/// Wraps a [`DynHttpClient`] and acquires an Entra-ID token from a
/// [`TokenProvider`] on the first request.  Overrides the `api-key` header
/// so the Azure OpenAI backend always sends a valid token.
#[derive(Clone)]
pub(crate) struct LazyApiKeyHttpClient {
    inner: DynHttpClient,
    token_provider: Arc<dyn TokenProvider>,
    scope: String,
    token: Arc<Mutex<Option<String>>>,
}

impl LazyApiKeyHttpClient {
    pub(crate) fn new(
        inner: DynHttpClient,
        token_provider: Arc<dyn TokenProvider>,
        scope: String,
    ) -> Self {
        Self {
            inner,
            token_provider,
            scope,
            token: Arc::new(Mutex::new(None)),
        }
    }

    async fn acquire_token(&self) -> std::result::Result<String, String> {
        {
            let guard = self.token.lock().unwrap();
            if let Some(ref t) = *guard {
                return Ok(t.clone());
            }
        }
        let token = self
            .token_provider
            .get_token(&self.scope)
            .await
            .map_err(|e| format!("token acquisition failed: {e}"))?;
        let t = token.trim();
        if t.is_empty() {
            return Err("acquired token is empty".into());
        }
        let mut guard = self.token.lock().unwrap();
        if guard.is_none() {
            *guard = Some(t.to_string());
        }
        Ok(guard.clone().unwrap())
    }

    fn api_key_value(token: &str) -> std::result::Result<HeaderValue, String> {
        let mut v =
            HeaderValue::from_str(token).map_err(|e| format!("invalid api-key token: {e}"))?;
        v.set_sensitive(true);
        Ok(v)
    }

    /// Ensure `api-key` header contains the acquired token.
    fn rewrite_request<T>(req: Request<T>, token: &str) -> std::result::Result<Request<T>, String> {
        let key_val = Self::api_key_value(token)?;
        let (mut parts, body) = req.into_parts();
        parts.headers.insert("api-key", key_val);
        Ok(Request::from_parts(parts, body))
    }
}

impl HttpClientExt for LazyApiKeyHttpClient {
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
        // Convert to Request<Bytes> before the async block so we don't
        // capture T across an await point.
        let req = req.map(|b| b.into());
        let inner = self.inner.clone();
        let this = self.clone();
        async move {
            let token = this.acquire_token().await.map_err(|e| {
                Box::new(std::io::Error::other(e)) as Box<dyn std::error::Error + Send + Sync>
            })?;
            let req = Self::rewrite_request(req, &token).map_err(|e| {
                Box::new(std::io::Error::other(e)) as Box<dyn std::error::Error + Send + Sync>
            })?;
            inner.send(req).await
        }
    }

    fn send_multipart<U>(
        &self,
        req: Request<MultipartForm>,
    ) -> impl Future<Output = Result<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        U: From<Bytes>,
        U: WasmCompatSend + 'static,
    {
        self.inner.send_multipart(req)
    }

    fn send_streaming<T>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = Result<StreamingResponse>> + WasmCompatSend
    where
        T: Into<Bytes> + WasmCompatSend,
    {
        // Convert to Request<Bytes> before the async block — see send().
        let req = req.map(|b| b.into());
        let inner = self.inner.clone();
        let this = self.clone();
        async move {
            let token = this.acquire_token().await.map_err(|e| {
                Box::new(std::io::Error::other(e)) as Box<dyn std::error::Error + Send + Sync>
            })?;
            let req = Self::rewrite_request(req, &token).map_err(|e| {
                Box::new(std::io::Error::other(e)) as Box<dyn std::error::Error + Send + Sync>
            })?;
            inner.send_streaming(req).await
        }
    }
}
