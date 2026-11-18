use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use indexmap::IndexMap;
use rig_core::completion::CompletionRequest;
use rig_core::http_client::middleware::HttpMiddleware;
use rig_core::http_client::{DynHttpClient, Exchange, Result as HttpResult};
use rig_core::providers::chatgpt::{self, auth::FileTokenStore};
use rig_core::test_utils::{CapturedHttpRequest, MockHttpResponse, SequencedHttpClient};
use rig_core::wasm_compat::WasmBoxedFuture;
use serde_json::json;

use super::ChatGptBackend;
use crate::clients::backend::{Backend, RunParams};
use crate::clients::chatgpt_auth::{store_dir, ACCESS_TOKEN_ENV};
use crate::clients::interactive::InteractiveSession;
use crate::test_support::EnvGuard;

const TASK_SPEC: &str = "chatgpt/gpt-5.3-codex";

fn done_response() -> MockHttpResponse {
    MockHttpResponse::success_typed(include_str!("done.sse"), "text/event-stream")
}

fn isolated_env(root: &Path) -> EnvGuard {
    let mut env = EnvGuard::lock();
    env.set("GREMLINS_SANDBOX_ROOT", root);
    env.remove(ACCESS_TOKEN_ENV);
    env
}

fn build(http: DynHttpClient) -> Arc<dyn Backend> {
    ChatGptBackend::build_with("gpt-5.4", &HashMap::new(), &IndexMap::new(), http).unwrap()
}

fn write_record(path: &Path, token: &str, account: Option<&str>) {
    let record = json!({"access_token": token, "account_id": account, "expires_at": i64::MAX});
    fs::write(path, record.to_string()).unwrap();
}

fn seed_oauth() -> PathBuf {
    let dir = store_dir();
    fs::create_dir_all(&dir).unwrap();
    let path = FileTokenStore::new(dir).path("chatgpt");
    write_record(&path, "first-token", Some("first-account"));
    path
}

fn assert_request(request: &CapturedHttpRequest, model: &str, token: &str, account: Option<&str>) {
    assert_eq!(
        request.uri,
        format!("{}/responses", chatgpt::DIALECT.base_url)
    );
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["model"], model);
    assert_eq!(body["stream"], true);
    assert_eq!(
        request.headers["authorization"].to_str().unwrap(),
        format!("Bearer {token}")
    );
    assert_eq!(
        request
            .headers
            .get("chatgpt-account-id")
            .map(|v| v.to_str().unwrap()),
        account
    );
}

async fn run(backend: &dyn Backend, cwd: &Path) {
    let prompt = "Say hello";
    let params = RunParams {
        prompt: prompt.into(),
        prepopulated_conversation: crate::conversation::new_blank_conversation(prompt, false, None),
        cwd: Some(cwd.to_owned()),
        ..RunParams::empty()
    };
    let completed = tokio::time::timeout(
        Duration::from_secs(10),
        backend.run(params, InteractiveSession::unattached()),
    )
    .await
    .expect("timed out waiting for Responses run")
    .unwrap();
    assert_eq!(completed.exit_code, 0);
    assert_eq!(completed.text_result.as_deref(), Some("Hello from ChatGPT"));
}

#[tokio::test]
async fn run_uses_env_bearer_and_completes_from_responses_stream() {
    let root = tempfile::tempdir().unwrap();
    let mut env = isolated_env(root.path());
    env.set(ACCESS_TOKEN_ENV, "env-token");
    let config_dir = root.path().join("config");
    fs::create_dir(&config_dir).unwrap();
    fs::write(
        config_dir.join("providers.yaml"),
        "chatgpt:\n  token: file-token\n",
    )
    .unwrap();

    let http = SequencedHttpClient::new([done_response()]);
    let backend = build(DynHttpClient::new(http.clone()));
    run(backend.as_ref(), root.path()).await;
    let requests = http.requests();
    assert_eq!(requests.len(), 1);
    assert_request(&requests[0], "gpt-5.4", "env-token", None);
}

#[tokio::test]
async fn same_task_model_uses_current_sign_in_and_stops_sending_after_removal() {
    let root = tempfile::tempdir().unwrap();
    let http = SequencedHttpClient::new([done_response(), done_response(), done_response()]);
    let (backend, path) = {
        let _env = isolated_env(root.path());
        let path = seed_oauth();
        (build(DynHttpClient::new(http.clone())), path)
    };
    let model = backend.make_model(TASK_SPEC).await.unwrap().unwrap();
    let send = || async {
        model
            .stream(CompletionRequest::new("Say hello"))?
            .finish()
            .await
    };

    for (token, account) in [
        ("second-token", Some("second-account")),
        ("third-token", Some("third-account")),
        ("no-account-token", None),
    ] {
        write_record(&path, token, account);
        send().await.unwrap();
        assert_request(
            http.requests().last().unwrap(),
            "gpt-5.3-codex",
            token,
            account,
        );
    }

    fs::remove_file(path).unwrap();
    let error = send().await.unwrap_err().to_string();
    assert!(error.contains("gremlins auth chatgpt login"), "{error}");
    let error = backend.make_model(TASK_SPEC).await.unwrap_err().to_string();
    assert!(error.contains("gremlins auth chatgpt login"), "{error}");
    assert_eq!(
        http.requests().len(),
        3,
        "signed-out requests must not reach the transport"
    );
}

struct ExpireSignInOnResponse(PathBuf);

impl HttpMiddleware for ExpireSignInOnResponse {
    fn after_response<'a>(
        &'a self,
        _: Exchange<'a>,
        _: http::StatusCode,
        _: &'a http::HeaderMap,
    ) -> WasmBoxedFuture<'a, HttpResult<()>> {
        Box::pin(async {
            let record = fs::read_to_string(&self.0).unwrap();
            let mut record: serde_json::Value = serde_json::from_str(&record).unwrap();
            if record["access_token"] == "first-token" {
                record["expires_at"] = json!(0);
                record["refresh_token"] = json!("refresh");
                fs::write(&self.0, record.to_string()).unwrap();
            }
            Ok(())
        })
    }
}

#[tokio::test]
async fn oauth_run_refreshes_expired_sign_in_on_next_tool_turn() {
    let root = tempfile::tempdir().unwrap();
    let _env = isolated_env(root.path());
    let path = seed_oauth();
    let first_turn = json!({
        "type": "response.completed", "response": {
            "status": "completed", "id": "resp_glob", "model": "gpt-5.4", "output": [{
                "type": "function_call", "id": "fc_glob", "call_id": "call_glob",
                "name": "Glob", "arguments": r#"{"pattern":"*.txt"}"#, "status": "completed"
            }]
        }
    });
    let http = SequencedHttpClient::new([
        MockHttpResponse::success_typed(format!("data: {first_turn}\n\n"), "text/event-stream"),
        MockHttpResponse::success(r#"{"access_token":"next-token","refresh_token":"rotated"}"#),
        done_response(),
    ]);
    let backend =
        build(DynHttpClient::new(http.clone()).with_middleware(ExpireSignInOnResponse(path.clone())));
    run(backend.as_ref(), root.path()).await;

    let requests = http.requests();
    assert_eq!(requests.len(), 3);
    assert_request(
        &requests[0],
        "gpt-5.4",
        "first-token",
        Some("first-account"),
    );
    assert_eq!(requests[1].uri, "https://auth.openai.com/oauth/token");
    assert!(!requests[1].headers.contains_key("authorization"));
    assert_request(&requests[2], "gpt-5.4", "next-token", None);
    let record: serde_json::Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(record["refresh_token"], "rotated");
    let body: serde_json::Value = serde_json::from_slice(&requests[2].body).unwrap();
    assert!(
        body["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "function_call_output"),
        "second request must continue the tool turn, not replay the prompt"
    );
}
