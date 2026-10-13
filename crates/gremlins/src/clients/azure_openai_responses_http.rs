//! HTTP client wrapper that rewrites Azure OpenAI Responses requests.
//!
//! Rig's `AZURE` dialect still resolves the Responses wire at the account
//! endpoint root (`{endpoint}/responses`) and keeps the deployment in the
//! top-level `model` body field.  Azure's deployment routing needs the
//! deployment in the URL instead:
//!
//! ```text
//! {endpoint}/openai/deployments/{model}/responses?api-version=…
//! ```
//!
//! This wrapper intercepts every outgoing request whose path ends in
//! `/responses`, moves the top-level `model` out of the JSON body, and
//! rewrites the URL to the Azure deployment form.  All other requests pass
//! through unchanged, as do all headers and every other body field.
//!
//! This module is self-contained.  When rig's Responses wire supports Azure
//! deployment-in-URL routing, delete this file and revert the call sites in
//! `backends/azure_openai.rs`.

use std::fmt::Write as _;
use std::future::Future;

use bytes::Bytes;
use http::header::{CONTENT_LENGTH, TRANSFER_ENCODING};
use http::{Request, Response};
use rig_core::http_client::{
    DynHttpClient, Error, HttpClientExt, LazyBody, MultipartForm, Result, StreamingResponse,
};
use rig_core::wasm_compat::WasmCompatSend;

/// Wraps a [`DynHttpClient`] and rewrites Azure OpenAI Responses requests.
///
/// Cheaply cloneable — the inner client, endpoint, and api-version are all
/// cheap to clone.
#[derive(Clone, Debug)]
pub(crate) struct AzureResponsesHttpClient {
    inner: DynHttpClient,
    endpoint: String,
    api_version: String,
}

impl AzureResponsesHttpClient {
    /// Wrap `inner` so Requests-route requests are rewritten to Azure's
    /// deployment-in-URL form using `endpoint` and `api_version`.
    pub(crate) fn new(
        inner: DynHttpClient,
        endpoint: impl Into<String>,
        api_version: impl Into<String>,
    ) -> Self {
        Self {
            inner,
            endpoint: endpoint.into(),
            api_version: api_version.into(),
        }
    }

    /// Rewrite a `/responses` request body and URL.  Non-`/responses`
    /// requests are returned exactly as they arrived.
    #[allow(clippy::result_large_err)]
    fn rewrite_request(
        req: Request<Bytes>,
        endpoint: &str,
        api_version: &str,
    ) -> Result<Request<Bytes>> {
        if !req.uri().path().ends_with("/responses") {
            return Ok(req);
        }

        let (mut parts, body) = req.into_parts();
        let mut value: serde_json::Value = serde_json::from_slice(&body).map_err(|e| {
            Error::instance(std::io::Error::other(format!(
                "Azure OpenAI Responses request body is not valid JSON: {e}"
            )))
        })?;

        let model = value
            .get("model")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                Error::instance(std::io::Error::other(
                    "Azure OpenAI Responses request body is missing the top-level `model` field",
                ))
            })?
            .to_string();

        if let Some(object) = value.as_object_mut() {
            object.remove("model");
        }

        let body = serde_json::to_vec(&value).map_err(|e| {
            Error::instance(std::io::Error::other(format!(
                "failed to serialize Azure OpenAI Responses request body: {e}"
            )))
        })?;

        let deployment = model.trim_matches('/');
        if deployment.is_empty() {
            return Err(Error::instance(std::io::Error::other(
                "Azure OpenAI Responses request body has an empty top-level `model` field",
            )));
        }

        let uri: http::Uri = format!(
            "{}/openai/deployments/{}/responses?api-version={}",
            endpoint.trim_end_matches('/'),
            percent_encode_path_segment(deployment),
            percent_encode_path_segment(api_version),
        )
        .parse()
        .map_err(|e| {
            Error::instance(std::io::Error::other(format!(
                "invalid Azure OpenAI Responses URL: {e}"
            )))
        })?;

        // The body was reserialized without `model`, so any framing headers
        // supplied by the caller no longer match the bytes that will be sent.
        parts.headers.remove(CONTENT_LENGTH);
        parts.headers.remove(TRANSFER_ENCODING);

        parts.uri = uri;
        Ok(Request::from_parts(parts, Bytes::from(body)))
    }
}

/// Percent-encode a URL path segment or query value, keeping only RFC 3986
/// unreserved characters unescaped.
fn percent_encode_path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            write!(&mut encoded, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    encoded
}

impl HttpClientExt for AzureResponsesHttpClient {
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
        let req = req.map(|b| b.into());
        let inner = self.inner.clone();
        let endpoint = self.endpoint.clone();
        let api_version = self.api_version.clone();
        async move {
            let req = Self::rewrite_request(req, &endpoint, &api_version)?;
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
        // Pass-through: Azure OpenAI Responses requests don't use multipart.
        self.inner.send_multipart(req)
    }

    fn send_streaming<T>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = Result<StreamingResponse>> + WasmCompatSend
    where
        T: Into<Bytes> + WasmCompatSend,
    {
        let req = req.map(|b| b.into());
        let inner = self.inner.clone();
        let endpoint = self.endpoint.clone();
        let api_version = self.api_version.clone();
        async move {
            let req = Self::rewrite_request(req, &endpoint, &api_version)?;
            inner.send_streaming(req).await
        }
    }
}

// ── tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::time::Duration;

    // ── construction ─────────────────────────────────────────────────

    fn test_dyn_http() -> DynHttpClient {
        DynHttpClient::new(rig_reqwest::shared())
    }

    /// Spawn a tiny HTTP listener on localhost, return its port.
    fn bind_ephemeral() -> (std::net::TcpListener, u16) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    struct CapturedRequest {
        request_line: String,
        headers: Vec<String>,
        body: Vec<u8>,
    }

    /// Read one HTTP request from `listener` and answer it so the client can
    /// finish cleanly.
    fn read_request(listener: std::net::TcpListener) -> CapturedRequest {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();

        // One BufReader for the whole read: a second reader would drop any
        // body bytes the first one buffered via read-ahead, leaving
        // `read_exact` blocking forever.
        let mut reader = BufReader::new(&mut stream);

        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();

        let mut headers = Vec::new();
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).unwrap();
            if n == 0 || line == "\r\n" || line == "\n" {
                break;
            }
            if let Some(value) = line
                .split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map(|(_, value)| value.trim())
            {
                content_length = value.parse().unwrap_or(0);
            }
            headers.push(line.trim_end().to_string());
        }

        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).unwrap();

        stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            .unwrap();

        CapturedRequest {
            request_line: request_line.trim_end().to_string(),
            headers,
            body,
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .enable_io()
            .build()
            .unwrap()
    }

    // ── body/URL rewriting against a local TCP listener ─────────────

    #[test]
    fn send_rewrites_responses_url_and_body_on_wire() {
        let (listener, port) = bind_ephemeral();
        let endpoint = format!("http://127.0.0.1:{port}");
        let wrapper = AzureResponsesHttpClient::new(
            test_dyn_http(),
            endpoint.clone(),
            "2025-05-15".to_string(),
        );

        let body = serde_json::json!({
            "model": "gpt-4o",
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
            "instructions": "be brief",
            "tools": [],
        });

        let req = Request::builder()
            .method("POST")
            .uri(format!("{endpoint}/responses"))
            .header("x-test-header", "keep-me")
            .header("content-type", "application/json")
            .header("content-length", "9999")
            .body(serde_json::to_vec(&body).unwrap())
            .unwrap();

        let jh = std::thread::spawn(move || read_request(listener));
        let response = runtime().block_on(wrapper.send::<_, bytes::Bytes>(req));
        let captured = jh.join().unwrap();

        response.expect("rewritten Responses request must be sent successfully");

        assert!(
            captured.request_line.starts_with("POST "),
            "unexpected request line: {}",
            captured.request_line
        );
        assert!(
            captured
                .request_line
                .contains("/openai/deployments/gpt-4o/responses?api-version=2025-05-15"),
            "rewritten URL missing from request line: {}",
            captured.request_line
        );
        assert!(
            captured
                .headers
                .iter()
                .any(|header| header.to_lowercase().starts_with("x-test-header: keep-me")),
            "unrelated headers must survive: {:?}",
            captured.headers
        );

        let parsed: serde_json::Value = serde_json::from_slice(&captured.body).unwrap();
        assert!(
            parsed.get("model").is_none(),
            "top-level model must be removed: {parsed}"
        );
        assert!(
            parsed.get("input").is_some(),
            "input must be preserved: {parsed}"
        );
        assert!(
            parsed.get("instructions").is_some(),
            "instructions must be preserved: {parsed}"
        );
        assert!(
            parsed.get("tools").is_some(),
            "tools must be preserved: {parsed}"
        );
    }

    #[test]
    fn send_passes_through_non_responses_requests() {
        let (listener, port) = bind_ephemeral();
        let endpoint = format!("http://127.0.0.1:{port}");
        let wrapper = AzureResponsesHttpClient::new(
            test_dyn_http(),
            endpoint.clone(),
            "2025-05-15".to_string(),
        );

        let body = br#"{"unchanged":true}"#;
        let req = Request::builder()
            .uri(format!("{endpoint}/v1/models"))
            .header("x-test-header", "keep-me")
            .header("content-length", body.len().to_string())
            .body(Bytes::from_static(body))
            .unwrap();

        let jh = std::thread::spawn(move || read_request(listener));
        let response = runtime().block_on(wrapper.send::<_, bytes::Bytes>(req));
        let captured = jh.join().unwrap();

        response.expect("non-Responses request must be sent successfully");

        assert!(
            captured.request_line.contains(" /v1/models "),
            "non-Responses request must not be rewritten: {}",
            captured.request_line
        );
        assert!(
            !captured.request_line.contains("openai/deployments"),
            "non-Responses request must not gain deployment routing: {}",
            captured.request_line
        );
        assert_eq!(captured.body, body);
        assert!(
            captured
                .headers
                .iter()
                .any(|header| header.to_lowercase().starts_with("x-test-header: keep-me")),
            "headers must survive pass-through: {:?}",
            captured.headers
        );
    }

    // ── malformed Responses bodies ───────────────────────────────────

    #[test]
    fn responses_request_with_invalid_json_yields_error() {
        let req = Request::builder()
            .uri("https://example.openai.azure.com/responses")
            .body(Bytes::from_static(b"not json"))
            .unwrap();

        let err = AzureResponsesHttpClient::rewrite_request(
            req,
            "https://example.openai.azure.com",
            "2025-05-15",
        )
        .expect_err("invalid JSON must yield an error");

        assert!(
            err.to_string().contains("not valid JSON"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn responses_request_ignores_path_prefix() {
        let req = Request::builder()
            .uri("https://example.openai.azure.com/v1/responses")
            .body(Bytes::from_static(br#"{"model":"gpt-4o","input":[]}"#))
            .unwrap();

        let rewritten = AzureResponsesHttpClient::rewrite_request(
            req,
            "https://example.openai.azure.com",
            "2025-05-15",
        )
        .expect("rewrite should succeed");

        assert_eq!(
            rewritten.uri().path(),
            "/openai/deployments/gpt-4o/responses"
        );
        assert_eq!(rewritten.uri().query(), Some("api-version=2025-05-15"));
    }

    #[test]
    fn responses_request_percent_encodes_deployment_and_api_version() {
        let req = Request::builder()
            .uri("https://example.openai.azure.com/responses")
            .body(Bytes::from_static(br#"{"model":"gpt 4/o?","input":[]}"#))
            .unwrap();

        let rewritten = AzureResponsesHttpClient::rewrite_request(
            req,
            "https://example.openai.azure.com",
            "2025-05/15",
        )
        .expect("rewrite should succeed");

        assert_eq!(
            rewritten.uri().path(),
            "/openai/deployments/gpt%204%2Fo%3F/responses"
        );
        assert_eq!(rewritten.uri().query(), Some("api-version=2025-05%2F15"));
    }

    #[test]
    fn responses_request_without_model_yields_error() {
        let req = Request::builder()
            .uri("https://example.openai.azure.com/responses")
            .body(Bytes::from_static(br#"{"input":[]}"#))
            .unwrap();

        let err = AzureResponsesHttpClient::rewrite_request(
            req,
            "https://example.openai.azure.com",
            "2025-05-15",
        )
        .expect_err("missing model must yield an error");

        assert!(
            err.to_string()
                .contains("missing the top-level `model` field"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn azure_responses_http_client_is_cloneable() {
        let wrapper = AzureResponsesHttpClient::new(
            test_dyn_http(),
            "https://example.openai.azure.com",
            "2025-05-15",
        );
        let _clone = wrapper.clone();
    }
}
