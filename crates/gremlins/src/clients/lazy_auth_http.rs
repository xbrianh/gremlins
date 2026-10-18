//! Lazy-auth HTTP client wrappers.
//!
//! When a `Backend::make_model` creates a model for a dynamic-auth backend,
//! token acquisition hasn't happened yet (it's async and happens per-attempt).
//! These wrappers hold a `TokenProvider` + scope and acquire a token on the
//! first outgoing request, caching it for the lifetime of the model.
//!
//! A single generic [`LazyAuthHttpClient<H>`] is parameterized by a
//! [`HeaderStrategy`]:
//! - [`BearerStrategy`] — Anthropic dynamic auth: strips `x-api-key`,
//!   injects `Authorization: Bearer <token>`.
//! - [`ApiKeyStrategy`] — Azure OpenAI dynamic auth: injects/overrides
//!   `api-key` with the acquired token.

use std::future::Future;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Request, Response};
use rig_core::http_client::{
    DynHttpClient, HttpClientExt, LazyBody, MultipartForm, Result, StreamingResponse,
};
use rig_core::wasm_compat::WasmCompatSend;

use crate::clients::token_provider::TokenProvider;

// ── HeaderStrategy ─────────────────────────────────────────────────────

/// How a [`LazyAuthHttpClient`] injects the acquired token into a request.
///
/// Implementors differ in the header name they set, how the header value is
/// derived from the token, and which headers (if any) they strip beforehand.
pub(crate) trait HeaderStrategy: Clone + Send + Sync + 'static {
    /// The HTTP header name to inject.
    fn header_name() -> HeaderName;

    /// Build the header value from the acquired token.
    fn header_value(token: &str) -> std::result::Result<HeaderValue, String>;

    /// Optionally strip headers before injecting (e.g. strip `x-api-key` for
    /// bearer auth).
    fn strip_headers(headers: &mut HeaderMap);
}

// ── BearerStrategy ──────────────────────────────────────────────────────

/// Bearer-token strategy: strips `x-api-key` and injects
/// `Authorization: Bearer <token>`.
#[derive(Clone)]
pub(crate) struct BearerStrategy;

impl HeaderStrategy for BearerStrategy {
    fn header_name() -> HeaderName {
        http::header::AUTHORIZATION
    }

    fn header_value(token: &str) -> std::result::Result<HeaderValue, String> {
        let mut v = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|e| format!("invalid bearer token: {e}"))?;
        v.set_sensitive(true);
        Ok(v)
    }

    fn strip_headers(headers: &mut HeaderMap) {
        headers.remove("x-api-key");
    }
}

// ── ApiKeyStrategy ──────────────────────────────────────────────────────

/// API-key strategy: injects/overrides `api-key` with the bare token.
#[derive(Clone)]
pub(crate) struct ApiKeyStrategy;

impl HeaderStrategy for ApiKeyStrategy {
    fn header_name() -> HeaderName {
        HeaderName::from_static("api-key")
    }

    fn header_value(token: &str) -> std::result::Result<HeaderValue, String> {
        let mut v =
            HeaderValue::from_str(token).map_err(|e| format!("invalid api-key token: {e}"))?;
        v.set_sensitive(true);
        Ok(v)
    }

    fn strip_headers(_headers: &mut HeaderMap) {
        // no-op
    }
}

// ── LazyAuthHttpClient ──────────────────────────────────────────────────

/// Wraps a [`DynHttpClient`] and acquires a token from a [`TokenProvider`] on
/// the first request, caching it for the lifetime of the client.  The token is
/// injected according to the [`HeaderStrategy`] `H`.
#[derive(Clone)]
pub(crate) struct LazyAuthHttpClient<H: HeaderStrategy> {
    inner: DynHttpClient,
    token_provider: Arc<dyn TokenProvider>,
    scope: String,
    /// Cached token.  A `Mutex` because the underlying `HttpClientExt`
    /// methods take `&self` and are called from async contexts that may be
    /// multi-threaded.
    token: Arc<Mutex<Option<String>>>,
    _phantom: std::marker::PhantomData<H>,
}

impl<H: HeaderStrategy> LazyAuthHttpClient<H> {
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
            _phantom: std::marker::PhantomData,
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

    /// Rewrite the request by stripping headers (per the strategy) and
    /// injecting the strategy's auth header.
    fn rewrite_request<T>(req: Request<T>, token: &str) -> std::result::Result<Request<T>, String> {
        let header_val = H::header_value(token)?;
        let header_name = H::header_name();
        let (mut parts, body) = req.into_parts();
        H::strip_headers(&mut parts.headers);
        parts.headers.insert(header_name, header_val);
        Ok(Request::from_parts(parts, body))
    }
}

impl<H: HeaderStrategy> HttpClientExt for LazyAuthHttpClient<H> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderMap;

    // ── BearerStrategy ──────────────────────────────────────────────

    #[test]
    fn bearer_strategy_header_name_is_authorization() {
        assert_eq!(BearerStrategy::header_name(), http::header::AUTHORIZATION);
    }

    #[test]
    fn bearer_strategy_header_value_is_bearer_token() {
        let val = BearerStrategy::header_value("abc123").unwrap();
        assert_eq!(val.to_str().unwrap(), "Bearer abc123");
        assert!(val.is_sensitive());
    }

    #[test]
    fn bearer_strategy_strips_x_api_key() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("old-key"));
        BearerStrategy::strip_headers(&mut headers);
        assert!(headers.get("x-api-key").is_none());
    }

    #[test]
    fn bearer_rewrite_injects_authorization_and_strips_x_api_key() {
        let req = Request::builder()
            .header("x-api-key", "should-be-removed")
            .body(())
            .unwrap();
        let rewritten = LazyAuthHttpClient::<BearerStrategy>::rewrite_request(req, "tok").unwrap();
        let headers = rewritten.headers();
        assert_eq!(
            headers
                .get(http::header::AUTHORIZATION)
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer tok"
        );
        assert!(headers.get("x-api-key").is_none());
    }

    // ── ApiKeyStrategy ──────────────────────────────────────────────

    #[test]
    fn apikey_strategy_header_name_is_api_key() {
        assert_eq!(
            ApiKeyStrategy::header_name(),
            HeaderName::from_static("api-key")
        );
    }

    #[test]
    fn apikey_strategy_header_value_is_bare_token() {
        let val = ApiKeyStrategy::header_value("secret").unwrap();
        assert_eq!(val.to_str().unwrap(), "secret");
        assert!(val.is_sensitive());
    }

    #[test]
    fn apikey_strategy_does_not_strip_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_static("keep-me"));
        ApiKeyStrategy::strip_headers(&mut headers);
        assert!(headers.get("x-api-key").is_some());
    }

    #[test]
    fn apikey_rewrite_injects_api_key() {
        let req = Request::builder().body(()).unwrap();
        let rewritten =
            LazyAuthHttpClient::<ApiKeyStrategy>::rewrite_request(req, "secret").unwrap();
        assert_eq!(
            rewritten
                .headers()
                .get(HeaderName::from_static("api-key"))
                .unwrap()
                .to_str()
                .unwrap(),
            "secret"
        );
    }
}
