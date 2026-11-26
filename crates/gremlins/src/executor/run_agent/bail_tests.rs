use super::tests::{chat_node, test_gremlin};
use super::*;
use crate::builders::agent::AgentBuilder;
use crate::clients::agent::agent_loop::{loop_opts, run_agent_loop, test_ctx};
use crate::clients::backend::Backend;
use crate::clients::client::install_test_backend;
use crate::clients::execution_input::{Delivery, InputControl, InputRegistry};
use crate::clients::interactive::{InteractiveHandle, InteractiveSessionFactory};
use crate::clients::protocol::{CompletedRun, UsageStats};
use crate::clients::InteractiveSession;
use crate::executor::stage_boundary::StageInvocation;
use std::sync::Arc;
use tokio::sync::Notify;

struct BailBackend {
    handle: InteractiveHandle,
    inputs: InputRegistry,
    inbox: bool,
    fail_accounting: bool,
    bailed: Notify,
    release: Notify,
}

#[async_trait::async_trait]
impl Backend for BailBackend {
    async fn run(
        &self,
        params: RunParams,
        interactive: InteractiveSession,
    ) -> Result<CompletedRun, ClientError> {
        use gremlins_magic_bus::ModelCallUsage;
        use rig_core::test_utils::{MockCompletionModel, MockStreamEvent as E};

        assert!(params.defer_done);
        let corr = params.output_corr_id.as_ref().unwrap();
        if self.fail_accounting {
            // Start succeeded; make the subsequent durable write fail, not setup.
            let journal = config::state_root().join("gr-test-0001/native_accounting.json");
            if journal.exists() {
                std::fs::remove_file(&journal).unwrap();
            }
            std::fs::create_dir(journal).unwrap();
        }
        emit_corr_event(
            corr,
            EventKind::ModelCallUsage {
                usage: ModelCallUsage {
                    request_id: format!("{}-partial", corr.stage_run_id().unwrap()),
                    provider: "anthropic".into(),
                    model: "bail-regression".into(),
                    agent_id: String::new(),
                    request_kind: String::new(),
                    reported: true,
                    completed: false,
                    raw: serde_json::json!({}),
                    usage: UsageStats {
                        turns: 0,
                        prompt_tokens: Some(17),
                        completion_tokens: Some(3),
                        cached_input_tokens: None,
                        cache_creation_input_tokens: None,
                        reasoning_tokens: None,
                    },
                },
            },
        );
        // Both operator routing paths use the exact execution inbox, not an
        // attachment command lease. Acceptance alone must not settle delivery.
        let (_, sender) = if self.inbox {
            self.inputs.resolve(
                corr.run().unwrap(),
                corr.stage_run_id().unwrap(),
                corr.stage(),
            )
        } else {
            self.handle.resolve_input(
                corr.run().unwrap(),
                corr.stage_run_id().unwrap(),
                corr.stage(),
            )
        }
        .expect("live execution input");
        let mut receipt = sender
            .try_control("operator-bail".into(), InputControl::Bail)
            .unwrap();
        assert!(matches!(
            receipt.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        let prompt = params.prompt.clone();
        let mut ctx = test_ctx(params.cwd.clone(), None);
        ctx.prepopulated_conversation = params.prepopulated_conversation.clone();
        ctx.params = params;
        // If the queued bail is lost this model completes instead, failing the assertion.
        let model = MockCompletionModel::from_stream_turns([vec![E::tool_call(
            "done",
            "Done",
            serde_json::json!({"summary": "unexpected completion"}),
        )]]);
        let result = run_agent_loop(
            model.erase(),
            &prompt,
            ctx,
            crate::clients::CancelToken::new(),
            loop_opts(None),
            interactive,
        )
        .await;
        assert!(
            matches!(&result, Err(ClientError::Bail { reason }) if reason.starts_with("operator bailed"))
        );
        assert_eq!(receipt.await.unwrap(), Delivery::Delivered);
        self.bailed.notify_one();
        self.release.notified().await;
        result
    }

    fn model(&self) -> &str {
        "bail-regression"
    }
}

async fn assert_bail_boundary(inbox: bool, fail_accounting: bool, chat: bool) {
    let stage = AgentBuilder::new("chat-0001")
        .prompt("bail regression")
        .build()
        .unwrap();
    let (_sandbox, mut gremlin) = test_gremlin(stage);
    if chat {
        gremlin.state.patch(
            &[],
            serde_json::json!({"kind": "chat"}).as_object().unwrap(),
        );
    }
    let (sessions, handle) = if chat {
        InteractiveSessionFactory::for_chat()
    } else {
        InteractiveSessionFactory::for_pipeline()
    };
    handle.set_input_registry(gremlin.execution_inputs.clone());
    gremlin.interactive_sessions = Some(sessions);
    let backend = Arc::new(BailBackend {
        handle,
        inputs: gremlin.execution_inputs.clone(),
        inbox,
        fail_accounting,
        bailed: Notify::new(),
        release: Notify::new(),
    });
    let spec = "anthropic/operator-bail-boundary-regression";
    install_test_backend(&ModelURI::parse(spec).unwrap().to_string(), backend.clone());
    gremlin.model_override = Some(spec.into());
    let mut node = chat_node(1, "bail regression").await;
    let ExecutorStage::Agent { model, .. } = &mut node else {
        unreachable!()
    };
    *model = Some(ModelURI::parse(spec).unwrap());
    let invocation = StageInvocation::enter(gremlin.id.as_str(), node.name(), "agent");
    let (_guard, mut rx) = gremlins_magic_bus::subscribe_run(gremlin.id.as_str());
    let observe = async {
        backend.bailed.notified().await;
        // The real interactive function has returned Bail, but the executor
        // has not yet crossed its persistence/error boundary.
        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(
                    event.kind(),
                    EventKind::Ended { .. } | EventKind::Done { .. }
                ),
                "interactive bail published a premature terminal"
            );
        }
        backend.release.notify_one();
        if !chat && !fail_accounting {
            loop {
                let gremlins_magic_bus::Recv::Event(event) = rx.recv().await else {
                    panic!("terminal subscription closed or lagged");
                };
                if let EventKind::Ended { reason } = event.kind() {
                    assert_eq!(reason, "bailed");
                    assert_eq!(event.corr_id(), &invocation.corr_id());
                    let rows = std::fs::read_to_string(
                        config::state_root().join("gr-test-0001/token_usage.jsonl"),
                    )
                    .expect("charge must be durable when terminal is received");
                    let row: serde_json::Value =
                        serde_json::from_str(rows.lines().next().unwrap()).unwrap();
                    assert_eq!(row["prompt_tokens"], 17);
                    assert_eq!(row["completion_tokens"], 3);
                    break;
                }
                assert!(!matches!(event.kind(), EventKind::Done { .. }));
            }
        }
    };
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(
            run_agent(&node, &mut gremlin, &invocation, "", "", None, None),
            observe
        )
    })
    .await
    .expect("bail boundary stalled");
    let error = result
        .expect_err("operator bail must fail the stage")
        .to_string();
    if fail_accounting {
        assert!(
            !error.contains("operator bailed"),
            "accounting failure must win: {error}"
        );
    } else {
        assert!(error.contains("bail"), "{error}");
        let usage = gremlin.state.read_field("token_usage").unwrap();
        assert_eq!(usage["prompt_tokens"], 17);
        assert_eq!(usage["completion_tokens"], 3);
        let rows =
            std::fs::read_to_string(config::state_root().join("gr-test-0001/token_usage.jsonl"))
                .unwrap();
        let rows: Vec<serde_json::Value> = rows
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["stage_run_id"], invocation.stage_run_id);
        assert_eq!(rows[0]["prompt_tokens"], 17);
    }
    let mut ended = 0;
    while let Ok(event) = rx.try_recv() {
        assert!(!matches!(event.kind(), EventKind::Done { .. }));
        if let EventKind::Ended { reason } = event.kind() {
            assert_eq!(event.corr_id(), &invocation.corr_id());
            assert_eq!(reason, "bailed");
            ended += 1;
        }
    }
    assert_eq!(ended, 0, "no duplicate or persistence-failure terminal");
    // Execution teardown is not terminal classification. Chat's supervisor
    // owns that classification; without it, no terminal may be published.
}

#[tokio::test]
async fn handle_bail_waits_for_executor_accounting_and_error_boundary() {
    assert_bail_boundary(false, false, false).await;
}

#[tokio::test]
async fn handle_bail_accounting_failure_has_no_success_terminal() {
    assert_bail_boundary(false, true, false).await;
}

#[tokio::test]
async fn inbox_bail_waits_for_accounting_and_accounting_failure_suppresses_ended() {
    assert_bail_boundary(true, false, false).await;
    assert_bail_boundary(true, true, false).await;
}

#[tokio::test]
async fn chat_bail_leaves_terminal_classification_to_supervisor() {
    for inbox in [false, true] {
        assert_bail_boundary(inbox, false, true).await;
        assert_bail_boundary(inbox, true, true).await;
    }
}
