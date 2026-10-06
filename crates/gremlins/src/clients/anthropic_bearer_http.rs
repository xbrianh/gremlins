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

use bytes::Bytes;
use http::{HeaderValue, Request, Response};
use rig_core::http_client::{
    HttpClientExt, LazyBody, MultipartForm, ReqwestClient, Result, StreamingResponse,
};
use rig_core::wasm_compat::WasmCompatSend;

/// Wraps a [`ReqwestClient`] and rewrites auth headers on every request.
///
/// Cheaply cloneable — the inner client and the pre-formatted bearer header
/// value are both cheap to clone.
#[derive(Clone, Debug)]
pub(crate) struct BearerHttpClient {
    inner: ReqwestClient,
    bearer: HeaderValue,
}

impl Default for BearerHttpClient {
    fn default() -> Self {
        Self {
            inner: ReqwestClient::new(),
            // Safe: "unused" contains only ASCII alphanumerics.
            bearer: HeaderValue::from_static("Bearer unused"),
        }
    }
}

impl BearerHttpClient {
    /// Wrap `inner` so every request carries `Authorization: Bearer <token>`
    /// instead of whatever API-key header rig's `AnthropicKey` injects.
    ///
    /// Returns an error if `token` contains characters that are invalid in
    /// an HTTP header value (e.g. non-ASCII bytes, newlines).
    pub(crate) fn new(inner: ReqwestClient, token: String) -> std::result::Result<Self, String> {
        let mut bearer = HeaderValue::from_str(&format!("Bearer {}", token.trim()))
            .map_err(|e| format!("invalid bearer token: {e}"))?;
        bearer.set_sensitive(true);
        Ok(Self { inner, bearer })
    }

    /// Strip `x-api-key` and insert `Authorization: Bearer <token>`.
    fn rewrite_request<T>(&self, mut req: Request<T>) -> Request<T> {
        let headers = req.headers_mut();
        headers.remove("x-api-key");
        headers.insert(http::header::AUTHORIZATION, self.bearer.clone());
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

    // ── construction ─────────────────────────────────────────────────

    #[test]
    fn new_rejects_invalid_token() {
        let inner = ReqwestClient::new();
        let result = BearerHttpClient::new(inner, "token\nwith-newline".into());
        assert!(result.is_err());
    }

    #[test]
    fn new_accepts_valid_token() {
        let inner = ReqwestClient::new();
        let result = BearerHttpClient::new(inner, "valid-token".into());
        assert!(result.is_ok());
    }

    #[test]
    fn new_trims_whitespace_from_token() {
        let inner = ReqwestClient::new();
        let wrapper = BearerHttpClient::new(inner, "  padded-token  ".into()).unwrap();
        let req = Request::builder()
            .uri("https://example.com/")
            .body("{}")
            .unwrap();
        let rewritten = wrapper.rewrite_request(req);
        assert_eq!(
            rewritten
                .headers()
                .get("Authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer padded-token"
        );
    }

    // ── header rewriting ─────────────────────────────────────────────

    #[test]
    fn rewrites_x_api_key_to_bearer() {
        let client = ReqwestClient::new();
        let wrapper = BearerHttpClient::new(client, "test-token".into()).unwrap();

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
    fn rewrite_request_without_x_api_key_adds_bearer() {
        let client = ReqwestClient::new();
        let wrapper = BearerHttpClient::new(client, "tok".into()).unwrap();

        let req = Request::builder()
            .uri("https://example.com/")
            .body("{}")
            .unwrap();

        let rewritten = wrapper.rewrite_request(req);
        assert_eq!(
            rewritten
                .headers()
                .get("Authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer tok"
        );
    }

    // ── trait delegation tests ───────────────────────────────────
    //
    // Exercise send / send_streaming against a local TCP listener so we
    // can assert the outgoing request has Authorization and no x-api-key.

    /// Spawn a tiny HTTP listener on localhost, return its port.
    fn bind_ephemeral() -> (std::net::TcpListener, u16) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    #[test]
    fn send_rewrites_headers_on_wire() {
        let (listener, port) = bind_ephemeral();
        let inner = ReqwestClient::new();
        let wrapper = BearerHttpClient::new(inner, "tok".into()).unwrap();

        let req = Request::builder()
            .uri(format!("http://127.0.0.1:{port}/v1/messages"))
            .header("x-api-key", "old")
            .body("{}")
            .unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .enable_io()
            .build()
            .unwrap();

        let jh = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            use std::io::BufRead;
            let mut reader = std::io::BufReader::new(&mut stream);
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
        });

        // Fire and forget — we only care about what the listener sees.
        let _ = rt.block_on(wrapper.send::<_, bytes::Bytes>(req));

        let headers = jh.join().unwrap();
        let has_auth = headers
            .iter()
            .any(|h| h.to_lowercase().starts_with("authorization:"));
        let has_x_api_key = headers
            .iter()
            .any(|h| h.to_lowercase().starts_with("x-api-key:"));
        assert!(has_auth, "Authorization header missing: {headers:?}");
        assert!(!has_x_api_key, "x-api-key must be stripped: {headers:?}");
    }

    #[test]
    fn send_streaming_rewrites_headers_on_wire() {
        let (listener, port) = bind_ephemeral();
        let inner = ReqwestClient::new();
        let wrapper = BearerHttpClient::new(inner, "tok".into()).unwrap();

        let req = Request::builder()
            .uri(format!("http://127.0.0.1:{port}/v1/messages"))
            .header("x-api-key", "old")
            .body("{}")
            .unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .enable_io()
            .build()
            .unwrap();

        let jh = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            use std::io::BufRead;
            let mut reader = std::io::BufReader::new(&mut stream);
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
        });

        let _ = rt.block_on(wrapper.send_streaming(req));

        let headers = jh.join().unwrap();
        let has_auth = headers
            .iter()
            .any(|h| h.to_lowercase().starts_with("authorization:"));
        let has_x_api_key = headers
            .iter()
            .any(|h| h.to_lowercase().starts_with("x-api-key:"));
        assert!(has_auth, "Authorization header missing: {headers:?}");
        assert!(!has_x_api_key, "x-api-key must be stripped: {headers:?}");
    }

    #[test]
    fn send_multipart_is_passthrough() {
        let (listener, port) = bind_ephemeral();
        let inner = ReqwestClient::new();
        let wrapper = BearerHttpClient::new(inner, "tok".into()).unwrap();

        let form = MultipartForm::new();
        let req = Request::builder()
            .uri(format!("http://127.0.0.1:{port}/upload"))
            .body(form)
            .unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .enable_io()
            .build()
            .unwrap();

        let jh = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            use std::io::BufRead;
            let mut reader = std::io::BufReader::new(&mut stream);
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
        });

        let _ = rt.block_on(wrapper.send_multipart::<bytes::Bytes>(req));

        let headers = jh.join().unwrap();
        // Multipart is pass-through: no rewriting.
        let has_auth = headers
            .iter()
            .any(|h| h.to_lowercase().starts_with("authorization:"));
        assert!(
            !has_auth,
            "multipart must not inject Authorization: {headers:?}"
        );
    }

    // ── clone ────────────────────────────────────────────────────────

    #[test]
    fn bearer_http_client_is_cloneable() {
        let client = ReqwestClient::new();
        let wrapper = BearerHttpClient::new(client, "cloned-token".into()).unwrap();
        let _clone = wrapper.clone();
    }
}
