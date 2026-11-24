//! Executor dispatch tests against native bus envelopes, not attachment projections.
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gremlins_magic_bus::{
    init_bus, CorrId, DispatchEvent, EventKind, ReceiverHalf, SubscriberRegistration, TryRecvError,
};

use crate::artifacts::uri::Uri;
use crate::builders::agent::AgentBuilder;
use crate::builders::composite::{ParallelBuilder, SequenceBuilder};
use crate::builders::exec::ExecBuilder;
use crate::clients::backend::{Backend, ClientError, RunParams};
use crate::clients::cancel::CancelToken;
use crate::clients::client::{install_test_backend, Client, ModelURI};
use crate::clients::interactive::InteractiveSession;
use crate::clients::protocol::CompletedRun;
use crate::definition::{ExecutorStage, StageSpec, StaticDefinition};
use crate::executor::gremlin::{validate_gremlin_id, Gremlin, RuntimeConfig};
use crate::executor::run::run_stage;
use crate::executor::state::{self, StateData};
use crate::schemas::bootstrap::Bootstrap;
use crate::test_support::Sandbox;
use gremlins_magic_bus::connection::BusSubGuard;

struct Subscription {
    _guard: BusSubGuard,
    rx: ReceiverHalf<EventKind>,
}

impl Subscription {
    fn new(prefix: CorrId) -> Self {
        let (sub, rx) = init_bus()
            .subscribe(SubscriberRegistration::lossless(prefix))
            .unwrap();
        Self {
            _guard: BusSubGuard::new(sub),
            rx,
        }
    }

    fn drain(&mut self) -> Vec<DispatchEvent<EventKind>> {
        let mut events = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(event) => events.push(event),
                Err(TryRecvError::Empty) => return events,
                Err(error) => panic!("unexpected bus receive failure: {error}"),
            }
        }
    }

    fn boundaries(&mut self) -> Vec<DispatchEvent<EventKind>> {
        self.drain().into_iter().filter(is_boundary).collect()
    }
}

fn is_boundary(event: &DispatchEvent<EventKind>) -> bool {
    matches!(event.kind(), EventKind::StageBoundary { .. })
}

fn fixture(stage: StageSpec) -> (Sandbox, Gremlin, ExecutorStage) {
    let sandbox = Sandbox::new();
    let id = format!("gr-invocation-{}", state::token_hex(8));
    let state_dir = crate::config::state_root().join(format!("{id}-0001"));
    std::fs::create_dir_all(state_dir.join("artifacts")).unwrap();
    let data = serde_json::json!({
        "id": id, "attempt": "0001", "stage": "starting", "model": "cmd/true"
    });
    state::write_state(&state_dir, data.as_object().unwrap()).unwrap();
    let definition = StaticDefinition::new(
        "invocations".into(),
        PathBuf::from("test.yaml"),
        ModelURI::parse("cmd/true").unwrap(),
        Bootstrap::default(),
        vec![],
        None,
        vec![],
        serde_json::Value::Null,
        None,
    );
    let node = definition.convert_stage(stage);
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let mut runtime_config = RuntimeConfig::snapshot(None);
    runtime_config.registry = Some(Arc::new(super::supervisor::run_registry::RunRegistry::new(
        shutdown,
    )));
    let gremlin = Gremlin {
        id: validate_gremlin_id(&id).unwrap(),
        definition: Box::new(definition),
        definition_path: None,
        model_override: None,
        workdir: None,
        project_root: sandbox.path().to_path_buf(),
        state: StateData::open(&id).unwrap(),
        env: HashMap::new(),
        client: Client::parse("cmd/true").unwrap(),
        loop_iter: "1".into(),
        stage_inputs: HashMap::new(),
        runtime_config,
        cancel_token: None,
        interactive_sessions: None,
        clean_cmds: vec![],
        enclosing_task_models: None,
        enclosing_model: None,
        discovered_skills: None,
        extra_sandbox_roots: vec![],
        native_definition: None,
    };
    (sandbox, gremlin, node)
}

fn assert_pair(events: &[DispatchEvent<EventKind>], run: &str, name: &str, kind: &str) {
    assert_eq!(
        events.len(),
        2,
        "expected exactly one boundary pair: {events:?}"
    );
    assert_eq!(events[0].corr_id(), events[1].corr_id());
    assert!(events[0].sequence() < events[1].sequence());
    for (event, expected_active) in events.iter().zip([true, false]) {
        let EventKind::StageBoundary {
            id,
            stage,
            stage_run_id,
            stage_type,
            active,
        } = event.kind()
        else {
            panic!("not a boundary: {event:?}");
        };
        assert_eq!(id, run);
        assert_eq!(stage, name);
        assert_eq!(stage_type, kind);
        assert_eq!(*active, expected_active);
        assert!(!stage_run_id.is_empty());
        assert_eq!(event.corr_id().run(), Some(run));
        assert_eq!(event.corr_id().stage(), Some(name));
        assert_eq!(event.corr_id().stage_run_id(), Some(stage_run_id.as_str()));
    }
}

fn assert_exec_envelopes(events: &[DispatchEvent<EventKind>], pair: &[DispatchEvent<EventKind>]) {
    let mut stdout = false;
    let mut stderr = false;
    let mut logs = 0;
    for event in events.iter().filter(|event| {
        matches!(
            event.kind(),
            EventKind::ExecOutput { .. } | EventKind::LogLine { .. }
        )
    }) {
        assert_eq!(event.corr_id(), pair[0].corr_id());
        assert!(pair[0].sequence() < event.sequence());
        assert!(event.sequence() < pair[1].sequence());
        match event.kind() {
            EventKind::ExecOutput {
                is_stderr, text, ..
            } => {
                stdout |= !is_stderr && text.contains("OUT_MARKER");
                stderr |= *is_stderr && text.contains("ERR_MARKER");
            }
            EventKind::LogLine { .. } => logs += 1,
            _ => unreachable!(),
        }
    }
    assert!(
        stdout && stderr,
        "both streams must drain before end: {events:?}"
    );
    assert!(logs >= 2, "preparation and streaming logs must be scoped");
}

#[tokio::test]
async fn exec_streams_and_logs_share_boundaries_on_success_and_failure() {
    for rc in [0, 23] {
        let stage = exec(
            "streams",
            &format!("printf 'OUT_MARKER\\n'; printf 'ERR_MARKER\\n' >&2; exit {rc}"),
        );
        let (_sandbox, mut gremlin, node) = fixture(stage);
        let mut bus = Subscription::new(CorrId::new().with_run(gremlin.id.as_str()));
        let result = run_stage(&node, &mut gremlin).await;
        assert_eq!(result.is_ok(), rc == 0);
        let events = bus.drain();
        let pair: Vec<_> = events.iter().filter(|e| is_boundary(e)).cloned().collect();
        assert_pair(&pair, gremlin.id.as_str(), "streams", "exec");
        assert_exec_envelopes(&events, &pair);
    }
}

#[tokio::test]
async fn exec_subscription_after_start_replays_start_then_streams_and_end() {
    let (_sandbox, mut gremlin, node) = fixture(exec(
        "replay",
        "sleep 0.1; printf 'OUT_MARKER\\n'; printf 'ERR_MARKER\\n' >&2",
    ));
    let id = gremlin.id.to_string();
    let mut bus = Subscription::new(CorrId::new().with_run(&id));
    let observe = async {
        let start = loop {
            let gremlins_magic_bus::Recv::Event(event) = bus.rx.recv().await else {
                panic!("subscription closed");
            };
            if is_boundary(&event) {
                break event;
            }
        };
        let (sub, mut rx, history) = init_bus()
            .subscribe_with_history(SubscriberRegistration::lossless(start.corr_id().clone()))
            .unwrap();
        let _guard = BusSubGuard::new(sub);
        assert!(history
            .events
            .iter()
            .any(|e| e.sequence() == start.sequence()));
        let mut events = history.events;
        while !events
            .iter()
            .any(|event| matches!(event.kind(), EventKind::StageBoundary { active: false, .. }))
        {
            let gremlins_magic_bus::Recv::Event(event) = rx.recv().await else {
                panic!("subscription closed");
            };
            let end = matches!(event.kind(), EventKind::StageBoundary { active: false, .. });
            events.push(event);
            if end {
                break;
            }
        }
        let pair: Vec<_> = events.iter().filter(|e| is_boundary(e)).cloned().collect();
        assert_pair(&pair, &id, "replay", "exec");
        assert_exec_envelopes(&events, &pair);

        // A bounded stream journal retains the last shell line and end, not start.
        let replay =
            gremlins_magic_bus::Bus::with_history_limits_and_retention(2, 2, |kind: &EventKind| {
                matches!(
                    kind,
                    EventKind::ExecOutput { .. } | EventKind::StageBoundary { .. }
                )
            });
        for event in &events {
            replay.emit(event.clone());
        }
        let (sub, _rx, retained) = replay
            .subscribe_with_history(SubscriberRegistration::lossless(
                CorrId::new().with_run(&id),
            ))
            .unwrap();
        assert!(!retained.complete);
        assert_eq!(retained.events.len(), 2);
        assert!(!retained
            .events
            .iter()
            .any(|event| matches!(event.kind(), EventKind::StageBoundary { active: true, .. })));
        let first_line = &retained.events[0];
        assert!(matches!(first_line.kind(), EventKind::ExecOutput { .. }));
        let envelope = first_line.corr_id();
        assert_eq!(envelope.run(), Some(id.as_str()));
        assert_eq!(envelope.stage(), Some("replay"));
        assert!(envelope.stage_run_id().is_some_and(|id| !id.is_empty()));
        assert_eq!(retained.events[1].corr_id(), envelope);
        assert!(matches!(
            retained.events[1].kind(),
            EventKind::StageBoundary { active: false, .. }
        ));
        replay.unsubscribe(&sub);
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        let (result, ()) = tokio::join!(run_stage(&node, &mut gremlin), observe);
        result.unwrap();
    })
    .await
    .unwrap();
}

fn exec(name: &str, command: &str) -> StageSpec {
    ExecBuilder::new(name).cmd(command).build().unwrap()
}

#[tokio::test]
async fn repeated_leaf_has_distinct_stage_run_filters_inside_one_sequence_pair() {
    let stage = SequenceBuilder::new("repeat")
        .max_iterations(2)
        .stage(exec(
            "leaf",
            "printf 'OUT_MARKER\\n'; printf 'ERR_MARKER\\n' >&2",
        ))
        .build()
        .unwrap();
    let (_sandbox, mut gremlin, node) = fixture(stage);
    let prefix = CorrId::new().with_run(gremlin.id.as_str());
    let mut run = Subscription::new(prefix.clone());
    let mut leaf = Subscription::new(prefix.clone().with_stage("leaf"));
    let mut other = Subscription::new(prefix.with_stage("absent"));
    run_stage(&node, &mut gremlin).await.unwrap();
    let all = run.boundaries();
    assert_eq!(all.len(), 6);
    assert_pair(
        &[all[0].clone(), all[5].clone()],
        gremlin.id.as_str(),
        "repeat",
        "sequence",
    );
    let leaf_events = leaf.drain();
    let leaves: Vec<_> = leaf_events
        .iter()
        .filter(|e| is_boundary(e))
        .cloned()
        .collect();
    assert_eq!(leaves.len(), 4);
    assert_pair(&leaves[..2], gremlin.id.as_str(), "leaf", "exec");
    assert_pair(&leaves[2..], gremlin.id.as_str(), "leaf", "exec");
    for pair in leaves.chunks(2) {
        let selected: Vec<_> = leaf_events
            .iter()
            .filter(|e| e.corr_id() == pair[0].corr_id())
            .cloned()
            .collect();
        assert_exec_envelopes(&selected, pair);
    }
    assert_ne!(
        leaves[0].corr_id().stage_run_id(),
        leaves[2].corr_id().stage_run_id()
    );
    let mut ids = HashSet::new();
    for event in [&all[0], &leaves[0], &leaves[2]] {
        assert!(ids.insert(event.corr_id().stage_run_id().unwrap()));
    }
    assert!(other.drain().is_empty());

    // Subscribe to one exact stage run and dispatch the same leaf again: it must not leak.
    let mut exact = Subscription::new(leaves[0].corr_id().clone());
    run_stage(&node, &mut gremlin).await.unwrap();
    assert!(exact.drain().is_empty());
    // Native bus history uses the same stage run prefix for both halves.
    let (sub, _, history) = init_bus()
        .subscribe_with_history(SubscriberRegistration::lossless(
            leaves[0].corr_id().clone(),
        ))
        .unwrap();
    let _guard = BusSubGuard::new(sub);
    let selected: Vec<_> = history
        .events
        .iter()
        .filter(|e| is_boundary(e))
        .cloned()
        .collect();
    assert_pair(&selected, gremlin.id.as_str(), "leaf", "exec");
    assert_exec_envelopes(&history.events, &selected);
}

#[tokio::test]
async fn nested_leaf_transition_keeps_display_payload_but_uses_leaf_invocation_route() {
    let stage = SequenceBuilder::new("outer")
        .stage(
            SequenceBuilder::new("inner")
                .stage(exec("leaf", "true"))
                .build()
                .unwrap(),
        )
        .build()
        .unwrap();
    let (_sandbox, mut gremlin, node) = fixture(stage);
    let prefix = CorrId::new().with_run(gremlin.id.as_str());
    let mut all = Subscription::new(prefix.clone());
    let mut leaf = Subscription::new(prefix.clone().with_stage("leaf"));
    let mut outer = Subscription::new(prefix.with_stage("outer"));
    run_stage(&node, &mut gremlin).await.unwrap();
    let events = all.drain();
    let transitions: Vec<_> = events
        .iter()
        .filter(|event| matches!(event.kind(), EventKind::StageTransition { .. }))
        .collect();
    assert_eq!(transitions.len(), 1);
    let transition = transitions[0];
    let EventKind::StageTransition {
        id,
        stage,
        sub_stage,
    } = transition.kind()
    else {
        unreachable!();
    };
    assert_eq!(id, gremlin.id.as_str());
    assert_eq!(stage, "outer");
    assert_eq!(sub_stage.as_deref(), Some("leaf"));
    let selected = leaf.drain();
    let pair: Vec<_> = selected
        .iter()
        .filter(|e| is_boundary(e))
        .cloned()
        .collect();
    assert_pair(&pair, gremlin.id.as_str(), "leaf", "exec");
    assert_eq!(transition.corr_id(), pair[0].corr_id());
    assert!(pair[0].sequence() < transition.sequence());
    assert!(transition.sequence() < pair[1].sequence());
    assert_eq!(
        selected
            .iter()
            .filter(|event| { matches!(event.kind(), EventKind::StageTransition { .. }) })
            .count(),
        1
    );
    assert!(!outer
        .drain()
        .iter()
        .any(|event| { matches!(event.kind(), EventKind::StageTransition { .. }) }));

    // Exact invocation filtering must include the transition in history and exclude reruns.
    let (sub, _, history) = init_bus()
        .subscribe_with_history(SubscriberRegistration::lossless(
            transition.corr_id().clone(),
        ))
        .unwrap();
    let _guard = BusSubGuard::new(sub);
    let history_transitions: Vec<_> = history
        .events
        .iter()
        .filter(|event| matches!(event.kind(), EventKind::StageTransition { .. }))
        .collect();
    assert_eq!(history_transitions.len(), 1);
    assert_eq!(history_transitions[0].sequence(), transition.sequence());
    let mut exact = Subscription::new(transition.corr_id().clone());
    run_stage(&node, &mut gremlin).await.unwrap();
    assert!(exact.drain().is_empty());
}

#[tokio::test]
async fn skipped_sequence_closes_itself_without_dispatching_its_leaf() {
    let stage = SequenceBuilder::new("skip")
        .skip_if_exists("artifact://ready")
        .stage(exec("must-not-run", "exit 19"))
        .build()
        .unwrap();
    let (_sandbox, mut gremlin, node) = fixture(stage);
    gremlin
        .state
        .write_into_registry(&Uri::parse("artifact://ready").unwrap(), "ready")
        .await
        .unwrap();
    let mut bus = Subscription::new(CorrId::new().with_run(gremlin.id.as_str()));
    run_stage(&node, &mut gremlin).await.unwrap();
    assert_pair(&bus.boundaries(), gremlin.id.as_str(), "skip", "sequence");
}

#[tokio::test]
async fn running_sequence_skips_guarded_leaf_but_dispatches_its_sibling() {
    // Leaves have no skip guard; the smallest supported guard is a sequence.
    let guarded = SequenceBuilder::new("guarded")
        .skip_if_exists("artifact://ready")
        .stage(exec("skipped", "exit 19"))
        .build()
        .unwrap();
    let stage = SequenceBuilder::new("outer")
        .stage(guarded)
        .stage(exec("sibling", "true"))
        .build()
        .unwrap();
    let (_sandbox, mut gremlin, node) = fixture(stage);
    gremlin
        .state
        .write_into_registry(&Uri::parse("artifact://ready").unwrap(), "ready")
        .await
        .unwrap();
    let prefix = CorrId::new().with_run(gremlin.id.as_str());
    let mut bus = Subscription::new(prefix.clone());
    let mut skipped = Subscription::new(prefix.clone().with_stage("skipped"));
    let mut sibling = Subscription::new(prefix.with_stage("sibling"));

    run_stage(&node, &mut gremlin).await.unwrap();

    let events = bus.boundaries();
    assert_eq!(events.len(), 6);
    assert_pair(
        &[events[0].clone(), events[5].clone()],
        gremlin.id.as_str(),
        "outer",
        "sequence",
    );
    assert_pair(&events[1..3], gremlin.id.as_str(), "guarded", "sequence");
    assert_pair(&events[3..5], gremlin.id.as_str(), "sibling", "exec");
    assert!(events
        .windows(2)
        .all(|pair| pair[0].sequence() < pair[1].sequence()));
    assert!(skipped.boundaries().is_empty());
    assert_pair(
        &sibling.boundaries(),
        gremlin.id.as_str(),
        "sibling",
        "exec",
    );
}

#[tokio::test]
async fn exec_and_preparation_failures_close_leaf_and_enclosing_sequence() {
    for (child, kind) in [
        (exec("bad", "exit 23"), "exec"),
        (
            ExecBuilder::new("bad")
                .cmd("cat {missing}")
                .interpolate("missing", "artifact://missing")
                .build()
                .unwrap(),
            "exec",
        ),
        (
            AgentBuilder::new("bad")
                .prompt("read {missing}")
                .interpolate("missing", "artifact://missing")
                .build()
                .unwrap(),
            "agent",
        ),
    ] {
        let stage = SequenceBuilder::new("outer").stage(child).build().unwrap();
        let (_sandbox, mut gremlin, node) = fixture(stage);
        let mut bus = Subscription::new(CorrId::new().with_run(gremlin.id.as_str()));
        assert!(run_stage(&node, &mut gremlin).await.is_err());
        let events = bus.boundaries();
        assert_eq!(events.len(), 4);
        assert_pair(
            &[events[0].clone(), events[3].clone()],
            gremlin.id.as_str(),
            "outer",
            "sequence",
        );
        assert_pair(&events[1..3], gremlin.id.as_str(), "bad", kind);
    }
}

#[derive(Clone, Default)]
struct RetryTransport {
    requests: Arc<std::sync::Mutex<Vec<rig_core::completion::CompletionRequest>>>,
    corr: CorrId,
}

impl rig_core::driver::Transport<rig_core::test_utils::MockScript> for RetryTransport {
    fn send(
        &self,
        request: rig_core::completion::CompletionRequest,
        _: rig_core::driver::Exchange,
    ) -> rig_core::driver::Opening<rig_core::test_utils::MockFrame> {
        use futures::StreamExt;
        use rig_core::test_utils::{MockFrame, MockStreamEvent as Event};

        let mut requests = self.requests.lock().unwrap();
        requests.push(request);
        let attempt = requests.len();
        gremlins_magic_bus::emit_corr_event(
            &self.corr,
            EventKind::LogLine {
                id: self.corr.run().unwrap().to_owned(),
                line: format!("model request #{attempt}"),
            },
        );
        let events = match attempt {
            1 => vec![Event::error("http://429: transient model failure")],
            2 => vec![
                Event::tool_call(
                    "finish",
                    "Done",
                    serde_json::json!({"summary": "recovered"}),
                ),
                Event::final_response_with_default_usage(),
            ],
            n => panic!("unexpected model request #{n}"),
        };
        rig_core::driver::Opening::ready(rig_core::driver::Opened::new(
            futures::stream::iter(events).map(MockFrame::Event).map(Ok),
        ))
    }
}

#[derive(Default)]
struct RetryingAgentBackend {
    runs: std::sync::atomic::AtomicUsize,
    requests: Arc<std::sync::Mutex<Vec<rig_core::completion::CompletionRequest>>>,
}

#[async_trait::async_trait]
impl Backend for RetryingAgentBackend {
    async fn run(
        &self,
        params: RunParams,
        interactive: InteractiveSession,
    ) -> Result<CompletedRun, ClientError> {
        use crate::clients::agent::agent_loop::{loop_opts, run_agent_loop};
        use crate::clients::agent::run_context::RunContext;

        self.runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let transport = RetryTransport {
            requests: self.requests.clone(),
            corr: params.output_corr_id.clone().unwrap(),
        };
        assert!(transport.corr.stage_run_id().is_some());
        let model =
            rig_core::driver::Model::new(rig_core::test_utils::MockScript::default(), transport);
        let prompt = params.prompt.clone();
        let cancel = params.cancel_token.clone().unwrap_or_else(CancelToken::new);
        let result = run_agent_loop(
            model.erase(),
            &prompt,
            RunContext::from_params(params),
            cancel,
            loop_opts(None),
            interactive,
        )
        .await?;
        assert_eq!(result.token_usage.as_ref().unwrap().turns, 1);
        Ok(result)
    }

    fn model(&self) -> &str {
        "invocation-retry-test"
    }
}

#[tokio::test(start_paused = true)]
async fn model_request_retry_stays_inside_one_agent_invocation() {
    let spec = format!("cmd/invocation-retry-{}", state::token_hex(8));
    let backend = Arc::new(RetryingAgentBackend::default());
    install_test_backend(&spec, backend.clone());
    let stage = AgentBuilder::new("agent")
        .model(ModelURI::parse(&spec).unwrap())
        .prompt("finish")
        .build()
        .unwrap();
    let (_sandbox, mut gremlin, node) = fixture(stage);
    let (sessions, _) = crate::clients::interactive::InteractiveSessionFactory::for_pipeline();
    gremlin.interactive_sessions = Some(sessions);
    let mut bus = Subscription::new(CorrId::new().with_run(gremlin.id.as_str()));

    run_stage(&node, &mut gremlin).await.unwrap();

    assert_eq!(backend.runs.load(std::sync::atomic::Ordering::SeqCst), 1);
    let requests = backend.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        serde_json::to_value(&requests[0]).unwrap(),
        serde_json::to_value(&requests[1]).unwrap(),
        "retry must resend the same turn, not advance the conversation"
    );
    let events = bus.drain();
    let boundaries: Vec<_> = events.iter().filter(|e| is_boundary(e)).cloned().collect();
    assert_pair(&boundaries, gremlin.id.as_str(), "agent", "agent");
    let model_calls: Vec<_> = events
        .iter()
        .filter(|e| matches!(e.kind(), EventKind::LogLine { line, .. } if line.starts_with("model request #")))
        .collect();
    let prompts: Vec<_> = events
        .iter()
        .filter(|e| matches!(e.kind(), EventKind::PromptRequest { .. }))
        .collect();
    assert_eq!(model_calls.len(), 2);
    assert_eq!(prompts.len(), 2);
    assert_eq!(model_calls[0].corr_id(), model_calls[1].corr_id());
    for (prompt, call) in prompts.iter().zip(&model_calls) {
        assert_eq!(prompt.corr_id(), call.corr_id());
        assert_eq!(call.corr_id().run(), boundaries[0].corr_id().run());
        assert_eq!(
            call.corr_id().stage_run_id(),
            boundaries[0].corr_id().stage_run_id()
        );
        assert!(boundaries[0].sequence() < prompt.sequence());
        assert!(prompt.sequence() < call.sequence());
        assert!(call.sequence() < boundaries[1].sequence());
    }
    assert!(model_calls[0].sequence() < prompts[1].sequence());
}

#[derive(Default)]
struct FailingBackend {
    wait_for_cancel: bool,
    readonly: bool,
}

#[async_trait::async_trait]
impl Backend for FailingBackend {
    async fn run(
        &self,
        params: RunParams,
        interactive: InteractiveSession,
    ) -> Result<CompletedRun, ClientError> {
        assert_eq!(interactive.accepts_commands(), !self.readonly);
        assert_eq!(interactive.snapshot().accepts_commands(), !self.readonly);
        let corr = params.output_corr_id.as_ref().unwrap();
        gremlins_magic_bus::emit_corr_event(
            corr,
            EventKind::LogLine {
                id: params.gremlin_id.clone().unwrap(),
                line: "backend entered".into(),
            },
        );
        if self.wait_for_cancel {
            params.cancel_token.as_ref().unwrap().cancelled().await;
        }
        Err(ClientError::Runtime {
            message: "scripted backend failure".into(),
        })
    }
    fn model(&self) -> &str {
        "invocation-tests"
    }
    fn accepts_interactive_commands(&self) -> bool {
        !self.readonly
    }
}

async fn backend_case(cancel: bool, abort: bool, readonly: bool) {
    let spec = format!("cmd/invocation-{}", state::token_hex(8));
    install_test_backend(
        &spec,
        Arc::new(FailingBackend {
            wait_for_cancel: cancel || abort,
            readonly,
        }),
    );
    let stage = SequenceBuilder::new("outer")
        .stage(
            AgentBuilder::new("agent")
                .model(ModelURI::parse(&spec).unwrap())
                .prompt("hi")
                .build()
                .unwrap(),
        )
        .build()
        .unwrap();
    let (_sandbox, mut gremlin, node) = fixture(stage);
    let token = CancelToken::new();
    gremlin.cancel_token = Some(token.clone());
    let (sessions, interactive) =
        crate::clients::interactive::InteractiveSessionFactory::for_pipeline();
    gremlin.interactive_sessions = Some(sessions);
    let id = gremlin.id.to_string();
    let mut bus = Subscription::new(CorrId::new().with_run(&id));
    let mut task = tokio::spawn(async move { run_stage(&node, &mut gremlin).await });
    let mut events = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let gremlins_magic_bus::Recv::Event(event) = bus.rx.recv().await else { panic!("bus closed") };
            let entered = matches!(event.kind(), EventKind::LogLine { line, .. } if line == "backend entered");
            events.push(event);
            if entered { break; }
        }
    }).await.expect("backend was not dispatched");
    if cancel || abort {
        let target = interactive.target();
        assert_eq!(target.accepts_commands(), !readonly);
        assert_eq!(
            target.stage_run_id(),
            events.last().unwrap().corr_id().stage_run_id()
        );
        if readonly {
            assert!(target.attach().is_err());
        }
    }
    if abort {
        task.abort();
    } else if cancel {
        token.cancel();
    }
    let result = tokio::time::timeout(Duration::from_secs(10), &mut task)
        .await
        .unwrap();
    if abort {
        assert!(result.unwrap_err().is_cancelled());
    } else {
        assert!(result
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("scripted backend failure"));
    }
    events.extend(bus.drain());
    let marker = events
        .iter()
        .find(|e| matches!(e.kind(), EventKind::LogLine { line, .. } if line == "backend entered"))
        .unwrap();
    let boundaries: Vec<_> = events.iter().filter(|e| is_boundary(e)).cloned().collect();
    assert_eq!(boundaries.len(), 4);
    assert_pair(
        &[boundaries[0].clone(), boundaries[3].clone()],
        &id,
        "outer",
        "sequence",
    );
    assert_pair(&boundaries[1..3], &id, "agent", "agent");
    assert_eq!(
        marker.corr_id(),
        boundaries[1].corr_id(),
        "pipeline session must use stage run identity"
    );
    assert!(boundaries[1].sequence() < marker.sequence());
    assert!(marker.sequence() < boundaries[2].sequence());
}

#[tokio::test]
async fn backend_failure_closes_both_pairs_and_shares_session_identity() {
    backend_case(false, false, false).await;
}
#[tokio::test]
async fn cancellation_closes_both_pairs() {
    backend_case(true, false, false).await;
}
#[tokio::test]
async fn task_abort_drops_nested_invocations_and_closes_both_pairs() {
    backend_case(false, true, false).await;
}

#[tokio::test]
async fn readonly_backend_receives_the_stage_run_target_identity() {
    backend_case(true, false, true).await;
}

struct ChatBackend {
    wait_for_cancel: bool,
    fail: bool,
}

#[async_trait::async_trait]
impl Backend for ChatBackend {
    async fn run(
        &self,
        params: RunParams,
        interactive: InteractiveSession,
    ) -> Result<CompletedRun, ClientError> {
        assert!(interactive.accepts_commands());
        // Capture input identity independently; output must use the supplied invocation route.
        let session = interactive.snapshot();
        gremlins_magic_bus::emit_corr_event(
            params.output_corr_id.as_ref().unwrap(),
            EventKind::LogLine {
                id: params.gremlin_id.clone().unwrap(),
                line: format!("chat backend session: {}", session.stage_run_id().unwrap()),
            },
        );
        gremlins_magic_bus::emit_corr_event(
            params.output_corr_id.as_ref().unwrap(),
            EventKind::StreamChunk {
                text: "CHAT_MARKER".into(),
            },
        );
        if self.wait_for_cancel {
            params.cancel_token.as_ref().unwrap().cancelled().await;
        }
        if self.fail {
            return Err(ClientError::Runtime {
                message: "scripted chat failure".into(),
            });
        }
        Ok(CompletedRun {
            exit_code: 0,
            text_result: Some("chat output".into()),
            summary: None,
            events: None,
            cost_usd: None,
            token_usage: None,
            conversation: None,
        })
    }

    fn model(&self) -> &str {
        "invocation-chat-test"
    }

    fn accepts_interactive_commands(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn chat_output_uses_invocation_identity_without_replacing_prepared_input_target() {
    // Repeat the same turn name, including on failure, to catch identity reuse.
    for mode in ["success", "failure", "cancel", "abort"] {
        let spec = format!("cmd/chat-invocation-{}", state::token_hex(8));
        install_test_backend(
            &spec,
            Arc::new(ChatBackend {
                wait_for_cancel: matches!(mode, "cancel" | "abort"),
                fail: mode != "success",
            }),
        );
        let stage = AgentBuilder::new("chat-0001")
            .model(ModelURI::parse(&spec).unwrap())
            .prompt("hi")
            .build()
            .unwrap();
        let (_sandbox, mut gremlin, node) = fixture(stage);
        gremlin.state.patch(
            &[],
            &[("kind".into(), serde_json::json!("chat"))]
                .into_iter()
                .collect(),
        );
        let (sessions, handle) = crate::clients::interactive::InteractiveSessionFactory::for_chat();
        gremlin.interactive_sessions = Some(sessions);
        let id = gremlin.id.to_string();
        let mut bus = Subscription::new(CorrId::new().with_run(&id));
        let node = Arc::new(node);
        let mut invocation_ids = HashSet::new();
        let mut session_ids = HashSet::new();
        for _ in 0..if mode == "abort" { 1 } else { 2 } {
            let token = CancelToken::new();
            gremlin.cancel_token = Some(token.clone());
            let target = gremlin
                .prepare_interactive_target_with_commands(true)
                .unwrap();
            let session_id = target.stage_run_id().unwrap().to_owned();
            assert!(session_ids.insert(session_id.clone()));
            let execution_end = target.end_signal();
            let stage = node.clone();
            let mut task = tokio::spawn(async move {
                let result = run_stage(&stage, &mut gremlin).await;
                (gremlin, result)
            });
            let mut events = Vec::new();
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let gremlins_magic_bus::Recv::Event(event) = bus.rx.recv().await else {
                        panic!("bus closed");
                    };
                    let entered = matches!(event.kind(), EventKind::LogLine { line, .. }
                        if line.starts_with("chat backend session: "));
                    events.push(event);
                    if entered {
                        break;
                    }
                }
            })
            .await
            .expect("chat backend was not dispatched");
            let marker = events.last().unwrap().clone();
            assert!(matches!(marker.kind(), EventKind::LogLine { line, .. }
                if line == &format!("chat backend session: {session_id}")));
            assert_eq!(handle.target().stage_run_id(), Some(session_id.as_str()));
            assert_eq!(handle.target().nonce(), target.nonce());
            assert_ne!(marker.corr_id().stage_run_id(), Some(session_id.as_str()));
            if mode == "abort" {
                task.abort();
            } else if mode == "cancel" {
                token.cancel();
            }
            let joined = tokio::time::timeout(Duration::from_secs(10), &mut task)
                .await
                .unwrap();
            // Recover ownership for another invocation, except when deliberately aborted.
            let recovered = if mode == "abort" {
                match joined {
                    Err(error) => assert!(error.is_cancelled()),
                    Ok(_) => panic!("aborted chat task completed"),
                }
                None
            } else {
                let (g, result) = joined.unwrap();
                assert_eq!(result.is_ok(), mode == "success", "{result:?}");
                Some(g)
            };
            events.extend(bus.drain());
            let pair: Vec<_> = events.iter().filter(|e| is_boundary(e)).cloned().collect();
            assert_pair(&pair, &id, "chat-0001", "agent");
            assert!(invocation_ids.insert(pair[0].corr_id().stage_run_id().unwrap().to_owned()));
            assert_eq!(marker.corr_id(), pair[0].corr_id());
            let chunks: Vec<_> = events.iter().filter(|event| {
                matches!(event.kind(), EventKind::StreamChunk { text } if text == "CHAT_MARKER")
            }).collect();
            assert_eq!(chunks.len(), 1);
            assert_eq!(chunks[0].corr_id(), pair[0].corr_id());
            assert!(pair[0].sequence() < chunks[0].sequence());
            assert!(chunks[0].sequence() < pair[1].sequence());
            assert!(pair[0].sequence() < marker.sequence());
            assert!(marker.sequence() < pair[1].sequence());
            assert!(execution_end.is_ended());
            assert_eq!(target.stage_run_id(), Some(session_id.as_str()));
            if let Some(g) = recovered {
                gremlin = g;
            } else {
                break;
            }
        }
    }
}

#[tokio::test]
async fn backend_setup_failure_closes_both_pairs_without_publishing_a_session() {
    // A valid model URI whose command cannot be parsed fails during backend construction.
    let stage = SequenceBuilder::new("outer")
        .stage(
            AgentBuilder::new("bad")
                .model(ModelURI::parse("cmd/\"").unwrap())
                .prompt("hi")
                .build()
                .unwrap(),
        )
        .build()
        .unwrap();
    let (_sandbox, mut gremlin, node) = fixture(stage);
    let (sessions, interactive) =
        crate::clients::interactive::InteractiveSessionFactory::for_pipeline();
    let before = interactive.target();
    assert_eq!(before.stage_run_id(), None);
    gremlin.interactive_sessions = Some(sessions);
    let mut bus = Subscription::new(CorrId::new().with_run(gremlin.id.as_str()));
    let error = run_stage(&node, &mut gremlin).await.unwrap_err();
    assert!(
        error.to_string().contains("failed to parse command"),
        "{error}"
    );
    let after = interactive.target();
    assert_eq!(after.stage_run_id(), None);
    assert_eq!(after.nonce(), before.nonce());
    assert!(!after.accepts_commands());
    let events = bus.boundaries();
    assert_eq!(events.len(), 4);
    assert_pair(
        &[events[0].clone(), events[3].clone()],
        gremlin.id.as_str(),
        "outer",
        "sequence",
    );
    assert_pair(&events[1..3], gremlin.id.as_str(), "bad", "agent");
}

#[tokio::test]
async fn missing_output_bail_closes_leaf_and_enclosing_sequence() {
    let stage = SequenceBuilder::new("outer")
        .stage(
            AgentBuilder::new("writer")
                .model(ModelURI::parse("cmd/true").unwrap())
                .output(
                    "out",
                    crate::builders::artifacts::output("artifact://out.md"),
                )
                .prompt("write {out}")
                .build()
                .unwrap(),
        )
        .build()
        .unwrap();
    let (_sandbox, mut gremlin, node) = fixture(stage);
    let mut bus = Subscription::new(CorrId::new().with_run(gremlin.id.as_str()));
    assert!(matches!(
        run_stage(&node, &mut gremlin).await,
        Err(super::RunError::Bail { .. })
    ));
    let events = bus.boundaries();
    assert_eq!(events.len(), 4);
    assert_pair(
        &[events[0].clone(), events[3].clone()],
        gremlin.id.as_str(),
        "outer",
        "sequence",
    );
    assert_pair(&events[1..3], gremlin.id.as_str(), "writer", "agent");
}

#[tokio::test]
async fn parallel_children_have_own_run_identities_visible_to_parent_subscription() {
    let stage = ParallelBuilder::new("fanout")
        .stage(exec(
            "left",
            "printf 'LEFT_OUT_MARKER\\n'; printf 'LEFT_ERR_MARKER\\n' >&2",
        ))
        .stage(exec(
            "right",
            "printf 'RIGHT_OUT_MARKER\\n'; printf 'RIGHT_ERR_MARKER\\n' >&2",
        ))
        .build()
        .unwrap();
    let (_sandbox, mut gremlin, node) = fixture(stage);
    let id = gremlin.id.to_string();
    let mut bus = Subscription::new(CorrId::new().with_run(&id));
    tokio::time::timeout(Duration::from_secs(20), run_stage(&node, &mut gremlin))
        .await
        .unwrap()
        .unwrap();
    let all = bus.drain();
    let terminals: Vec<_> = all
        .iter()
        .filter(|event| matches!(event.kind(), EventKind::RunCompleted { .. }))
        .cloned()
        .collect();
    assert_eq!(
        terminals.len(),
        2,
        "child terminals must reach the parent run filter"
    );
    for event in &terminals {
        let EventKind::RunCompleted {
            id: child_id,
            exit_code,
        } = event.kind()
        else {
            unreachable!();
        };
        assert_eq!(*exit_code, 0);
        assert_ne!(child_id, &id);
        assert_eq!(event.corr_id(), &CorrId::new().with_run(child_id));
    }
    let events: Vec<_> = all.iter().filter(|e| is_boundary(e)).cloned().collect();
    assert_eq!(
        events.len(),
        6,
        "parent subscription must include both child pairs"
    );
    let parent: Vec<_> = events
        .iter()
        .filter(|e| e.corr_id().run() == Some(id.as_str()))
        .cloned()
        .collect();
    assert_pair(&parent, &id, "fanout", "parallel");
    let mut child_runs = HashSet::new();
    for name in ["left", "right"] {
        let child: Vec<_> = events
            .iter()
            .filter(|e| e.corr_id().stage() == Some(name))
            .cloned()
            .collect();
        let run = child[0].corr_id().run().unwrap();
        assert_ne!(run, id);
        assert!(child_runs.insert(run.to_owned()));
        let terminal = terminals
            .iter()
            .find(|event| event.corr_id().run() == Some(run))
            .unwrap();
        assert!(child[1].sequence() < terminal.sequence());
        assert!(CorrId::new()
            .with_run(run)
            .matches_prefix(&CorrId::new().with_run(&id)));
        assert_pair(&child, run, name, "exec");
        let output: Vec<_> = all
            .iter()
            .filter(|e| e.corr_id() == child[0].corr_id())
            .cloned()
            .collect();
        assert_exec_envelopes(&output, &child);
        let marker = name.to_uppercase();
        for (is_stderr, suffix) in [(false, "OUT_MARKER"), (true, "ERR_MARKER")] {
            let expected = format!("{marker}_{suffix}");
            let lines: Vec<_> = all
                .iter()
                .filter(|event| {
                    matches!(
                        event.kind(),
                        EventKind::ExecOutput { text, .. } if text.trim() == expected
                    )
                })
                .collect();
            assert_eq!(lines.len(), 1, "missing or duplicated {expected}: {all:?}");
            let line = lines[0];
            assert_eq!(line.corr_id(), child[0].corr_id());
            assert!(child[0].sequence() < line.sequence());
            assert!(line.sequence() < child[1].sequence());
            assert!(matches!(
                line.kind(),
                EventKind::ExecOutput { is_stderr: actual, .. } if *actual == is_stderr
            ));
        }
        assert!(parent[0].sequence() < child[0].sequence());
        assert!(child[1].sequence() < parent[1].sequence());
    }
}
