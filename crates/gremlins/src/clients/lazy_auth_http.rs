//! Lazy-auth HTTP client wrappers.
//!
//! When a `Backend::make_model` creates a model for a dynamic-auth backend,
//! token acquisition hasn't happened yet (it's async and happens per-attempt).
//! These wrappers hold a `TokenProvider` + scope and acquire a fresh token
//! on every outgoing request. Token caching and refresh is owned by the
//! `TokenProvider` implementations (which delegate to the Azure SDK's
//! `TokenCredential` with its own in-memory cache).
//!
//! A single generic [`LazyAuthHttpClient<H>`] is parameterized by a
//! [`HeaderStrategy`]:
//! - [`BearerStrategy`] — Anthropic dynamic auth: strips `x-api-key`,
//!   injects `Authorization: Bearer <token>`.
//! - [`ApiKeyStrategy`] — Azure OpenAI dynamic auth: injects/overrides
//!   `api-key` with the acquired token.

use std::future::Future;
use std::sync::Arc;

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

// ── LazyAuthHttpClient ───────────────────────────────────────────────────

/// Wraps a [`DynHttpClient`] and acquires a fresh token from a
/// [`TokenProvider`] on every outgoing request, injecting it according to
/// the [`HeaderStrategy`] `H`. The [`TokenProvider`] owns caching and
/// refresh — typically backed by the Azure SDK's `TokenCredential` which
/// caches tokens in memory and auto-refreshes ~5 minutes before expiry.
#[derive(Clone)]
pub(crate) struct LazyAuthHttpClient<H: HeaderStrategy> {
    inner: DynHttpClient,
    token_provider: Arc<dyn TokenProvider>,
    scope: String,
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
            _phantom: std::marker::PhantomData,
        }
    }

    /// Acquire a fresh token from the [`TokenProvider`] on every call.
    /// The provider owns caching and refresh — this is a cheap mutex
    /// lock + cache lookup when backed by the Azure SDK.
    async fn acquire_token(&self) -> std::result::Result<String, String> {
        let token = self
            .token_provider
            .get_token(&self.scope)
            .await
            .map_err(|e| format!("token acquisition failed: {e}"))?;
        let t = token.trim();
        if t.is_empty() {
            return Err("acquired token is empty".into());
        }
        Ok(t.to_string())
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
    use crate::clients::token_provider::{TokenProvider, TokenProviderError};
    use async_trait::async_trait;
    use http::HeaderMap;
    use std::sync::Mutex;

    // ── helpers ───────────────────────────────────────────────────────

    fn test_dyn_http() -> DynHttpClient {
        DynHttpClient::new(rig_reqwest::shared())
    }

    fn bind_ephemeral() -> (std::net::TcpListener, u16) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    /// Read HTTP request headers from a TCP stream until the header terminator.
    fn read_headers(stream: &mut std::net::TcpStream) -> Vec<String> {
        use std::io::BufRead;
        let mut reader = std::io::BufReader::new(stream);
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).unwrap();
            if n == 0 || line == "\r\n" || line == "\n" {
                break;
            }
            lines.push(line.trim_end().to_string());
        }
        lines
    }

    /// Extract the value of an auth header from captured request lines.
    fn auth_header(lines: &[String], header_name: &str) -> Option<String> {
        let prefix = format!("{}:", header_name.to_lowercase());
        lines
            .iter()
            .find(|l| l.to_lowercase().starts_with(&prefix))
            .map(|l| l.split_once(':').unwrap().1.trim().to_string())
    }

    /// A [`TokenProvider`] that rotates through a predefined list of results.
    ///
    /// Returns the next result on each call, then errors with "exhausted".
    /// Also logs every scope it receives for later inspection.
    struct RotatingProvider {
        results: Mutex<Vec<std::result::Result<String, TokenProviderError>>>,
        scope_log: Mutex<Vec<String>>,
    }

    impl RotatingProvider {
        fn new(results: Vec<std::result::Result<String, TokenProviderError>>) -> Self {
            Self {
                results: Mutex::new(results),
                scope_log: Mutex::new(Vec::new()),
            }
        }

        fn scopes(&self) -> Vec<String> {
            self.scope_log.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl TokenProvider for RotatingProvider {
        async fn get_token(&self, scope: &str) -> std::result::Result<String, TokenProviderError> {
            self.scope_log.lock().unwrap().push(scope.to_string());
            let mut results = self.results.lock().unwrap();
            if results.is_empty() {
                return Err(TokenProviderError::Other("exhausted".into()));
            }
            results.remove(0)
        }
    }

    // ── BearerStrategy ──────────────────────────────────────────────────

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

    // ── ApiKeyStrategy ──────────────────────────────────────────────────────

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

    // ── async regression: BearerStrategy ──────────────────────────────

    #[test]
    fn bearer_send_acquires_fresh_token_on_every_request() {
        let (listener, port) = bind_ephemeral();
        let provider = Arc::new(RotatingProvider::new(vec![
            Ok("token-1".into()),
            Ok("token-2".into()),
        ]));
        let client = LazyAuthHttpClient::<BearerStrategy>::new(
            test_dyn_http(),
            provider.clone(),
            "my-scope".into(),
        );

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .enable_io()
            .build()
            .unwrap();

        // Request 1 → token-1
        let headers1 = {
            let jh = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                read_headers(&mut stream)
            });
            let req = Request::builder()
                .uri(format!("http://127.0.0.1:{port}/v1/messages"))
                .body("{}")
                .unwrap();
            let _ = rt.block_on(client.send::<_, bytes::Bytes>(req));
            jh.join().unwrap()
        };

        // Request 2 → token-2 (fresh acquisition)
        let (listener2, port2) = bind_ephemeral();
        let client2 = client.clone();
        let headers2 = {
            let jh = std::thread::spawn(move || {
                let (mut stream, _) = listener2.accept().unwrap();
                read_headers(&mut stream)
            });
            let req = Request::builder()
                .uri(format!("http://127.0.0.1:{port2}/v1/messages"))
                .body("{}")
                .unwrap();
            let _ = rt.block_on(client2.send::<_, bytes::Bytes>(req));
            jh.join().unwrap()
        };

        assert_eq!(
            auth_header(&headers1, "authorization"),
            Some("Bearer token-1".into())
        );
        assert_eq!(
            auth_header(&headers2, "authorization"),
            Some("Bearer token-2".into())
        );
    }

    #[test]
    fn bearer_send_streaming_acquires_fresh_token_on_every_request() {
        let (listener, port) = bind_ephemeral();
        let provider = Arc::new(RotatingProvider::new(vec![
            Ok("tok-a".into()),
            Ok("tok-b".into()),
        ]));
        let client =
            LazyAuthHttpClient::<BearerStrategy>::new(test_dyn_http(), provider, "s".into());

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .enable_io()
            .build()
            .unwrap();

        let headers1 = {
            let jh = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                read_headers(&mut stream)
            });
            let req = Request::builder()
                .uri(format!("http://127.0.0.1:{port}/v1/messages"))
                .body("{}")
                .unwrap();
            let _ = rt.block_on(client.send_streaming(req));
            jh.join().unwrap()
        };

        let (listener2, port2) = bind_ephemeral();
        let headers2 = {
            let jh = std::thread::spawn(move || {
                let (mut stream, _) = listener2.accept().unwrap();
                read_headers(&mut stream)
            });
            let req = Request::builder()
                .uri(format!("http://127.0.0.1:{port2}/v1/messages"))
                .body("{}")
                .unwrap();
            let _ = rt.block_on(client.send_streaming(req));
            jh.join().unwrap()
        };

        assert_eq!(
            auth_header(&headers1, "authorization"),
            Some("Bearer tok-a".into())
        );
        assert_eq!(
            auth_header(&headers2, "authorization"),
            Some("Bearer tok-b".into())
        );
    }

    #[test]
    fn bearer_clone_acquires_fresh_token_independently() {
        let provider = Arc::new(RotatingProvider::new(vec![
            Ok("orig".into()),
            Ok("clone".into()),
        ]));
        let original =
            LazyAuthHttpClient::<BearerStrategy>::new(test_dyn_http(), provider, "s".into());
        let cloned = original.clone();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .enable_io()
            .build()
            .unwrap();

        let (l1, p1) = bind_ephemeral();
        let h1 = {
            let jh = std::thread::spawn(move || {
                let (mut stream, _) = l1.accept().unwrap();
                read_headers(&mut stream)
            });
            let req = Request::builder()
                .uri(format!("http://127.0.0.1:{p1}/"))
                .body("{}")
                .unwrap();
            let _ = rt.block_on(original.send::<_, bytes::Bytes>(req));
            jh.join().unwrap()
        };

        let (l2, p2) = bind_ephemeral();
        let h2 = {
            let jh = std::thread::spawn(move || {
                let (mut stream, _) = l2.accept().unwrap();
                read_headers(&mut stream)
            });
            let req = Request::builder()
                .uri(format!("http://127.0.0.1:{p2}/"))
                .body("{}")
                .unwrap();
            let _ = rt.block_on(cloned.send::<_, bytes::Bytes>(req));
            jh.join().unwrap()
        };

        assert_eq!(
            auth_header(&h1, "authorization"),
            Some("Bearer orig".into())
        );
        assert_eq!(
            auth_header(&h2, "authorization"),
            Some("Bearer clone".into())
        );
    }

    #[test]
    fn bearer_provider_receives_configured_scope() {
        let (listener, port) = bind_ephemeral();
        let provider = Arc::new(RotatingProvider::new(vec![Ok("tok".into())]));
        let scope = "https://custom.scope/.default";
        let client = LazyAuthHttpClient::<BearerStrategy>::new(
            test_dyn_http(),
            provider.clone(),
            scope.into(),
        );

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .enable_io()
            .build()
            .unwrap();

        let jh = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_headers(&mut stream)
        });
        let req = Request::builder()
            .uri(format!("http://127.0.0.1:{port}/"))
            .body("{}")
            .unwrap();
        let _ = rt.block_on(client.send::<_, bytes::Bytes>(req));
        jh.join().unwrap();

        assert_eq!(provider.scopes(), vec![scope.to_string()]);
    }

    #[test]
    fn bearer_empty_token_is_error() {
        let provider = Arc::new(RotatingProvider::new(vec![Ok("  ".into())]));
        let client =
            LazyAuthHttpClient::<BearerStrategy>::new(test_dyn_http(), provider, "s".into());

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .enable_io()
            .build()
            .unwrap();

        let req = Request::builder()
            .uri("http://127.0.0.1:1/")
            .body("{}")
            .unwrap();
        let result = rt.block_on(client.send::<_, bytes::Bytes>(req));

        assert!(result.is_err(), "empty token must cause send error");
        let err = if let Err(e) = result {
            e.to_string()
        } else {
            String::new()
        };
        assert!(
            err.contains("empty"),
            "error must mention empty token, got: {err}"
        );
    }

    #[test]
    fn bearer_acquisition_failure_is_propagated() {
        let provider = Arc::new(RotatingProvider::new(vec![Err(
            TokenProviderError::Credential("auth down".into()),
        )]));
        let client =
            LazyAuthHttpClient::<BearerStrategy>::new(test_dyn_http(), provider, "s".into());

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .enable_io()
            .build()
            .unwrap();

        let req = Request::builder()
            .uri("http://127.0.0.1:1/")
            .body("{}")
            .unwrap();
        let result = rt.block_on(client.send::<_, bytes::Bytes>(req));

        assert!(result.is_err(), "acquisition failure must cause send error");
        let err = if let Err(e) = result {
            e.to_string()
        } else {
            String::new()
        };
        assert!(
            err.contains("token acquisition failed") || err.contains("auth down"),
            "error must reflect acquisition failure, got: {err}"
        );
    }

    #[test]
    fn bearer_send_streaming_acquisition_failure_is_propagated() {
        let provider = Arc::new(RotatingProvider::new(vec![Err(
            TokenProviderError::Credential("stream auth down".into()),
        )]));
        let client =
            LazyAuthHttpClient::<BearerStrategy>::new(test_dyn_http(), provider, "s".into());

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .enable_io()
            .build()
            .unwrap();

        let req = Request::builder()
            .uri("http://127.0.0.1:1/")
            .body("{}")
            .unwrap();
        let result = rt.block_on(client.send_streaming(req));

        assert!(
            result.is_err(),
            "send_streaming acquisition failure must cause error"
        );
        let err = if let Err(e) = result {
            e.to_string()
        } else {
            String::new()
        };
        assert!(
            err.contains("token acquisition failed") || err.contains("stream auth down"),
            "error must reflect acquisition failure, got: {err}"
        );
    }

    #[test]
    fn bearer_send_strips_x_api_key_on_wire() {
        let (listener, port) = bind_ephemeral();
        let provider = Arc::new(RotatingProvider::new(vec![Ok("tok".into())]));
        let client =
            LazyAuthHttpClient::<BearerStrategy>::new(test_dyn_http(), provider, "s".into());

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .enable_io()
            .build()
            .unwrap();

        let jh = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_headers(&mut stream)
        });
        let req = Request::builder()
            .uri(format!("http://127.0.0.1:{port}/"))
            .header("x-api-key", "should-be-removed")
            .header("content-type", "application/json")
            .body("{}")
            .unwrap();
        let _ = rt.block_on(client.send::<_, bytes::Bytes>(req));
        let headers = jh.join().unwrap();

        assert_eq!(
            auth_header(&headers, "authorization"),
            Some("Bearer tok".into())
        );
        let has_x_api_key = headers
            .iter()
            .any(|h| h.to_lowercase().starts_with("x-api-key:"));
        assert!(!has_x_api_key, "x-api-key must be stripped on the wire");
        let has_content_type = headers
            .iter()
            .any(|h| h.to_lowercase().starts_with("content-type:"));
        assert!(has_content_type, "unrelated headers must survive");
    }

    // ── async regression: ApiKeyStrategy ──────────────────────────────

    #[test]
    fn apikey_send_acquires_fresh_token_on_every_request() {
        let (listener, port) = bind_ephemeral();
        let provider = Arc::new(RotatingProvider::new(vec![
            Ok("key-1".into()),
            Ok("key-2".into()),
        ]));
        let client = LazyAuthHttpClient::<ApiKeyStrategy>::new(
            test_dyn_http(),
            provider.clone(),
            "my-scope".into(),
        );

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .enable_io()
            .build()
            .unwrap();

        let headers1 = {
            let jh = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                read_headers(&mut stream)
            });
            let req = Request::builder()
                .uri(format!("http://127.0.0.1:{port}/"))
                .body("{}")
                .unwrap();
            let _ = rt.block_on(client.send::<_, bytes::Bytes>(req));
            jh.join().unwrap()
        };

        let (listener2, port2) = bind_ephemeral();
        let headers2 = {
            let jh = std::thread::spawn(move || {
                let (mut stream, _) = listener2.accept().unwrap();
                read_headers(&mut stream)
            });
            let req = Request::builder()
                .uri(format!("http://127.0.0.1:{port2}/"))
                .body("{}")
                .unwrap();
            let _ = rt.block_on(client.send::<_, bytes::Bytes>(req));
            jh.join().unwrap()
        };

        assert_eq!(auth_header(&headers1, "api-key"), Some("key-1".into()));
        assert_eq!(auth_header(&headers2, "api-key"), Some("key-2".into()));
    }

    #[test]
    fn apikey_send_streaming_acquires_fresh_token_on_every_request() {
        let (listener, port) = bind_ephemeral();
        let provider = Arc::new(RotatingProvider::new(vec![
            Ok("ak-a".into()),
            Ok("ak-b".into()),
        ]));
        let client =
            LazyAuthHttpClient::<ApiKeyStrategy>::new(test_dyn_http(), provider, "s".into());

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .enable_io()
            .build()
            .unwrap();

        let headers1 = {
            let jh = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                read_headers(&mut stream)
            });
            let req = Request::builder()
                .uri(format!("http://127.0.0.1:{port}/"))
                .body("{}")
                .unwrap();
            let _ = rt.block_on(client.send_streaming(req));
            jh.join().unwrap()
        };

        let (listener2, port2) = bind_ephemeral();
        let headers2 = {
            let jh = std::thread::spawn(move || {
                let (mut stream, _) = listener2.accept().unwrap();
                read_headers(&mut stream)
            });
            let req = Request::builder()
                .uri(format!("http://127.0.0.1:{port2}/"))
                .body("{}")
                .unwrap();
            let _ = rt.block_on(client.send_streaming(req));
            jh.join().unwrap()
        };

        assert_eq!(auth_header(&headers1, "api-key"), Some("ak-a".into()));
        assert_eq!(auth_header(&headers2, "api-key"), Some("ak-b".into()));
    }

    #[test]
    fn apikey_clone_acquires_fresh_token_independently() {
        let provider = Arc::new(RotatingProvider::new(vec![
            Ok("orig-k".into()),
            Ok("clone-k".into()),
        ]));
        let original =
            LazyAuthHttpClient::<ApiKeyStrategy>::new(test_dyn_http(), provider, "s".into());
        let cloned = original.clone();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .enable_io()
            .build()
            .unwrap();

        let (l1, p1) = bind_ephemeral();
        let h1 = {
            let jh = std::thread::spawn(move || {
                let (mut stream, _) = l1.accept().unwrap();
                read_headers(&mut stream)
            });
            let req = Request::builder()
                .uri(format!("http://127.0.0.1:{p1}/"))
                .body("{}")
                .unwrap();
            let _ = rt.block_on(original.send::<_, bytes::Bytes>(req));
            jh.join().unwrap()
        };

        let (l2, p2) = bind_ephemeral();
        let h2 = {
            let jh = std::thread::spawn(move || {
                let (mut stream, _) = l2.accept().unwrap();
                read_headers(&mut stream)
            });
            let req = Request::builder()
                .uri(format!("http://127.0.0.1:{p2}/"))
                .body("{}")
                .unwrap();
            let _ = rt.block_on(cloned.send::<_, bytes::Bytes>(req));
            jh.join().unwrap()
        };

        assert_eq!(auth_header(&h1, "api-key"), Some("orig-k".into()));
        assert_eq!(auth_header(&h2, "api-key"), Some("clone-k".into()));
    }

    #[test]
    fn apikey_provider_receives_configured_scope() {
        let (listener, port) = bind_ephemeral();
        let scope = "https://azure.openai/.default";
        let provider = Arc::new(RotatingProvider::new(vec![Ok("tok".into())]));
        let client = LazyAuthHttpClient::<ApiKeyStrategy>::new(
            test_dyn_http(),
            provider.clone(),
            scope.into(),
        );

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .enable_io()
            .build()
            .unwrap();

        let jh = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_headers(&mut stream)
        });
        let req = Request::builder()
            .uri(format!("http://127.0.0.1:{port}/"))
            .body("{}")
            .unwrap();
        let _ = rt.block_on(client.send::<_, bytes::Bytes>(req));
        jh.join().unwrap();

        assert_eq!(provider.scopes(), vec![scope.to_string()]);
    }

    #[test]
    fn apikey_empty_token_is_error() {
        let provider = Arc::new(RotatingProvider::new(vec![Ok("".into())]));
        let client =
            LazyAuthHttpClient::<ApiKeyStrategy>::new(test_dyn_http(), provider, "s".into());

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .enable_io()
            .build()
            .unwrap();

        let req = Request::builder()
            .uri("http://127.0.0.1:1/")
            .body("{}")
            .unwrap();
        let result = rt.block_on(client.send::<_, bytes::Bytes>(req));

        assert!(result.is_err(), "empty token must cause error");
    }

    #[test]
    fn apikey_acquisition_failure_is_propagated() {
        let provider = Arc::new(RotatingProvider::new(vec![Err(
            TokenProviderError::Credential("apikey auth down".into()),
        )]));
        let client =
            LazyAuthHttpClient::<ApiKeyStrategy>::new(test_dyn_http(), provider, "s".into());

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .enable_io()
            .build()
            .unwrap();

        let req = Request::builder()
            .uri("http://127.0.0.1:1/")
            .body("{}")
            .unwrap();
        let result = rt.block_on(client.send::<_, bytes::Bytes>(req));

        assert!(result.is_err(), "acquisition failure must cause error");
    }
}
