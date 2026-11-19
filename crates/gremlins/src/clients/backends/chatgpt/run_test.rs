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
    let backend = build(
        DynHttpClient::new(http.clone()).with_middleware(ExpireSignInOnResponse(path.clone())),
    );
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

const REPORT: &str = "artifact://report.txt";
const EMPTY: &str = "artifact://empty.txt";
const OPTIONAL: &str = "artifact://notes.txt";
const DIAGNOSTIC: &str = "DISTINCTIVE-RETRY-DIAGNOSTIC";

fn producer_definition(
    model: &str,
    configure: impl FnOnce(crate::builders::AgentBuilder) -> crate::builders::AgentBuilder,
) -> crate::definition::StaticDefinition {
    use crate::builders::{AgentBuilder, DefinitionBuilder, SequenceBuilder};
    let producer = AgentBuilder::new("inner")
        .output("report", REPORT)
        .output("empty", EMPTY)
        .output("notes?", OPTIONAL)
        .prompt("report={report}\nempty={empty}\nnotes={notes}");
    let seq = SequenceBuilder::new("outer")
        .stage(configure(producer).build().unwrap())
        .build()
        .unwrap();
    DefinitionBuilder::new("nested", crate::model_uri::ModelURI::parse(model).unwrap())
        .stage(seq)
        .build()
        .unwrap()
}

fn proposed_done(summary: &str, turn: usize) -> MockHttpResponse {
    let sse = include_str!("done.sse")
        .replace("Hello from ChatGPT", summary)
        .replace("_1\"", &format!("_{turn}\""));
    MockHttpResponse::success_typed(sse, "text/event-stream")
}

fn result_text(request: &CapturedHttpRequest, call_id: &str) -> String {
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    body["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == call_id)
        .and_then(|item| item["output"].as_str())
        .unwrap_or_else(|| panic!("missing tool result for {call_id}: {}", body["input"]))
        .to_owned()
}

// Act as the scripted producer between the first rejection and the next Done.
// Read its actual write destinations from the interpolated model prompt, not
// from the assessor (which must not have run yet) or the rejection message.
struct WriteAfterReminder {
    http: SequencedHttpClient,
    state: std::sync::Weak<dyn crate::executor::state::StateStore + Send + Sync>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
    main_state_dir: PathBuf,
    invocation_log: Option<PathBuf>,
}

impl HttpMiddleware for WriteAfterReminder {
    fn after_response<'a>(
        &'a self,
        _: Exchange<'a>,
        _: http::StatusCode,
        _: &'a http::HeaderMap,
    ) -> WasmBoxedFuture<'a, HttpResult<()>> {
        Box::pin(async move {
            let requests = self.http.requests();
            let state = self.state.upgrade().expect("run still owns the state");
            for uri in [REPORT, EMPTY, OPTIONAL] {
                assert!(!state.is_registered(uri).await, "premature commit of {uri}");
            }
            if requests.len() != 2 {
                return Ok(());
            }
            assert_eq!(self.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
            if let Some(log) = &self.invocation_log {
                assert!(!log.exists(), "custom command ran before outputs existed");
            }
            let reminder = result_text(&requests[1], "call_1");
            let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
            let prompt = body["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|item| item["content"][0]["text"].as_str())
                .find(|text| text.lines().any(|line| line.starts_with("report=")))
                .unwrap();
            for (key, uri, contents) in [
                ("report", REPORT, Some("produced")),
                ("empty", EMPTY, Some("")),
                ("notes", OPTIONAL, None),
            ] {
                let prefix = format!("{key}=");
                let path = Path::new(
                    prompt
                        .lines()
                        .find_map(|line| line.strip_prefix(&prefix))
                        .unwrap(),
                );
                assert!(
                    path.is_absolute() && path.ends_with(uri.trim_start_matches("artifact://"))
                );
                assert!(
                    !path.starts_with(&self.main_state_dir),
                    "not a checkout: {path:?}"
                );
                assert!(!path.exists(), "presence check created {path:?}");
                assert_eq!(
                    reminder.contains(path.to_str().unwrap()) || reminder.contains(uri),
                    contents.is_some(),
                    "{reminder}"
                );
                if let Some(text) = contents {
                    tokio::fs::write(path, text).await.unwrap();
                }
            }
            Ok(())
        })
    }
}

async fn run_producer(
    fx: &crate::test_support::GitSandbox,
    definition: &crate::definition::StaticDefinition,
    model: &str,
    summaries: &[&str],
    calls: Arc<std::sync::atomic::AtomicUsize>,
    registry: Option<Arc<crate::executor::supervisor::run_registry::RunRegistry>>,
    invocation_log: Option<PathBuf>,
) -> (crate::executor::gremlin::Gremlin, Vec<CapturedHttpRequest>) {
    use crate::clients::client::{install_test_backend, Client};
    use crate::executor::gremlin::{Gremlin, GremlinConfig};
    use gremlins_magic_bus::{subscribe_run, EventKind};

    let mut gremlin = Gremlin::init(
        "gr-producer",
        fx.definition_path(),
        definition,
        &HashMap::new(),
        None,
        &GremlinConfig {
            ephemeral: true,
            ..Default::default()
        },
    )
    .unwrap();
    gremlin.runtime_config.registry = registry;
    // A same-named file outside the producer's artifact checkout must not satisfy Done.
    fs::write(gremlin.cwd().join("report.txt"), "decoy").unwrap();
    let http = SequencedHttpClient::new(
        summaries
            .iter()
            .enumerate()
            .map(|(i, s)| proposed_done(s, i + 1)),
    );
    seed_oauth();
    let transport = DynHttpClient::new(http.clone()).with_middleware(WriteAfterReminder {
        http: http.clone(),
        state: Arc::downgrade(&gremlin.state.store_handle()),
        calls,
        main_state_dir: gremlin.state.state_dir().canonicalize().unwrap(),
        invocation_log,
    });
    let spec = Client::parse(model).unwrap();
    let backend =
        ChatGptBackend::build_with(spec.model(), &HashMap::new(), &IndexMap::new(), transport)
            .unwrap();
    install_test_backend(&spec.to_string(), backend);
    let (sub, mut events) = subscribe_run(gremlin.id.as_str());
    let _guard = gremlins_magic_bus::connection::BusSubGuard::new(sub);
    let exit = tokio::time::timeout(Duration::from_secs(30), gremlin.run(None))
        .await
        .expect("producer timed out")
        .unwrap();
    assert_eq!(exit, 0);
    let mut completions = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let EventKind::Done { summary, .. } = event.kind() {
            completions.push(summary.clone());
        }
    }
    assert_eq!(completions, [*summaries.last().unwrap()]);
    assert_eq!(http.requests().len(), summaries.len());
    assert_eq!(
        gremlin.state.content(REPORT, None).await.unwrap(),
        "produced"
    );
    assert_eq!(gremlin.state.content(EMPTY, None).await.unwrap(), "");
    assert!(!gremlin.state.is_registered(OPTIONAL).await);
    (gremlin, http.requests())
}

#[tokio::test]
async fn nested_native_done_check_retries_same_conversation_through_public_run() {
    use crate::builders::DoneVerdict;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let fx = crate::test_support::GitSandbox::new();
    if fx.is_skipped() {
        return;
    }
    let model = "chatgpt/gpt-done-check";
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let contexts = observed.clone();
    let definition = producer_definition(model, |agent| {
        agent.done_check(move |ctx| {
            let n = seen.fetch_add(1, Ordering::SeqCst);
            assert_eq!(ctx.summary, ["review", "accepted"][n]);
            assert!(!ctx.cancel.is_cancelled());
            assert_eq!(ctx.outputs.len(), 2);
            for (key, uri, contents) in [("report", REPORT, "produced"), ("empty", EMPTY, "")] {
                let output = ctx.outputs.iter().find(|o| o.key == key).unwrap();
                assert_eq!(output.uri, uri);
                assert_eq!(fs::read_to_string(&output.path).unwrap(), contents);
            }
            contexts.lock().unwrap().push(ctx);
            async move {
                if n == 0 {
                    DoneVerdict::Retry(DIAGNOSTIC.into())
                } else {
                    DoneVerdict::Accept
                }
            }
        })
    });
    let (gremlin, requests) = run_producer(
        &fx,
        &definition,
        model,
        &["missing", "review", "accepted"],
        calls.clone(),
        None,
        None,
    )
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(result_text(&requests[2], "call_2").contains(DIAGNOSTIC));
    let missing = result_text(&requests[1], "call_1");
    let committed_report = PathBuf::from(gremlin.state.data_uri(REPORT).await.unwrap());
    let committed_empty = PathBuf::from(gremlin.state.data_uri(EMPTY).await.unwrap());
    for ctx in observed.lock().unwrap().iter() {
        assert_eq!(ctx.cwd, gremlin.cwd());
        for output in &ctx.outputs {
            let committed = if output.uri == REPORT {
                &committed_report
            } else {
                &committed_empty
            };
            assert_ne!(
                &output.path, committed,
                "assessor saw the main registry path"
            );
            assert_eq!(output.path.file_name(), committed.file_name());
            assert!(missing.contains(output.path.to_str().unwrap()));
        }
    }
}

/// Builds this checkout's CLI into a dedicated target directory and returns
/// the directory holding the binary. Failure to build fails the test.
fn build_checkout_cli() -> PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let target = root.join("target/dsl-done-cli");
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let status = std::process::Command::new(cargo)
            .args(["build", "-q", "-p", "gremlins-cli", "--target-dir"])
            .arg(&target)
            .current_dir(&root)
            .status()
            .expect("cargo must be runnable to build the CLI");
        assert!(status.success(), "building the checkout CLI failed");
        let dir = target.join("debug");
        assert!(dir.join("gremlins").exists(), "built CLI is missing");
        dir
    })
    .clone()
}

/// A DSL agent reloaded from a hermetic snapshot runs through the real CLI for
/// required outputs, then the interpolated custom command, and only a repaired
/// Done completes.
#[tokio::test]
async fn reloaded_dsl_agent_runs_cli_then_custom_check() {
    use crate::definition::{GremlinDefinition, StaticDefinition};
    use crate::executor::dsl_done::DSL_STAGE_KEY;
    let mut fx = crate::test_support::GitSandbox::new();
    assert!(!fx.is_skipped(), "git sandbox setup must succeed");
    let cli_dir = build_checkout_cli();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let joined =
        std::env::join_paths(std::iter::once(cli_dir).chain(std::env::split_paths(&path))).unwrap();
    fx.set_env("PATH", joined);

    let supervisor = crate::executor::supervisor::ArtifactTestSupervisor::start();
    let log = tempfile::tempdir().unwrap();
    let log = log.path().join("invocations");
    let command = format!(
        "pwd >> {log}; test -e {{report}} || exit 2; \
         test -z \"$GREMLINS_ARTIFACT_CHECK_SCOPE\" || exit 2; if [ $(wc -l < {log}) -eq 1 ]; then echo {DIAGNOSTIC}; exit 1; fi",
        log = log.display()
    );
    let model = "chatgpt/gpt-dsl-done";
    let built = producer_definition(model, |agent| {
        agent
            .option(DSL_STAGE_KEY, true)
            .option("done_check", command)
    });
    let bytes = built.serialize().unwrap();
    let json = StaticDefinition::deserialize_owned(&bytes)
        .unwrap()
        .to_expanded_json()
        .to_string();
    assert!(
        json.contains("done_check") && json.contains("{report}"),
        "{json}"
    );
    let definition = StaticDefinition::deserialize_owned(&bytes).unwrap();

    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (gremlin, requests) = run_producer(
        &fx,
        &definition,
        model,
        &["missing", "review", "accepted"],
        calls,
        Some(supervisor.registry()),
        Some(log.clone()),
    )
    .await;
    // The missing-output rejection came before the custom command ever ran.
    let reminder = result_text(&requests[1], "call_1");
    assert!(!reminder.contains(DIAGNOSTIC));
    assert!(
        reminder.contains("report.txt") && !reminder.contains("notes.txt"),
        "{reminder}"
    );
    assert!(result_text(&requests[2], "call_2").contains(DIAGNOSTIC));
    let cwds = fs::read_to_string(&log).unwrap();
    assert_eq!(cwds.lines().count(), 2, "{cwds}");
    for line in cwds.lines() {
        assert_eq!(
            Path::new(line).canonicalize().unwrap(),
            gremlin.cwd().canonicalize().unwrap()
        );
    }
    loop_scoped_text_then_done(&fx, &supervisor).await;
}

/// A nested producer inside a repeating sequence, with loop-scoped URIs.
fn loop_producer_definition(
    model: &str,
    configure: impl FnOnce(crate::builders::AgentBuilder) -> crate::builders::AgentBuilder,
) -> crate::definition::StaticDefinition {
    use crate::builders::{AgentBuilder, DefinitionBuilder, SequenceBuilder};
    let producer = AgentBuilder::new("inner")
        .output("report", "artifact://{loop_iter}/report.txt")
        .output("notes?", "artifact://{loop_iter}/notes.txt")
        .prompt("report={report}\nnotes={notes}");
    let seq = SequenceBuilder::new("outer")
        .max_iterations(2)
        .skip_if_exists("artifact://{loop_iter}/report.txt")
        .stage(configure(producer).build().unwrap())
        .build()
        .unwrap();
    DefinitionBuilder::new("nested", crate::model_uri::ModelURI::parse(model).unwrap())
        .stage(seq)
        .build()
        .unwrap()
}

fn reload(definition: &crate::definition::StaticDefinition) -> crate::definition::StaticDefinition {
    use crate::definition::{GremlinDefinition, StaticDefinition};
    StaticDefinition::deserialize_owned(&definition.serialize().unwrap()).unwrap()
}

fn text_only(text: &str) -> MockHttpResponse {
    let event = json!({
        "type": "response.completed", "response": {
            "status": "completed", "id": "resp_text", "model": "gpt-5.4", "output": [{
                "type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
                "content": [{"type": "output_text", "text": text, "annotations": []}]
            }]
        }
    });
    MockHttpResponse::success_typed(format!("data: {event}\n\n"), "text/event-stream")
}

/// Puts the checkout CLI first on `PATH` for the rest of the test.
fn use_checkout_cli(fx: &mut crate::test_support::GitSandbox) {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let joined = std::env::join_paths(
        std::iter::once(build_checkout_cli()).chain(std::env::split_paths(&path)),
    )
    .unwrap();
    fx.set_env("PATH", joined);
}

fn init_gremlin(
    fx: &crate::test_support::GitSandbox,
    id: &str,
    definition: &crate::definition::StaticDefinition,
    registry: Option<Arc<crate::executor::supervisor::run_registry::RunRegistry>>,
    cancel: Option<Arc<crate::clients::CancelToken>>,
) -> crate::executor::gremlin::Gremlin {
    use crate::executor::gremlin::{Gremlin, GremlinConfig};
    let mut gremlin = Gremlin::init(
        id,
        fx.definition_path(),
        definition,
        &HashMap::new(),
        None,
        &GremlinConfig {
            ephemeral: true,
            ..Default::default()
        },
    )
    .unwrap();
    gremlin.runtime_config.registry = registry;
    gremlin.cancel_token = cancel;
    // A same-named file outside the producer's artifact checkout must not satisfy Done.
    fs::write(gremlin.cwd().join("report.txt"), "decoy").unwrap();
    gremlin
}

/// Runs the gremlin against `transport`; returns the run result (stringified
/// error) and the summaries of every emitted completion.
async fn drive(
    gremlin: &mut crate::executor::gremlin::Gremlin,
    model: &str,
    transport: DynHttpClient,
) -> (Result<i32, String>, Vec<String>) {
    let (result, completions, _) = drive_with_stages(gremlin, model, transport).await;
    (result, completions)
}

/// Like [`drive`], also returning the leaf stages entered, in order.
async fn drive_with_stages(
    gremlin: &mut crate::executor::gremlin::Gremlin,
    model: &str,
    transport: DynHttpClient,
) -> (Result<i32, String>, Vec<String>, Vec<String>) {
    use crate::clients::client::{install_test_backend, Client};
    use gremlins_magic_bus::{subscribe_run, EventKind};
    seed_oauth();
    let spec = Client::parse(model).unwrap();
    let backend =
        ChatGptBackend::build_with(spec.model(), &HashMap::new(), &IndexMap::new(), transport)
            .unwrap();
    install_test_backend(&spec.to_string(), backend);
    let (sub, mut events) = subscribe_run(gremlin.id.as_str());
    let _guard = gremlins_magic_bus::connection::BusSubGuard::new(sub);
    let result = tokio::time::timeout(Duration::from_secs(30), gremlin.run(None))
        .await
        .expect("run timed out")
        .map_err(|e| e.to_string());
    let (mut completions, mut stages) = (Vec::new(), Vec::new());
    while let Ok(event) = events.try_recv() {
        match event.kind() {
            EventKind::Done { summary, .. } => completions.push(summary.clone()),
            EventKind::StageTransition { stage, sub_stage, .. } => {
                stages.push(sub_stage.clone().unwrap_or_else(|| stage.clone()));
            }
            _ => {}
        }
    }
    (result, completions, stages)
}

/// Writes the empty required output of the loop producer once the third
/// response arrives, after checking the custom command has not run yet.
struct WriteLoopOutput {
    http: SequencedHttpClient,
    log: PathBuf,
    cwd: PathBuf,
}

impl HttpMiddleware for WriteLoopOutput {
    fn after_response<'a>(
        &'a self,
        _: Exchange<'a>,
        _: http::StatusCode,
        _: &'a http::HeaderMap,
    ) -> WasmBoxedFuture<'a, HttpResult<()>> {
        Box::pin(async move {
            let requests = self.http.requests();
            if requests.len() <= 3 {
                assert!(
                    !self.log.exists(),
                    "custom command ran before outputs existed"
                );
            }
            if requests.len() != 3 {
                return Ok(());
            }
            let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
            let prompt = body["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|item| item["content"][0]["text"].as_str())
                .find(|text| text.lines().any(|line| line.starts_with("report=")))
                .unwrap();
            let path = Path::new(
                prompt
                    .lines()
                    .find_map(|line| line.strip_prefix("report="))
                    .unwrap(),
            );
            assert!(
                path.is_absolute() && !path.starts_with(&self.cwd),
                "{path:?}"
            );
            tokio::fs::write(path, "").await.unwrap();
            Ok(())
        })
    }
}

/// Text-only turn, then a Done, both with the required output missing: each
/// rejection names the resolved required URI only, and the custom command
/// stays unrun until the output exists in the producer checkout.
async fn loop_scoped_text_then_done(
    fx: &crate::test_support::GitSandbox,
    supervisor: &crate::executor::supervisor::ArtifactTestSupervisor,
) {
    let log = tempfile::tempdir().unwrap();
    let log = log.path().join("invocations");
    let command = format!(
        "pwd >> {log}; test -e {{report}} || exit 2; \
         if [ $(wc -l < {log}) -eq 1 ]; then echo {DIAGNOSTIC}; exit 1; fi",
        log = log.display()
    );
    let model = "chatgpt/gpt-dsl-loop";
    let definition = reload(&loop_producer_definition(model, |agent| {
        agent.option("done_check", command)
    }));
    let mut gremlin = init_gremlin(
        fx,
        "gr-loop",
        &definition,
        Some(supervisor.registry()),
        None,
    );
    let http = SequencedHttpClient::new([
        text_only("thinking"),
        proposed_done("missing", 2),
        proposed_done("review", 3),
        proposed_done("accepted", 4),
    ]);
    let transport = DynHttpClient::new(http.clone()).with_middleware(WriteLoopOutput {
        http: http.clone(),
        log: log.clone(),
        cwd: gremlin.cwd(),
    });
    let (result, completions) = drive(&mut gremlin, model, transport).await;
    assert_eq!(result, Ok(0));
    assert_eq!(completions, ["accepted"]);
    let requests = http.requests();
    assert_eq!(requests.len(), 4);
    let loop_report = loop_report_uri(&requests[0]);
    let loop_notes = loop_report.replace("report.txt", "notes.txt");
    // The artifact reminder (newest message of request 2) and the Done
    // rejection (call_2 result) are inspected separately, not via history.
    let reminder = last_input_text(&requests[1]);
    let rejection = result_text(&requests[2], "call_2");
    for message in [&reminder, &rejection] {
        assert!(message.contains(&loop_report), "{message}");
        assert!(!message.contains(&loop_notes), "{message}");
        assert!(!message.contains("{loop_iter}"), "{message}");
        assert!(!message.contains(DIAGNOSTIC), "{message}");
    }
    assert!(result_text(&requests[3], "call_3").contains(DIAGNOSTIC));
    let cwds = fs::read_to_string(&log).unwrap();
    assert_eq!(cwds.lines().count(), 2, "{cwds}");
    for line in cwds.lines() {
        assert_eq!(
            Path::new(line).canonicalize().unwrap(),
            gremlin.cwd().canonicalize().unwrap()
        );
    }
}

/// The loop-resolved required URI, derived from the first request's prompt.
fn loop_report_uri(first_request: &CapturedHttpRequest) -> String {
    let first: serde_json::Value = serde_json::from_slice(&first_request.body).unwrap();
    let scope = first["input"][0]["content"][0]["text"]
        .as_str()
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("report="))
        .and_then(|path| Path::new(path).parent()?.file_name()?.to_str())
        .unwrap()
        .to_owned();
    format!("artifact://{scope}/report.txt")
}

/// Text of the newest item of a request's input (message or tool result).
fn last_input_text(request: &CapturedHttpRequest) -> String {
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    let last = body["input"].as_array().unwrap().last().unwrap().clone();
    last["output"]
        .as_str()
        .or_else(|| last["content"][0]["text"].as_str())
        .unwrap_or_else(|| panic!("no text in {last}"))
        .to_owned()
}

/// A stage with only a custom command, whose checker is cancelled mid-run.
fn command_only_definition(model: &str, command: &str) -> crate::definition::StaticDefinition {
    use crate::builders::{AgentBuilder, DefinitionBuilder, SequenceBuilder};
    use crate::executor::dsl_done::DSL_STAGE_KEY;
    let agent = AgentBuilder::new("inner")
        .option(DSL_STAGE_KEY, true)
        .option("done_check", command)
        .prompt("go");
    let seq = SequenceBuilder::new("outer")
        .stage(agent.build().unwrap())
        .build()
        .unwrap();
    reload(
        &DefinitionBuilder::new("nested", crate::model_uri::ModelURI::parse(model).unwrap())
            .stage(seq)
            .build()
            .unwrap(),
    )
}

/// Whether `pid` names a live process.
fn process_alive(pid: i32) -> bool {
    // SAFETY: signal 0 only probes for existence.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn read_pid(path: &Path) -> i32 {
    fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("{path:?}: {e}"))
        .trim()
        .parse()
        .unwrap()
}

/// Process identities a running check records before signalling readiness.
struct CheckPids {
    checker: i32,
    descendant: i32,
}

impl CheckPids {
    fn read(dir: &Path) -> Self {
        Self {
            checker: read_pid(&dir.join("checker")),
            descendant: read_pid(&dir.join("descendant")),
        }
    }

    fn assert_all_stopped(&self) {
        assert!(!process_alive(self.checker), "checker outlived the run");
        assert!(
            !process_alive(self.descendant),
            "descendant outlived the run"
        );
    }
}

/// Shell prelude recording the checker and delayed-writer identities in `dir`,
/// the writer itself signals readiness (`entered`) once running; the script
/// ends by waiting on the writer.
fn hold_script(dir: &Path, alive: &Path) -> String {
    format!(
        "echo $$ > {d}/checker.tmp; mv {d}/checker.tmp {d}/checker\n\
         (touch {d}/entered; sleep 1; touch {alive}) &\n\
         echo $! > {d}/descendant.tmp; mv {d}/descendant.tmp {d}/descendant\n\
         wait\n",
        d = dir.display(),
        alive = alive.display()
    )
}

/// Drives the run until the check signals `entered`, calls `while_held`, then
/// cancels. The startup wait is bounded and abandoned if the run ends early.
/// Asserts the cancellation outcome, prompt return, no completion and no
/// further model request, then that the checker and descendant are gone.
async fn cancel_when_entered(
    gremlin: &mut crate::executor::gremlin::Gremlin,
    cancel: Arc<crate::clients::CancelToken>,
    dir: &Path,
    model: &str,
    http: &SequencedHttpClient,
    while_held: impl FnOnce(&SequencedHttpClient),
    at_return: impl FnOnce(),
) {
    let finished = std::sync::atomic::AtomicBool::new(false);
    let pids = std::cell::OnceCell::<CheckPids>::new();
    let (driven, returned_after) = tokio::join!(
        async {
            let driven = drive(gremlin, model, DynHttpClient::new(http.clone())).await;
            // Immediately at return, without yielding: no grace period.
            if let Some(pids) = pids.get() {
                pids.assert_all_stopped();
            }
            at_return();
            finished.store(true, std::sync::atomic::Ordering::SeqCst);
            driven
        },
        async {
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            // Ready only once the writer itself has started and both
            // identities are recorded.
            let ready = || {
                ["entered", "checker", "descendant"]
                    .iter()
                    .all(|f| dir.join(f).exists())
            };
            while !ready() {
                assert!(
                    !finished.load(std::sync::atomic::Ordering::SeqCst),
                    "run ended before the check started"
                );
                assert!(std::time::Instant::now() < deadline, "check never started");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            while_held(http);
            let live = pids.get_or_init(|| CheckPids::read(dir));
            assert!(process_alive(live.checker) && process_alive(live.descendant));
            cancel.cancel();
            let cancelled_at = std::time::Instant::now();
            while !finished.load(std::sync::atomic::Ordering::SeqCst) {
                assert!(
                    cancelled_at.elapsed() < Duration::from_secs(4),
                    "slow cancellation"
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            cancelled_at.elapsed()
        }
    );
    assert!(returned_after < Duration::from_secs(4), "slow cancellation");
    let error = driven.0.expect_err("cancelled run must not succeed");
    assert!(
        error.to_lowercase().contains("cancel"),
        "not a cancellation: {error}"
    );
    assert!(driven.1.is_empty(), "completed after cancellation");
    assert_eq!(
        http.requests().len(),
        1,
        "model asked again after cancellation"
    );
}

#[tokio::test]
async fn cancelling_standalone_done_stops_custom_check_process_group() {
    let mut fx = crate::test_support::GitSandbox::new();
    assert!(!fx.is_skipped(), "git sandbox setup must succeed");
    use_checkout_cli(&mut fx);
    let dir = tempfile::tempdir().unwrap();
    let alive = dir.path().join("alive");
    let command = hold_script(dir.path(), &alive);
    let model = "chatgpt/gpt-dsl-cancel-done";
    let definition = command_only_definition(model, &command);
    let cancel = crate::clients::CancelToken::new();
    let mut gremlin = init_gremlin(
        &fx,
        "gr-cancel-done",
        &definition,
        None,
        Some(cancel.clone()),
    );
    let http = SequencedHttpClient::new([proposed_done("done", 1), proposed_done("again", 2)]);
    cancel_when_entered(
        &mut gremlin,
        cancel,
        dir.path(),
        model,
        &http,
        |_| {},
        || {},
    )
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!alive.exists(), "descendant outlived cancellation");
}

#[tokio::test]
async fn cancelling_text_turn_stops_artifact_check_and_revokes_scope() {
    use std::os::unix::fs::PermissionsExt;
    let mut fx = crate::test_support::GitSandbox::new();
    assert!(!fx.is_skipped(), "git sandbox setup must succeed");
    let supervisor = crate::executor::supervisor::ArtifactTestSupervisor::start();
    let dir = tempfile::tempdir().unwrap();
    let alive = dir.path().join("alive");
    // Wraps the real checkout CLI: runs it, records its status and the scope
    // token, then holds the scope open until cancelled.
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let script = bin.join("gremlins");
    let real = build_checkout_cli().join("gremlins");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf %s \"$GREMLINS_ARTIFACT_CHECK_SCOPE\" > {d}/token\nprintf %s \"$2\" > {d}/run_id\n\
             {real} \"$@\" 2> {d}/cli_stderr\necho $? > {d}/cli_status\n{hold}",
            d = dir.path().display(),
            real = real.display(),
            hold = hold_script(dir.path(), &alive)
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::env::var_os("PATH").unwrap_or_default();
    fx.set_env(
        "PATH",
        std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&path))).unwrap(),
    );
    let model = "chatgpt/gpt-dsl-cancel-text";
    let definition = reload(&loop_producer_definition(model, |agent| agent));
    let cancel = crate::clients::CancelToken::new();
    let run_id = "gr-cancel-text";
    let mut gremlin = init_gremlin(
        &fx,
        run_id,
        &definition,
        Some(supervisor.registry()),
        Some(cancel.clone()),
    );
    let registry = supervisor.registry();
    let run_id_path = dir.path().join("run_id");
    let query = |token: &str, uri: &str| {
        // The run ID the real CLI was invoked with.
        let id = fs::read_to_string(&run_id_path).unwrap();
        assert!(id.starts_with(run_id), "{id}");
        registry.missing_in_scope(&id, token, &[uri.to_string()])
    };
    let token_path = dir.path().join("token");
    let http = SequencedHttpClient::new([text_only("thinking"), proposed_done("again", 2)]);
    cancel_when_entered(
        &mut gremlin,
        cancel,
        dir.path(),
        model,
        &http,
        |http| {
            let token = fs::read_to_string(&token_path).unwrap();
            assert!(!token.is_empty(), "check ran without a scope token");
            let uri = loop_report_uri(&http.requests()[0]);
            // The real CLI ran against the live scope and found the output missing.
            assert_eq!(read_pid(&dir.path().join("cli_status")), 1);
            assert_eq!(query(&token, &uri), Ok(vec![uri.clone()]));
            let undeclared = query(&token, "artifact://x/report.txt").unwrap_err();
            assert!(undeclared.contains("not declared"), "{undeclared}");
        },
        || {
            // At return: the previously live pending-output token is revoked.
            let token = fs::read_to_string(&token_path).unwrap();
            let uri = loop_report_uri(&http.requests()[0]);
            let revoked = query(&token, &uri).unwrap_err();
            assert!(revoked.contains("invalid or expired"), "{revoked}");
        },
    )
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!alive.exists(), "descendant outlived cancellation");
}

#[tokio::test]
async fn pre_option_snapshot_uses_default_scoped_cli_check() {
    let mut fx = crate::test_support::GitSandbox::new();
    assert!(!fx.is_skipped(), "git sandbox setup must succeed");
    use_checkout_cli(&mut fx);
    let supervisor = crate::executor::supervisor::ArtifactTestSupervisor::start();
    let model = "chatgpt/gpt-dsl-default";
    let definition = reload(&producer_definition(model, |agent| agent));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // The scripted producer writes after the first rejection; Done completes only then.
    let (_, requests) = run_producer(
        &fx,
        &definition,
        model,
        &["missing", "accepted"],
        calls,
        Some(supervisor.registry()),
        None,
    )
    .await;
    assert_eq!(requests.len(), 2);
    // The scoped CLI reports missing outputs by URI; a native file predicate
    // would name filesystem paths. Required outputs only, never the optional.
    let reminder = result_text(&requests[1], "call_1");
    assert!(
        reminder.contains(REPORT) && reminder.contains(EMPTY),
        "{reminder}"
    );
    assert!(!reminder.contains(OPTIONAL), "{reminder}");
    // URIs only, never filesystem paths: the CLI's diagnostic, not a native predicate's.
    let residue = reminder.replace(REPORT, "").replace(EMPTY, "");
    assert!(
        !residue.contains("report.txt") && !residue.contains("empty.txt"),
        "{reminder}"
    );
}

#[tokio::test]
async fn no_outputs_and_no_command_complete_without_executables() {
    use crate::builders::{AgentBuilder, DefinitionBuilder, SequenceBuilder};
    let fx = crate::test_support::GitSandbox::new();
    assert!(!fx.is_skipped(), "git sandbox setup must succeed");
    let model = "chatgpt/gpt-dsl-bare";
    let seq = SequenceBuilder::new("outer")
        .stage(AgentBuilder::new("inner").prompt("go").build().unwrap())
        .build()
        .unwrap();
    let definition = reload(
        &DefinitionBuilder::new("nested", crate::model_uri::ModelURI::parse(model).unwrap())
            .stage(seq)
            .build()
            .unwrap(),
    );
    let empty = tempfile::tempdir().unwrap();
    let mut gremlin = init_gremlin(&fx, "gr-bare", &definition, None, None);
    gremlin
        .env
        .insert("PATH".into(), empty.path().display().to_string());
    let http = SequencedHttpClient::new([proposed_done("finished", 1)]);
    let (result, completions) = drive(&mut gremlin, model, DynHttpClient::new(http.clone())).await;
    assert_eq!(result, Ok(0));
    assert_eq!(completions, ["finished"]);
    assert_eq!(http.requests().len(), 1);
}

/// With every retry budget at zero, operational check failures end the run
/// with a bounded diagnostic instead of retrying or completing.
async fn assert_operational_failure(command: Option<&str>, expected: &str) -> String {
    let mut fx = crate::test_support::GitSandbox::new();
    assert!(!fx.is_skipped(), "git sandbox setup must succeed");
    fx.set_env("GREMLINS_ARTIFACT_REMINDER_BUDGET", "0");
    fx.set_env("GREMLINS_COMPLETION_NUDGE_BUDGET", "0");
    let supervisor = crate::executor::supervisor::ArtifactTestSupervisor::start();
    let model = "chatgpt/gpt-dsl-operational";
    let empty = tempfile::tempdir().unwrap();
    let (definition, registry) = match command {
        Some(command) => (command_only_definition(model, command), None),
        None => {
            // Fixture setup keeps a PATH with git only; the check's own PATH
            // is the empty directory (below), so the error comes from spawning it.
            let git = which_git();
            let tools = empty.path().join("git-only");
            fs::create_dir(&tools).unwrap();
            std::os::unix::fs::symlink(git, tools.join("git")).unwrap();
            fx.set_env("PATH", tools);
            (
                reload(&producer_definition(model, |agent| agent)),
                Some(supervisor.registry()),
            )
        }
    };
    let mut gremlin = init_gremlin(&fx, "gr-operational", &definition, registry, None);
    if command.is_none() {
        let nothing = empty.path().join("nothing");
        fs::create_dir(&nothing).unwrap();
        gremlin
            .env
            .insert("PATH".into(), nothing.display().to_string());
    }
    let http = SequencedHttpClient::new([proposed_done("one", 1), proposed_done("two", 2)]);
    let (result, completions) = drive(&mut gremlin, model, DynHttpClient::new(http.clone())).await;
    let error = result.expect_err("operational failure must fail the run");
    assert!(error.contains(expected), "{error}");
    assert!(
        error.len() < 8192,
        "unbounded diagnostic: {} bytes",
        error.len()
    );
    assert!(completions.is_empty());
    assert_eq!(http.requests().len(), 1, "operational failure was retried");
    error
}

fn which_git() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("git"))
        .find(|candidate| candidate.exists())
        .expect("git must be on PATH")
}

const BIG: &str = "RECOGNIZABLE-BIG-DIAGNOSTIC";

#[tokio::test]
async fn failed_check_launch_fails_the_run_at_zero_nudges() {
    assert_operational_failure(None, "failed to start").await;
}

#[tokio::test]
async fn custom_exit_two_fails_the_run_at_zero_nudges() {
    // Far larger than the diagnostic cap, with a recognizable head.
    let command = format!("echo {BIG} >&2; yes xxxxxxxxxxxxxxxx | head -c 50000 >&2; exit 2");
    let error = assert_operational_failure(Some(&command), "exited 2").await;
    assert!(error.contains(BIG), "diagnostic lost: {error}");
    assert!(
        error.len() < 5000,
        "diagnostic not capped: {} bytes",
        error.len()
    );
}

#[tokio::test]
async fn standalone_reloaded_dsl_agent_checks_checkout_files() {
    let fx = crate::test_support::GitSandbox::new();
    assert!(!fx.is_skipped(), "git sandbox setup must succeed");
    let model = "chatgpt/gpt-standalone-dsl";
    let definition = reload(&producer_definition(model, |agent| agent));
    let (_, requests) = run_producer(
        &fx,
        &definition,
        model,
        &["missing", "accepted"],
        Default::default(),
        None,
        None,
    )
    .await;
    let reminder = result_text(&requests[1], "call_1");
    assert!(!reminder.contains("artifact://"), "{reminder}");
}

// ── shared `implement` stage: no-new-changes nudge ──────────────────────────

const IMPLEMENT_DEFINITION: &str = r#"{
  prompt_dir: "prompts",
  bootstrap: {
    source: { plan: { type: "filepath" } },
    launch_cmds: [
      'gremlins:bind_artifact("artifact://plan.md", plan)',
      'SHA=$(git -C "$GREMLINS_PROJECT_ROOT" rev-parse HEAD) && git -C "$GREMLINS_PROJECT_ROOT" worktree add --detach "$GREMLIN_WORKDIR" "$SHA" && echo "$SHA" > "$GREMLINS_STATE_DIR/artifacts/base_sha"',
    ],
    cli_out: { base_sha: "artifact://base_sha" },
    env: 'export PATH="${GREMLINS_PROJECT_ROOT}/.gremlins/bin:${PATH}"\n',
  },
  default_model: "chatgpt/gpt-implement-nudge",
  "stage-definitions": { "check-and-fix": "check-and-fix" },
  stages: [
    {
      name: "implement-and-qa",
      type: "implement-and-qa",
      options: { verify_cmds: "true" },
    },
  ],
}"#;

const RETRY_MARKER: &str = "retry-plan-marker-7f3a";

/// A disposable project carrying the checked-in `.gremlins` definitions.
fn implement_project(definition: &str) -> crate::test_support::GitSandbox {
    let mut fx = crate::test_support::GitSandbox::with_definition(definition);
    assert!(!fx.is_skipped(), "git sandbox setup must succeed");
    use_checkout_cli(&mut fx);
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.gremlins");
    let overlay = fx.repo_path().join(".gremlins");
    for dir in ["stages", "prompts", "bin"] {
        let copied = std::process::Command::new("cp")
            .arg("-R")
            .arg(source.join(dir))
            .arg(&overlay)
            .status()
            .unwrap();
        assert!(copied.success());
    }
    for (key, value) in [("user.name", "Test"), ("user.email", "test@example.com")] {
        let set = crate::test_support::git(fx.repo_path(), &["config", key, value]);
        assert!(set.status.success());
    }
    fs::write(fx.repo_path().join("plan.txt"), "do the thing\n").unwrap();
    fs::write(fx.repo_path().join("feature.txt"), "zero\n").unwrap();
    crate::test_support::git(fx.repo_path(), &["add", "."]);
    let commit = crate::test_support::git(fx.repo_path(), &["commit", "-qm", "project"]);
    assert!(commit.status.success());
    fx
}

fn find_files(dir: &Path, name: &str, found: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            find_files(&path, name, found);
        } else if path.file_name().is_some_and(|n| n == name) {
            found.push(path);
        }
    }
}

fn judge_verdict_path(request: &CapturedHttpRequest) -> PathBuf {
    let body = String::from_utf8(request.body.to_vec()).unwrap();
    let rest = body.split("Your only writable file is `").nth(1).unwrap();
    PathBuf::from(rest.split('`').next().unwrap())
}

/// Plays the implementer and judge between scripted model responses.
struct ImplementScript {
    http: SequencedHttpClient,
    cwd: PathBuf,
    commits_at_nudge: Arc<std::sync::atomic::AtomicUsize>,
    stop: Arc<crate::clients::CancelToken>,
    /// Live stage transitions, drained into `stages_at_nudge` at the nudge.
    events: std::sync::Mutex<gremlins_magic_bus::ReceiverHalf<gremlins_magic_bus::EventKind>>,
    stages_at_nudge: Arc<std::sync::Mutex<Vec<String>>>,
}

impl ImplementScript {
    fn drain_stages(&self) {
        use gremlins_magic_bus::EventKind;
        let mut events = self.events.lock().unwrap();
        let mut seen = self.stages_at_nudge.lock().unwrap();
        while let Ok(event) = events.try_recv() {
            if let EventKind::StageTransition { stage, sub_stage, .. } = event.kind() {
                seen.push(sub_stage.clone().unwrap_or_else(|| stage.clone()));
            }
        }
    }

    fn commits(&self) -> usize {
        let out = crate::test_support::git(&self.cwd, &["rev-list", "--count", "HEAD"]);
        String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
    }
}

impl HttpMiddleware for ImplementScript {
    fn after_response<'a>(
        &'a self,
        _: Exchange<'a>,
        _: http::StatusCode,
        _: &'a http::HeaderMap,
    ) -> WasmBoxedFuture<'a, HttpResult<()>> {
        Box::pin(async move {
            let requests = self.http.requests();
            match requests.len() {
                // Attempt one edits, then proposes Done.
                1 => fs::write(self.cwd.join("feature.txt"), "one\n").unwrap(),
                2 => fs::write(
                    judge_verdict_path(&requests[1]),
                    format!("RETRY\n{RETRY_MARKER}: finish the rest\n"),
                )
                .unwrap(),
                // Attempt two's second Done: after the nudge, before any edit.
                4 => {
                    self.drain_stages();
                    self.commits_at_nudge
                        .store(self.commits(), std::sync::atomic::Ordering::SeqCst);
                    fs::write(self.cwd.join("feature.txt"), "two\n").unwrap();
                }
                5 => fs::write(judge_verdict_path(&requests[4]), "COMPLETE\n").unwrap(),
                // Approval entered QA; the release gate is satisfied.
                6 => self.stop.cancel(),
                _ => {}
            }
            Ok(())
        })
    }
}

fn init_project_gremlin(
    fx: &crate::test_support::GitSandbox,
    id: &str,
    supervisor: &crate::executor::supervisor::ArtifactTestSupervisor,
) -> crate::executor::gremlin::Gremlin {
    use crate::executor::gremlin::{Gremlin, GremlinConfig};
    let inputs = HashMap::from([(
        "plan".to_string(),
        fx.repo_path().join("plan.txt").display().to_string(),
    )]);
    let mut gremlin = Gremlin::init(
        id,
        fx.definition_path(),
        &crate::definition::StaticDefinition::from_json5_file(fx.definition_path(), None, None)
            .unwrap(),
        &inputs,
        None,
        &GremlinConfig {
            ephemeral: true,
            ..Default::default()
        },
    )
    .unwrap();
    gremlin.runtime_config.registry = Some(supervisor.registry());
    gremlin
}

#[tokio::test]
async fn unchanged_second_attempt_is_nudged_after_judge_retry() {
    let fx = implement_project(IMPLEMENT_DEFINITION);
    let supervisor = crate::executor::supervisor::ArtifactTestSupervisor::start();
    let mut gremlin = init_project_gremlin(&fx, "gr-nudge", &supervisor);
    let http = SequencedHttpClient::new((1..=6).map(|turn| proposed_done("done", turn)));
    let commits_at_nudge = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let stages_at_nudge = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (sub, events) = gremlins_magic_bus::subscribe_run(gremlin.id.as_str());
    let _guard = gremlins_magic_bus::connection::BusSubGuard::new(sub);
    let stop = crate::clients::CancelToken::new();
    gremlin.cancel_token = Some(stop.clone());
    // The workdir is a worktree only once bootstrap has run; the script reads
    // its path lazily from the handle's cwd, which is fixed at init.
    let transport = DynHttpClient::new(http.clone()).with_middleware(ImplementScript {
        http: http.clone(),
        cwd: gremlin.cwd(),
        commits_at_nudge: commits_at_nudge.clone(),
        stop: stop.clone(),
        events: std::sync::Mutex::new(events),
        stages_at_nudge: stages_at_nudge.clone(),
    });
    let (result, _, stages) =
        drive_with_stages(&mut gremlin, "chatgpt/gpt-implement-nudge", transport).await;

    let requests = http.requests();
    assert!(requests.len() >= 5, "run stalled after {} requests: {result:?}", requests.len());
    // Attempt two starts with the judge's feedback and, unedited, is nudged.
    let second_prompt = String::from_utf8(requests[2].body.to_vec()).unwrap();
    assert!(second_prompt.contains(RETRY_MARKER), "{second_prompt}");
    let nudge = result_text(&requests[3], "call_3");
    assert!(nudge.contains("No new changes"), "{nudge}");
    // Init, project and attempt one's commit existed; no judge ran before the nudge.
    assert_eq!(commits_at_nudge.load(std::sync::atomic::Ordering::SeqCst), 3);
    // Stages observed by the nudged request: attempt one's commit stage and
    // judge each ran exactly once, and the second attempt had begun.
    let at_nudge = stages_at_nudge.lock().unwrap().clone();
    for stage in ["git-commit", "judge"] {
        let ran = at_nudge.iter().filter(|s| *s == stage).count();
        assert_eq!(ran, 1, "{stage} through the nudge: {at_nudge:?}");
    }
    let attempts = at_nudge.iter().filter(|s| *s == "implement-2").count();
    assert_eq!(attempts, 2, "{at_nudge:?}");
    assert!(stages.starts_with(&at_nudge), "{at_nudge:?} vs {stages:?}");
    // The edited second Done was accepted and committed before the judge.
    let log = crate::test_support::git(&gremlin.cwd(), &["log", "--format=%s"]);
    let log = String::from_utf8_lossy(&log.stdout).into_owned();
    assert_eq!(log.matches("implement plan").count(), 2, "{log}");

    // The judge's COMPLETE was routed to approval.
    let mut approvals = Vec::new();
    find_files(&gremlin.state.state_dir(), "implementation-approved", &mut approvals);
    assert_eq!(approvals.len(), 1, "{approvals:?}");

    // Distinct baselines; the second is the tree the first commit produced.
    let mut baselines = Vec::new();
    find_files(&gremlin.state.state_dir(), "implement-baseline", &mut baselines);
    let mut trees: Vec<String> = baselines
        .iter()
        .map(|p| fs::read_to_string(p).unwrap().trim().to_owned())
        .collect();
    assert_eq!(trees.len(), 2, "{baselines:?}");
    trees.sort();
    trees.dedup();
    assert_eq!(trees.len(), 2, "baselines must differ");
    let first_commit = crate::test_support::git(&gremlin.cwd(), &["rev-parse", "HEAD~1^{tree}"]);
    let first_commit = String::from_utf8_lossy(&first_commit.stdout).trim().to_owned();
    assert!(trees.contains(&first_commit), "{trees:?} vs {first_commit}");
}

#[tokio::test]
async fn capture_failure_stops_stage_before_the_agent_starts() {
    let fx = implement_project(IMPLEMENT_DEFINITION);
    let helper = fx.repo_path().join(".gremlins/bin/tree_snapshot");
    let ran = fx.sandbox_path().join("capture-ran");
    let script = format!("#!/bin/sh\necho ran > '{}'\necho boom >&2\nexit 1\n", ran.display());
    fs::write(&helper, script).unwrap();
    let commit = crate::test_support::git(fx.repo_path(), &["commit", "-qam", "break"]);
    assert!(commit.status.success());
    let supervisor = crate::executor::supervisor::ArtifactTestSupervisor::start();
    let mut gremlin = init_project_gremlin(&fx, "gr-capture-fail", &supervisor);
    let http = SequencedHttpClient::new((1..=2).map(|turn| proposed_done("done", turn)));
    let transport = DynHttpClient::new(http.clone());
    let (result, _, stages) =
        drive_with_stages(&mut gremlin, "chatgpt/gpt-implement-nudge", transport).await;
    assert!(result.is_err() || result == Ok(1), "{result:?}");
    assert!(ran.exists(), "the failing capture never ran: {stages:?}");
    assert_eq!(
        stages.last().map(String::as_str),
        Some("capture-implement-baseline"),
        "capture must be the last stage entered: {stages:?}"
    );
    assert!(http.requests().is_empty(), "the agent must not start");
}

/// Collects every stage object satisfying `matches` anywhere in a definition.
fn collect_stages<'a>(
    json: &'a serde_json::Value,
    matches: &dyn Fn(&serde_json::Value) -> bool,
    found: &mut Vec<&'a serde_json::Value>,
) {
    match json {
        serde_json::Value::Object(map) => {
            if matches(json) {
                found.push(json);
            }
            map.values().for_each(|v| collect_stages(v, matches, found));
        }
        serde_json::Value::Array(items) => {
            items.iter().for_each(|v| collect_stages(v, matches, found))
        }
        _ => {}
    }
}

/// The baseline artifact URI declared under `interpolation.<side>.baseline`.
fn baseline_key(stage: &serde_json::Value, side: &str) -> String {
    let declared = stage["interpolation"][side]["baseline"].as_str().unwrap();
    declared
        .strip_prefix("content(\"")
        .and_then(|rest| rest.strip_suffix("\")"))
        .unwrap_or(declared)
        .to_owned()
}

#[test]
fn callers_resolve_baseline_producer_and_consumer_to_one_key() {
    let fx = implement_project(IMPLEMENT_DEFINITION);
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.gremlins");
    let overlay = fx.repo_path().join(".gremlins");
    // (definition, namespaces of its implement-and-qa invocations)
    let callers: [(&str, &[&str]); 4] = [
        ("local", &[""]),
        ("gh", &[""]),
        ("drag-optimizer", &["drag/"]),
        ("undrag", &["", "drag/"]),
    ];
    for (name, namespaces) in &callers {
        let path = overlay.join(format!("{name}.json5"));
        fs::copy(source.join(format!("{name}.json5")), &path).unwrap();
        let def = crate::definition::StaticDefinition::from_json5_file(&path, Some("cmd/true"), None)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let json = def.to_expanded_json();
        let (mut captures, mut implements) = (Vec::new(), Vec::new());
        // Stage names are de-duplicated on expansion, so find the consumer by
        // its `done_check` rather than its name.
        collect_stages(
            &json,
            &|stage| stage["name"] == "capture-implement-baseline",
            &mut captures,
        );
        collect_stages(
            &json,
            &|stage| {
                stage["options"]["done_check"]
                    .as_str()
                    .is_some_and(|check| check.contains("tree_snapshot check"))
            },
            &mut implements,
        );
        assert_eq!(captures.len(), namespaces.len(), "{name}");
        assert_eq!(implements.len(), namespaces.len(), "{name}: {json}");

        let mut seen = Vec::new();
        let consumed_keys: Vec<String> =
            implements.iter().map(|s| baseline_key(s, "inputs")).collect();
        for capture in &captures {
            let produced = baseline_key(capture, "outputs");
            let consumed = consumed_keys
                .iter()
                .find(|key| **key == produced)
                .unwrap_or_else(|| panic!("{name}: no consumer for {produced}"));
            assert_eq!(&produced, consumed);
            assert!(produced.contains("{loop_iter}"), "{name}: {produced}");
            seen.push(produced);
        }
        let mut expected: Vec<String> = namespaces
            .iter()
            .map(|ns| format!("artifact://{ns}{{loop_iter}}/implement-baseline"))
            .collect();
        let (mut seen_sorted, mut distinct) = (seen.clone(), seen.clone());
        seen_sorted.sort();
        expected.sort();
        assert_eq!(seen_sorted, expected, "{name}");
        distinct.sort();
        distinct.dedup();
        assert_eq!(distinct.len(), seen.len(), "{name}: keys must be distinct");
    }
}
