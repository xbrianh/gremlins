//! Agent stage runner.
//!
//! [`run_agent`] drives a single agent stage: it resolves the client and task
//! models, prepares the agent, runs the client, records token usage, persists
//! chat conversation turns, and commits the produced artifacts.
//!
//! The chat conversation helpers (per-turn artifact read/write, compaction,
//! and tool-message repair) live here alongside it.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::artifacts::uri::Uri;
use crate::clients::agent::done_assess::DoneAssessment;
use crate::clients::backend::{ClientError, RunParams};
use crate::clients::client::ModelURI;
use crate::clients::compaction::ResolvedRequestContext;
use crate::config;
use crate::definition::ExecutorStage;
use crate::executor::agent_runner::{commit_agent, prepare_agent};
use crate::executor::gremlin::Gremlin;
use crate::executor::state;
use crate::executor::state::{Checkout, Collision, FileSystemStateStore};
use crate::executor::RunError;
use crate::task_kind::TaskKind;
use gremlins_magic_bus::{emit_corr_event, EventKind};
use indexmap::IndexMap;
use rig_core::completion::Message;

use crate::executor::run::{
    agent_commit_err, agent_prepare_err, client_run_err, collect_checkout_keys, resolve_client,
    stage_failed, stage_key,
};

/// On-disk path for the agent raw stream, routed through the store's blob seam.
pub(crate) fn agent_raw_stream_path(gremlin: &Gremlin, name: &str) -> Result<PathBuf, RunError> {
    let store = gremlin.state.store_ref();
    let fs = store
        .as_any()
        .and_then(|a| a.downcast_ref::<FileSystemStateStore>())
        .ok_or_else(|| {
            RunError::Message("agent raw stream: store is not filesystem-backed".to_string())
        })?;
    let safe_name = crate::core::proc::sanitize_log_filename(name);
    let blob_name = format!("artifacts/stream-{safe_name}.jsonl");
    store
        .open(&blob_name, state::BlobMode::Append)
        .map_err(|e| RunError::Message(format!("agent raw stream: {e}")))?;
    Ok(fs.root_dir().join(&blob_name))
}

// ---------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------

fn usage_baseline_from_state(
    value: Option<&serde_json::Value>,
) -> crate::clients::protocol::UsageStats {
    let counter = |key: &str| value.and_then(|v| v.get(key)).and_then(|v| v.as_u64());
    crate::clients::protocol::UsageStats {
        prompt_tokens: counter("prompt_tokens"),
        completion_tokens: counter("completion_tokens"),
        cached_input_tokens: counter("cached_input_tokens"),
        cache_creation_input_tokens: counter("cache_creation_input_tokens"),
        reasoning_tokens: counter("reasoning_tokens"),
        turns: counter("turns").unwrap_or(0) as usize,
    }
}

fn chat_done_usage(
    baseline: Option<&crate::clients::protocol::UsageStats>,
    local: Option<&crate::clients::protocol::UsageStats>,
) -> Option<crate::clients::protocol::UsageStats> {
    use crate::clients::protocol::{add_opt, UsageStats};
    match (baseline, local) {
        (None, usage) | (usage, None) => usage.cloned(),
        (Some(base), Some(local)) => Some(UsageStats {
            turns: base.turns + local.turns,
            prompt_tokens: add_opt(base.prompt_tokens, local.prompt_tokens),
            completion_tokens: add_opt(base.completion_tokens, local.completion_tokens),
            cached_input_tokens: add_opt(base.cached_input_tokens, local.cached_input_tokens),
            cache_creation_input_tokens: add_opt(
                base.cache_creation_input_tokens,
                local.cache_creation_input_tokens,
            ),
            reasoning_tokens: add_opt(base.reasoning_tokens, local.reasoning_tokens),
        }),
    }
}

/// Move persisted pre-rename balances together, before consulting the native journal.
/// Removing the old keys makes this a one-time migration; new keys always win.
fn migrate_session_chat_usage(gremlin: &Gremlin) {
    let mut fields = serde_json::Map::new();
    let mut delete = Vec::new();
    for suffix in ["starting_usage", "cmd_usage", "outer_usage"] {
        let old = format!("home_{suffix}");
        let new = format!("session_chat_{suffix}");
        if let Some(value) = gremlin.state.read_field(&old) {
            if gremlin.state.read_field(&new).is_none() {
                fields.insert(new, value);
            }
            delete.push(old);
        }
    }
    if !delete.is_empty() {
        gremlin.state.patch(&delete, &fields);
    }
}

fn session_chat_starting_usage(gremlin: &Gremlin) -> crate::clients::protocol::UsageStats {
    migrate_session_chat_usage(gremlin);
    if let Some(value) = gremlin.state.read_field("session_chat_starting_usage") {
        return usage_baseline_from_state(Some(&value));
    }
    // Snapshot legacy balances before the first native collector creates a ledger.
    let legacy = (!gremlin.state.store_ref().exists("native_accounting.json"))
        .then(|| {
            gremlin
                .state
                .read_field("session_chat_outer_usage")
                .or_else(|| gremlin.state.read_field("token_usage"))
        })
        .flatten();
    let usage = usage_baseline_from_state(legacy.as_ref());
    gremlin.state.patch(
        &[],
        serde_json::json!({"session_chat_starting_usage": usage})
            .as_object()
            .unwrap(),
    );
    usage
}

/// Starting balance and command usage are separate from the corrected native ledger.
fn session_chat_native_usage(
    gremlin: &Gremlin,
) -> Result<crate::clients::protocol::UsageStats, RunError> {
    let starting = session_chat_starting_usage(gremlin);
    let command =
        usage_baseline_from_state(gremlin.state.read_field("session_chat_cmd_usage").as_ref());
    let native = gremlin
        .state
        .store_ref()
        .outer_conversational_usage(gremlin.id.as_str())
        .map_err(|error| RunError::Message(format!("session_chat usage journal: {error}")))?;
    let baseline = chat_done_usage(Some(&starting), Some(&command));
    Ok(chat_done_usage(baseline.as_ref(), Some(&native)).unwrap())
}

fn advance_session_chat_cmd_usage(gremlin: &Gremlin, local: &crate::clients::protocol::UsageStats) {
    let previous =
        usage_baseline_from_state(gremlin.state.read_field("session_chat_cmd_usage").as_ref());
    let usage = chat_done_usage(Some(&previous), Some(local)).unwrap();
    gremlin.state.patch(
        &[],
        serde_json::json!({"session_chat_cmd_usage": usage})
            .as_object()
            .unwrap(),
    );
}

fn record_cmd_usage(
    gremlin: &Gremlin,
    stage_path: &str,
    is_chat: bool,
    usage: &crate::clients::protocol::UsageStats,
) {
    let mut delta = HashMap::from([("turns".to_string(), usage.turns as i64)]);
    delta.extend(
        [
            ("prompt_tokens", usage.prompt_tokens),
            ("completion_tokens", usage.completion_tokens),
            ("cached_input_tokens", usage.cached_input_tokens),
            (
                "cache_creation_input_tokens",
                usage.cache_creation_input_tokens,
            ),
            ("reasoning_tokens", usage.reasoning_tokens),
        ]
        .into_iter()
        .filter_map(|(key, value)| value.map(|value| (key.to_string(), value as i64))),
    );
    if is_chat {
        advance_session_chat_cmd_usage(gremlin, usage);
    }
    gremlin.state.accumulate_token_usage(&delta);
    state::emit_stage_token_usage(gremlin.state.store_ref(), stage_path, &delta);
}

/// Merge task-model layers into a kind-keyed map.
pub(crate) fn resolve_task_models(
    global: Option<&IndexMap<String, String>>,
    enclosing: Option<&IndexMap<String, String>>,
    stage: Option<&IndexMap<String, String>>,
) -> Result<HashMap<TaskKind, String>, config::ConfigError> {
    let effective = config::merge_task_models_runtime(global, enclosing, stage);
    config::parse_task_models_map(effective.as_ref())
}

pub(crate) async fn run_agent(
    node: &ExecutorStage,
    gremlin: &mut Gremlin,
    invocation: &crate::executor::stage_boundary::StageInvocation,
    scope: &str,
    stage_usage_path: &str,
    enclosing_model: Option<&ModelURI>,
    enclosing_task_models: Option<&IndexMap<String, String>>,
) -> Result<(), RunError> {
    let ExecutorStage::Agent {
        stage: agent,
        model: _stage_model,
        task_models: stage_task_models,
    } = node
    else {
        unreachable!("run_agent is only called for agent stages")
    };

    let stage_path = if !stage_usage_path.is_empty() {
        stage_key(stage_usage_path, &agent.name)
    } else {
        stage_key(scope, &agent.name)
    };

    // Chat turns publish their stage run identity before TurnStarted. If setup
    // fails before we take that prepared session, revoke it rather than leaving
    // the announced execution live. Pipeline sessions are not published yet.
    let is_chat = gremlin.state.read_str("kind") == "chat";

    let task_models_by_kind =
        if let Some(selection) = &gremlin.runtime_config.resolved_model_selection {
            // Replacement maps are complete, not another layer over old pins.
            resolve_task_models(selection.task_models.as_ref(), None, None)
        } else {
            resolve_task_models(
                gremlin.runtime_config.default_task_models.as_ref(),
                enclosing_task_models,
                stage_task_models.as_ref(),
            )
        };
    let task_models_by_kind = match task_models_by_kind {
        Ok(models) => models,
        Err(error) => {
            if is_chat {
                gremlin.discard_prepared_interactive_session();
            }
            return Err(stage_failed(&agent.name, error));
        }
    };

    let client = match resolve_client(node, gremlin, enclosing_model) {
        Ok(client) => client,
        Err(error) => {
            if is_chat {
                gremlin.discard_prepared_interactive_session();
            }
            return Err(error);
        }
    };
    // Resolve the actual backend before preparing a session. Lookup failures
    // are real stage failures; don't silently publish a command-disabled
    // session and then retry backend selection inside Client::run.
    let backend = match client.get_or_build_backend() {
        Ok(backend) => backend,
        Err(error) => {
            if is_chat {
                gremlin.discard_prepared_interactive_session();
            }
            return Err(stage_failed(&agent.name, error));
        }
    };
    let accepts_commands = backend.accepts_commands();
    // Input and output share the actual invocation identity.
    let session =
        gremlin.next_pipeline_interactive_session(&invocation.stage_run_id, accepts_commands);
    let mut interactive = match session {
        Ok(session) => session,
        Err(error) => return Err(stage_failed(&agent.name, error)),
    };
    interactive.bind_input(
        &gremlin.execution_inputs,
        invocation.input_identity(),
        accepts_commands,
        gremlin.cancel_token.as_deref(),
    );
    let corr_id = invocation.corr_id();
    let framework_subs = gremlin.framework_subs(&agent.name);
    let _attempt = gremlin.state.read_str("attempt");
    let loop_iter = gremlin.loop_iter.clone();

    let checkout_keys = collect_checkout_keys(
        gremlin.state.store_ref(),
        &agent.interpolation_map,
        &agent.outputs_map,
        &agent.options,
        &loop_iter,
        &framework_subs,
        &agent.name,
    )
    .await?;

    let Checkout {
        store: local_registry,
        artifact_dir,
    } = gremlin
        .state
        .checkout_registry(&checkout_keys)
        .await
        .map_err(|error| stage_failed(&agent.name, error))?;

    let mut prepared = prepare_agent(
        agent,
        gremlin.state.store_ref(),
        local_registry.as_ref(),
        &loop_iter,
        &framework_subs,
    )
    .await
    .map_err(|e| agent_prepare_err(e, &agent.name))?;

    prepared.cwd = gremlin.cwd().to_string_lossy().into_owned();
    prepared.artifact_dir = artifact_dir.to_string_lossy().into_owned();

    // Ensure the checkout artifact directory exists so the agent's
    // local state store can create files under it.
    std::fs::create_dir_all(&artifact_dir)?;

    let system_prompt = prepared.system_prompt();

    let stage_env = gremlin.env.clone();

    let task_model_factory: crate::clients::agent::task::TaskModelFactory =
        std::sync::Arc::new(|spec: &str| {
            let spec = spec.to_string();
            Box::pin(async move { crate::clients::client::resolve_model(&spec).await })
        });
    let task_models = if gremlin.runtime_config.resolved_model_selection.is_some() {
        Some(
            crate::clients::agent::task::TaskModelSelector::from_overrides(
                task_models_by_kind,
                task_model_factory,
            )
            .with_fallback_spec(client.to_string()),
        )
    } else {
        crate::clients::agent::task::build_task_selector(task_model_factory, &task_models_by_kind)
    };

    let chat_turn: Option<usize> = chat_turn_from_stage_name(&agent.name);

    // Messages from turns before the current one — the canonical baseline
    // used for diff computation on rollback.
    let mut canonical_prefix: Vec<Message> = Vec::new();
    let verbatim_history = if is_chat {
        let Some(current_turn) = chat_turn else {
            return Err(RunError::Message(format!(
                "chat stage name '{}' does not contain a valid turn number",
                agent.name
            )));
        };

        // Load baseline: turns 1..current_turn-1.
        for t in 1..current_turn {
            let uri = conversation_uri(t);
            let json = gremlin.state.content(&uri, None).await.map_err(|e| {
                RunError::Message(format!("failed to read conversation artifact {uri}: {e}"))
            })?;
            let msgs = messages_from_conversation(&json).map_err(|e| {
                RunError::Message(format!("conversation artifact {uri} unparseable: {e}"))
            })?;
            canonical_prefix.extend(msgs);
        }

        let mut all_msgs = canonical_prefix.clone();

        // Recover partial messages from a prior attempt (soft-interrupt).
        let cur_uri = conversation_uri(current_turn);
        match gremlin.state.content(&cur_uri, None).await {
            Ok(json) => all_msgs.extend(messages_from_conversation(&json).map_err(|e| {
                RunError::Message(format!(
                    "current-turn conversation artifact {cur_uri} unparseable: {e}"
                ))
            })?),
            Err(e)
                if e.downcast_ref::<crate::executor::state::MissingArtifact>()
                    .is_some() => {}
            Err(e) => {
                return Err(RunError::Message(format!(
                    "failed to read current-turn conversation artifact {cur_uri}: {e}"
                )));
            }
        }

        (!all_msgs.is_empty()).then_some(all_msgs)
    } else {
        None
    };

    let mut conv = crate::conversation::standard::StandardConversation::new(
        "",
        false,
        verbatim_history,
        Some(backend),
    );
    // Let the request environment override the compaction threshold
    // so the TUI chat gremlin can pin it to u64::MAX without changing
    // the daemon's process-wide environment (which would affect
    // headless pipeline runs as well).
    if let Some(t) = stage_env
        .get("GREMLINS_COMPACTION_THRESHOLD_TOKENS")
        .and_then(|v| v.parse().ok())
    {
        conv.set_compaction_threshold_tokens(t);
    }

    // ── Bus subscriber for prompt-debug dumps and live token usage ─────
    // The agent loop emits run-scoped `PromptRequest` and `TokenUsage`
    // events. Prompt dumps are DEBUG-gated; token-usage tracking is
    // always active so failed stages can persist partial-turn totals.
    let stage_usage: std::sync::Arc<
        std::sync::Mutex<Option<crate::clients::protocol::UsageStats>>,
    > = std::sync::Arc::new(std::sync::Mutex::new(None));
    let (prompt_dumper_guard, prompt_dumper_task) = {
        use gremlins_magic_bus::{subscribe_run, Recv};
        let (sub, mut rx) = subscribe_run(gremlin.id.as_str());
        let guard = gremlins_magic_bus::connection::BusSubGuard::new(sub);
        let store = gremlin.state.store_handle();
        let prompt_gremlin_id = gremlin.id.to_string();
        let usage_snap = stage_usage.clone();
        let task = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Recv::Event(dispatch) => match dispatch.kind() {
                        EventKind::TokenUsage { id, usage, .. } if *id == prompt_gremlin_id => {
                            *usage_snap.lock().unwrap() = Some(usage.clone());
                        }
                        EventKind::PromptRequest { id, json }
                            if *id == prompt_gremlin_id
                                && crate::core::logging::log_level_threshold()
                                    >= log::LevelFilter::Debug =>
                        {
                            let ts = state::now_stamp_millis()
                                .trim_matches(|c: char| c == '[' || c == ']')
                                .replace(':', "-");
                            let name = format!("prompt_{ts}.txt");
                            match store.open(&name, state::BlobMode::Write) {
                                Ok(mut blob) => {
                                    let _ = std::io::Write::write_all(&mut blob, json.as_bytes());
                                    let _ = std::io::Write::write_all(&mut blob, b"\n");
                                }
                                Err(e) => {
                                    log::warn!("prompt dump: failed to open {name}: {e}");
                                }
                            }
                        }
                        _ => {}
                    },
                    Recv::Lagged(_) => continue,
                    Recv::Closed => break,
                }
            }
        });
        (guard, task)
    };
    let prepopulated_conversation: Box<dyn crate::clients::conversation::Conversation> =
        Box::new(conv);

    let required_outputs = prepared.required_outputs();
    let mut done_check = agent.done_check.clone();
    let mut artifact_check = None;
    let is_dsl = agent
        .options
        .get(crate::executor::dsl_done::DSL_STAGE_KEY)
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if is_dsl {
        if let Some(command) = prepared.done_check.clone() {
            done_check = Some(crate::executor::dsl_done::dsl_done_check(
                crate::executor::dsl_done::DslDoneParams {
                    env: done_check_env(&gremlin.env, &command),
                    command: command.source,
                },
            ));
        }
        // Standalone runs have no supervisor to serve the CLI query. Leave
        // `artifact` unset so Done checks the resolved checkout paths instead.
        if let (false, Some(registry)) = (
            required_outputs.is_empty(),
            gremlin.runtime_config.registry.clone(),
        ) {
            let scope_outputs = prepared
                .output_uris
                .iter()
                .filter_map(|(key, uri, _)| {
                    prepared
                        .output_paths
                        .get(key)
                        .map(|p| (uri.clone(), PathBuf::from(p)))
                })
                .collect();
            artifact_check = Some(crate::executor::dsl_done::dsl_artifact_check(
                crate::executor::dsl_done::ArtifactParams {
                    run_id: gremlin.id.to_string(),
                    registry,
                    env: gremlin.env.clone(),
                    checkout_root: artifact_dir.canonicalize()?,
                    scope_outputs,
                    required_uris: required_outputs.iter().map(|o| o.uri.clone()).collect(),
                },
            ));
        }
    }
    let done_assessment =
        (!required_outputs.is_empty() || done_check.is_some()).then(|| DoneAssessment {
            outputs: required_outputs,
            check: done_check,
            artifact: artifact_check,
        });

    let is_native = client.provider() != "cmd";
    let usage_baseline = if is_chat {
        Some(session_chat_native_usage(gremlin)?)
    } else {
        None
    };
    let accounting = if is_native {
        let collector = crate::executor::accounting::AccountingCollector::start(
            gremlin.id.as_str(),
            gremlin.state.store_handle(),
        )
        .map_err(|error| stage_failed(&agent.name, error))?;
        collector
            .register_execution(&invocation.stage_run_id, &stage_path)
            .map_err(|error| stage_failed(&agent.name, error))?;
        Some(collector)
    } else {
        None
    };
    let params = RunParams {
        defer_done: is_chat || is_native,
        output_corr_id: Some(corr_id.clone()),
        usage_baseline: usage_baseline.clone(),
        prompt: prepared.user_prompt(),
        label: prepared.name.clone(),
        model: if gremlin.runtime_config.resolved_model_selection.is_some() {
            Some(client.model().to_string())
        } else {
            prepared
                .model
                .clone()
                .or_else(|| Some(client.model().to_string()))
        },
        raw_path: Some(
            agent_raw_stream_path(gremlin, &prepared.name)
                .map_err(|e| stage_failed(&prepared.name, e))?,
        ),
        capture_events: false,
        on_timeout_prompt: None,
        max_retries: 3,
        cwd: Some(gremlin.cwd()),
        artifact_dir: Some(artifact_dir.clone()),
        idle_timeout: None,
        extra_env: Some(stage_env),
        expected_artifact_paths: Vec::new(),
        done: done_assessment,
        system_prompt: Some(system_prompt),
        gremlin_id: Some(gremlin.id.to_string()),
        base_env: Some(gremlin.env.clone()),
        allow_all: prepared.allow_all,
        task_models,
        cancel_token: gremlin.cancel_token.clone(),
        prepopulated_conversation,
        discovered_skills: gremlin.discovered_skills.clone(),
        extra_sandbox_roots: gremlin.extra_sandbox_roots.clone(),
        path_context: {
            let mut path_context = crate::clients::path_context::PathContext::new(&prepared.prompt);
            let cwd = prepared.cwd.clone();
            tokio::task::spawn_blocking(move || {
                path_context.resolve(std::path::Path::new(&cwd));
                path_context
            })
            .await
            .unwrap_or_default()
        },
    };

    let effective_model = params
        .model
        .clone()
        .unwrap_or_else(|| client.model().to_string());
    let effective_model: &str = &effective_model;
    let msg = format!(
        "[{name}][{model}] agent: preparing",
        name = agent.name,
        model = effective_model,
    );
    emit_corr_event(
        &corr_id,
        EventKind::LogLine {
            id: gremlin.id.to_string(),
            line: msg,
        },
    );

    log::debug!(
        "agent stage '{}' (gremlin={}): invoking client.run (model={})",
        prepared.name,
        gremlin.id.as_str(),
        params.model.as_deref().unwrap_or("default")
    );

    let result = async {
        let run_result = client.run(params, interactive).await;
        let accounting_result = if let Some(collector) = &accounting {
            collector.flush().await
        } else {
            Ok(())
        };
        // Close our subscription so the prompt-dump task drains any
        // already-buffered events and then exits.
        drop(prompt_dumper_guard);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), prompt_dumper_task).await;
        accounting_result.map_err(|error| stage_failed(&prepared.name, error))?;

        // Persist even when the backend failed after reporting partial requests.
        let native_session_chat_usage = if is_chat && is_native {
            let usage = session_chat_native_usage(gremlin)?;
            gremlin.state.patch(
                &[],
                serde_json::json!({"session_chat_outer_usage": usage})
                    .as_object()
                    .unwrap(),
            );
            Some(usage)
        } else {
            None
        };

        let completed = match run_result {
            Ok(c) => c,
            Err(error) => {
                // Persist partial token usage from failed agent-loop turns so
                // the Gremlin::finish() snapshot has accurate totals rather
                // than the previous stage's stale values.
                if let Some(usage) = stage_usage.lock().unwrap().take().filter(|_| !is_native) {
                    record_cmd_usage(gremlin, &stage_path, is_chat, &usage);
                }
                // Earlier turns remain authoritative in their own artifacts. Persist
                // only this prompt so cancellation cannot replay the signed prefix.
                if matches!(&error, ClientError::Cancelled) && is_chat {
                    if let Some(turn_num) = chat_turn {
                        let messages = [Message::user(prepared.user_prompt())];
                        if let Err(e) =
                            persist_chat_conversation_turn(gremlin, turn_num, &messages).await
                        {
                            log::warn!("chat: failed to persist conversation on cancel: {e}");
                        }
                    }
                }
                if matches!(&error, ClientError::Bail { .. }) && !is_chat {
                    emit_corr_event(
                        &corr_id,
                        EventKind::Ended {
                            reason: "bailed".into(),
                        },
                    );
                }
                return Err(client_run_err(error, &prepared.name));
            }
        };

        emit_corr_event(
            &corr_id,
            EventKind::LogLine {
                id: gremlin.id.to_string(),
                line: format!(
                    "[{name}][{model}] agent: completed (turns={turns})",
                    name = prepared.name,
                    model = effective_model,
                    turns = completed.token_usage.as_ref().map(|u| u.turns).unwrap_or(0)
                ),
            },
        );

        if let Some(usage) = completed.token_usage.as_ref().filter(|_| !is_native) {
            record_cmd_usage(gremlin, &stage_path, is_chat, usage);
        }

        if is_chat {
            if let Some(usage) =
                chat_done_usage(usage_baseline.as_ref(), completed.token_usage.as_ref())
                    .filter(|_| !is_native)
            {
                gremlin.state.patch(
                    &[],
                    serde_json::json!({"session_chat_outer_usage": usage})
                        .as_object()
                        .unwrap(),
                );
            }
            if let (Some(conversation), Some(turn_num)) = (&completed.conversation, chat_turn) {
                if conversation.compacted_prefix() {
                    // Compaction replaced the prefix — write the full compacted
                    // transcript as turn 0001 and clear all other per-turn
                    // artifacts so the next submission does not reload
                    // compacted-away messages or duplicate retained ones.
                    let messages = conversation.messages();
                    if let Err(e) = persist_compacted_conversation_with_context(
                        gremlin,
                        &messages,
                        conversation.request_context(),
                    )
                    .await
                    {
                        log::warn!("chat: failed to persist compacted conversation: {e}");
                    }
                } else {
                    let messages = conversation.messages();
                    let diff: Vec<Message> =
                        if canonical_prefix.is_empty() || messages.len() < canonical_prefix.len() {
                            messages.clone()
                        } else {
                            let cp = &messages[..canonical_prefix.len()];
                            let same = serde_json::to_string(cp).unwrap_or_default()
                                == serde_json::to_string(&canonical_prefix).unwrap_or_default();
                            if same {
                                messages[canonical_prefix.len()..].to_vec()
                            } else {
                                messages.clone()
                            }
                        };
                    if let Err(e) = persist_chat_conversation_turn_with_context(
                        gremlin,
                        turn_num,
                        &diff,
                        conversation.request_context(),
                    )
                    .await
                    {
                        log::warn!("chat: failed to persist conversation turn {turn_num}: {e}");
                    }
                }
            }
        }

        commit_agent(&prepared, local_registry.as_ref())
            .await
            .map_err(|error| agent_commit_err(error, &prepared.name))?;

        // Merge the localized registry back into the main registry.
        gremlin
            .state
            .join(local_registry.as_ref(), Collision::Ignore, None)
            .await
            .map_err(|error| stage_failed(&prepared.name, error))?;

        // Native completion (including attachment output) must follow durable
        // accounting, artifact commit, and registry merge, not just the model loop.
        if (is_chat || is_native) && completed.exit_code == 0 {
            emit_corr_event(
                &corr_id,
                EventKind::Done {
                    text: completed.text_result.unwrap_or_default(),
                    summary: completed.summary.unwrap_or_default(),
                    usage: native_session_chat_usage.or_else(|| {
                        chat_done_usage(usage_baseline.as_ref(), completed.token_usage.as_ref())
                    }),
                },
            );
        }

        Ok(())
    }
    .await;
    result
}

// ---------------------------------------------------------------------------
// Chat conversation artifacts
// ---------------------------------------------------------------------------

/// Deserialize a legacy message array or a context-bearing conversation envelope.
pub(crate) fn messages_from_conversation(
    json: &str,
) -> Result<Vec<rig_core::completion::Message>, String> {
    conversation_from_json(json).map(|(messages, _)| messages)
}

fn conversation_from_json(
    json: &str,
) -> Result<(Vec<Message>, Option<ResolvedRequestContext>), String> {
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum ConversationArtifact {
        Legacy(Vec<Message>),
        Envelope {
            messages: Vec<Message>,
            #[serde(default)]
            request_context: Option<Box<ResolvedRequestContext>>,
        },
    }

    match serde_json::from_str(json)
        .map_err(|e| format!("chat: failed to parse conversation artifact: {e}"))?
    {
        ConversationArtifact::Legacy(messages) => Ok((messages, None)),
        ConversationArtifact::Envelope {
            messages,
            request_context,
        } => Ok((messages, request_context.map(|context| *context))),
    }
}

/// Persist messages to `artifact://conversation/{turn:04}.json`.
///
/// Returns the canonical artifact path on success. Write errors are
/// propagated so callers never mistake a lost write for a persisted turn.
pub(crate) async fn persist_chat_conversation_turn(
    gremlin: &Gremlin,
    turn: usize,
    messages: &[rig_core::completion::Message],
) -> Result<String, Box<dyn std::error::Error>> {
    persist_chat_conversation_turn_with_context(gremlin, turn, messages, None).await
}

/// Bind the credential-free request context atomically with its committed messages.
async fn persist_chat_conversation_turn_with_context(
    gremlin: &Gremlin,
    turn: usize,
    messages: &[Message],
    context: Option<ResolvedRequestContext>,
) -> Result<String, Box<dyn std::error::Error>> {
    let value = match context {
        Some(context) => serde_json::json!({"messages": messages, "request_context": context}),
        None => serde_json::to_value(messages)?,
    };
    let json = serde_json::to_string(&value).map_err(|e| {
        format!("chat: failed to serialize conversation artifact for turn {turn}: {e}")
    })?;
    let uri = Uri::parse(&conversation_uri(turn))?;
    gremlin.state.write_into_registry(&uri, &json).await
}

/// Format a per-turn conversation artifact URI.
pub(crate) fn conversation_uri(turn: usize) -> String {
    format!("artifact://conversation/{turn:04}.json")
}

/// Derive a turn number from a chat stage name like `chat-0001`.
pub(crate) fn chat_turn_from_stage_name(name: &str) -> Option<usize> {
    name.strip_prefix("chat-").and_then(|s| s.parse().ok())
}

/// Clear a per-turn conversation artifact by writing `[]`.
/// Used by soft-interrupt recovery to clear partial messages from a cancelled turn.
pub(crate) async fn clear_chat_turn_artifact(gremlin: &Gremlin, turn: usize) {
    if let Ok(uri) = Uri::parse(&conversation_uri(turn)) {
        let _ = gremlin.state.write_into_registry(&uri, "[]").await;
    }
}

/// Walk per-turn conversation artifacts concatenating message arrays.
#[cfg(test)]
pub(crate) async fn read_chat_conversation(
    gremlin: &Gremlin,
    max_turn: Option<usize>,
) -> Vec<Message> {
    read_chat_conversation_with_context(gremlin, max_turn)
        .await
        .0
}

/// The last nonempty committed turn owns the context; never inherit context
/// across a newer legacy/context-free turn. Empty cleared turns do not own it.
pub(crate) async fn read_chat_conversation_with_context(
    gremlin: &Gremlin,
    max_turn: Option<usize>,
) -> (Vec<Message>, Option<ResolvedRequestContext>) {
    let mut all_msgs = Vec::new();
    let mut context = None;
    for t in 1.. {
        if max_turn.is_some_and(|max| t > max) {
            break;
        }
        let uri = conversation_uri(t);
        match gremlin.state.content(&uri, None).await {
            Ok(json) => match conversation_from_json(&json) {
                Ok((msgs, turn_context)) => {
                    if !msgs.is_empty() {
                        context = turn_context;
                    }
                    all_msgs.extend(msgs);
                }
                Err(_) => {
                    log::warn!("chat: conversation artifact {uri} unparseable; stopping walk");
                    break;
                }
            },
            Err(e)
                if e.downcast_ref::<crate::executor::state::MissingArtifact>()
                    .is_some() =>
            {
                break;
            }
            Err(_) => continue,
        }
    }
    (all_msgs, context)
}

/// Overwrite per-turn conversation artifacts from a compacted conversation.
///
/// Writes the full compacted history as turn 0001 and clears every other
/// registered `conversation/NNNN.json` artifact. The replacement write must
/// succeed before any later artifact is cleared — otherwise a failure would
/// destroy the only stored copies of those messages while still reporting
/// success.
#[cfg(test)]
pub(crate) async fn persist_compacted_conversation(
    gremlin: &Gremlin,
    messages: &[Message],
) -> Result<(), Box<dyn std::error::Error>> {
    persist_compacted_conversation_with_context(gremlin, messages, None).await
}

pub(crate) async fn persist_compacted_conversation_with_context(
    gremlin: &Gremlin,
    messages: &[Message],
    context: Option<ResolvedRequestContext>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Save replacement and context together before clearing the old prefix.
    persist_chat_conversation_turn_with_context(gremlin, 1, messages, context).await?;

    // Enumerate the registered conversation artifacts rather than assuming a
    // numeric range. Turn counts are unbounded, and scanning only 2..1000
    // would leave later artifacts in place to be replayed as duplicate history.
    let keys = gremlin.state.keys().await;
    for key in keys {
        let Some(name) = key.strip_prefix("artifact://conversation/") else {
            continue;
        };
        let Some(turn) = name
            .strip_suffix(".json")
            .and_then(|s| s.parse::<usize>().ok())
        else {
            continue;
        };
        if turn == 1 {
            continue;
        }
        let Ok(uri) = Uri::parse(&key) else {
            continue;
        };
        gremlin.state.write_into_registry(&uri, "[]").await?;
    }
    Ok(())
}

/// The environment of the author's `done_check`: the stage environment with
/// the command's substituted values layered on top.
pub(crate) fn done_check_env(
    stage_env: &HashMap<String, String>,
    command: &crate::executor::agent_runner::DoneCommand,
) -> HashMap<String, String> {
    stage_env
        .iter()
        .chain(&command.env)
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

#[cfg(test)]
#[path = "run_agent/bail_tests.rs"]
mod bail_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builders::agent::AgentBuilder;
    use crate::clients::agent::task::TaskModel;
    use crate::clients::backend::Backend;
    use crate::clients::client::{install_test_backend, Client};
    use crate::clients::protocol::CompletedRun;
    use crate::clients::InteractiveSession;
    use crate::definition::{ResolvedModelSelection, StageSpec, StaticDefinition};
    use crate::executor::gremlin::{validate_gremlin_id, RuntimeConfig};
    use crate::executor::state::StateData;
    use crate::schemas::bootstrap::Bootstrap;
    use crate::test_support::Sandbox;
    use std::sync::{Arc, Mutex};

    #[test]
    fn chat_done_usage_adds_delta_without_mutating_it() {
        let baseline = usage_baseline_from_state(Some(&serde_json::json!({
            "turns": 3, "prompt_tokens": 17, "completion_tokens": 0
        })));
        let local = usage_baseline_from_state(Some(&serde_json::json!({
            "turns": 1, "prompt_tokens": 5, "cached_input_tokens": 2
        })));
        let usage = chat_done_usage(Some(&baseline), Some(&local)).unwrap();
        assert_eq!(usage.turns, 4);
        assert_eq!(usage.prompt_tokens, Some(22));
        assert_eq!(usage.completion_tokens, Some(0));
        assert_eq!(usage.cached_input_tokens, Some(2));
        assert_eq!(usage.reasoning_tokens, None);
        assert_eq!(local.turns, 1);
        assert_eq!(local.prompt_tokens, Some(5));
        assert!(chat_done_usage(None, None).is_none());
    }

    #[test]
    fn chat_usage_baseline_preserves_optional_counters() {
        let empty = usage_baseline_from_state(None);
        assert_eq!(empty.turns, 0);
        assert_eq!(empty.prompt_tokens, None);
        let value = serde_json::json!({
            "turns": 4, "prompt_tokens": 123, "completion_tokens": 0,
            "cached_input_tokens": 12, "cache_creation_input_tokens": 8,
            "reasoning_tokens": 7
        });
        assert_eq!(
            serde_json::to_value(usage_baseline_from_state(Some(&value))).unwrap(),
            value
        );
    }

    #[test]
    fn conversation_decoder_accepts_legacy_arrays_and_envelopes() {
        let messages = vec![Message::user("saved history")];
        let value = serde_json::to_value(&messages).unwrap();
        for artifact in [value.clone(), serde_json::json!({"messages": value})] {
            let (restored, context) = conversation_from_json(&artifact.to_string()).unwrap();
            assert_eq!(serde_json::to_value(restored).unwrap(), value);
            assert!(context.is_none());
        }

        // Older envelopes have no provider and must remain readable.
        let artifact = serde_json::json!({
            "messages": value,
            "request_context": {
                "model": "old-model", "system": null, "tools": [],
                "extra": null, "temperature": null, "max_tokens": null
            }
        });
        let (restored, context) = conversation_from_json(&artifact.to_string()).unwrap();
        assert_eq!(serde_json::to_value(restored).unwrap(), value);
        let context = context.unwrap();
        assert!(context.provider.is_empty());
        assert!(context.system.is_none());
        assert_eq!(context.model, "old-model");
    }

    #[test]
    fn conversation_decoder_rejects_invalid_artifacts() {
        for artifact in ["not json", "{}", "{\"messages\": false}", "null"] {
            assert!(conversation_from_json(artifact).is_err());
        }
    }

    #[tokio::test]
    async fn chat_request_context_tracks_committed_history() {
        let stage = AgentBuilder::new("chat-0001")
            .prompt("hello")
            .build()
            .unwrap();
        let (_sandbox, gremlin) = test_gremlin(stage);
        let context = ResolvedRequestContext {
            usage_attribution: None,
            provider: "anthropic".into(),
            model: "retained-model".into(),
            system: Some("resolved workspace and skills".into()),
            tools: vec![],
            extra: Some(
                serde_json::json!({"thinking": {"type": "enabled", "budget_tokens": 2048}}),
            ),
            temperature: Some(0.25),
            max_tokens: Some(4096),
        };
        let messages = vec![Message::user("committed"), Message::assistant("answer")];
        persist_chat_conversation_turn_with_context(&gremlin, 1, &messages, Some(context.clone()))
            .await
            .unwrap();
        let (restored, retained) = read_chat_conversation_with_context(&gremlin, None).await;
        assert_eq!(
            serde_json::to_value(restored).unwrap(),
            serde_json::to_value(&messages).unwrap()
        );
        assert_eq!(
            serde_json::to_value(retained).unwrap(),
            serde_json::to_value(&context).unwrap()
        );

        // Cleared artifacts must not discard the replacement prefix's context.
        persist_chat_conversation_turn(&gremlin, 2, &[])
            .await
            .unwrap();
        assert!(read_chat_conversation_with_context(&gremlin, None)
            .await
            .1
            .is_some());
        // A newer context-free turn cannot reuse the older turn's request.
        persist_chat_conversation_turn(&gremlin, 2, &[Message::user("legacy")])
            .await
            .unwrap();
        assert!(read_chat_conversation_with_context(&gremlin, None)
            .await
            .1
            .is_none());
        persist_compacted_conversation_with_context(&gremlin, &messages, Some(context.clone()))
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(read_chat_conversation_with_context(&gremlin, None).await.1)
                .unwrap(),
            serde_json::to_value(context).unwrap()
        );
        // Replacing a context-bearing artifact without context removes it atomically.
        persist_chat_conversation_turn(&gremlin, 1, &messages)
            .await
            .unwrap();
        assert!(read_chat_conversation_with_context(&gremlin, None)
            .await
            .1
            .is_none());
    }

    #[tokio::test]
    async fn leaf_producer_keeps_its_identity_after_session_replacement() {
        use crate::clients::interactive::InteractiveSessionFactory;
        use gremlins_magic_bus::{subscribe_run, Recv};

        let id = "executor-leaf-producer-identity";
        let (mut sessions, _) = InteractiveSessionFactory::for_chat();
        let first = sessions.next_session();
        let expected = first.snapshot().stage_run_id().unwrap().to_owned();
        let invocation =
            crate::executor::stage_boundary::StageInvocation::enter(id, "turn-1", "agent");
        let corr_id = invocation.corr_id();
        assert_ne!(corr_id.stage_run_id(), Some(expected.as_str()));
        let successor = sessions.next_session();
        assert_ne!(successor.snapshot().stage_run_id(), Some(expected.as_str()));
        assert_eq!(invocation.corr_id(), corr_id);
        let (sub, mut rx) = subscribe_run(id);
        let _guard = gremlins_magic_bus::connection::BusSubGuard::new(sub);
        emit_corr_event(
            &corr_id,
            EventKind::LogLine {
                id: id.into(),
                line: "late leaf output".into(),
            },
        );
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap();
        let Recv::Event(event) = event else {
            panic!("missing leaf event")
        };
        assert_eq!(event.corr_id().run(), Some(id));
        assert_eq!(event.corr_id(), &corr_id);
        assert_eq!(event.corr_id().stage(), Some("turn-1"));
        assert_eq!(first.snapshot().stage_run_id(), Some(expected.as_str()));
    }

    const CAPTURED: &str = "deliberate run_agent dispatch capture";

    struct SessionChatUsageBackend {
        baselines: Mutex<Vec<u64>>,
    }

    impl SessionChatUsageBackend {
        fn report_usage(&self, params: &RunParams, completed: bool) {
            use crate::clients::protocol::UsageStats;
            use gremlins_magic_bus::ModelCallUsage;
            let baseline = params.usage_baseline.as_ref().unwrap();
            self.baselines
                .lock()
                .unwrap()
                .push(baseline.prompt_tokens.unwrap_or(0));
            let corr = params.output_corr_id.as_ref().unwrap();
            // Live snapshots may be corrected or include Task usage. Only the
            // final request record is durable, as with the native stream guard.
            for (request, tokens, task) in [
                ("outer", 4, false),
                ("outer", 6, false),
                ("task", 100, true),
            ] {
                let usage = UsageStats {
                    prompt_tokens: Some(tokens),
                    turns: 1,
                    completion_tokens: None,
                    cached_input_tokens: None,
                    cache_creation_input_tokens: None,
                    reasoning_tokens: None,
                };
                emit_corr_event(
                    corr,
                    EventKind::TokenUsage {
                        id: params.gremlin_id.clone().unwrap(),
                        usage: usage.clone(),
                        cost_usd: None,
                        last_prompt_tokens: Some(tokens),
                        last_cached_input_tokens: None,
                    },
                );
                if tokens == 4 {
                    continue;
                }
                emit_corr_event(
                    corr,
                    EventKind::ModelCallUsage {
                        usage: ModelCallUsage {
                            request_id: format!("{}-{request}", corr.stage_run_id().unwrap()),
                            provider: "anthropic".into(),
                            model: "session_chat-test".into(),
                            agent_id: if task { "[task.test]" } else { "" }.into(),
                            request_kind: String::new(),
                            reported: true,
                            completed,
                            raw: serde_json::json!({}),
                            usage,
                        },
                    },
                );
            }
        }
    }

    struct SessionChatFailureBackend(SessionChatUsageBackend);

    #[async_trait::async_trait]
    impl Backend for SessionChatFailureBackend {
        async fn run(
            &self,
            params: RunParams,
            _interactive: InteractiveSession,
        ) -> Result<CompletedRun, ClientError> {
            self.0.report_usage(&params, false);
            Err(ClientError::Runtime {
                message: CAPTURED.into(),
            })
        }

        fn model(&self) -> &str {
            self.0.model()
        }
    }

    #[async_trait::async_trait]
    impl Backend for SessionChatUsageBackend {
        async fn run(
            &self,
            mut params: RunParams,
            _interactive: InteractiveSession,
        ) -> Result<CompletedRun, ClientError> {
            use crate::clients::protocol::UsageStats;

            self.report_usage(&params, true);
            params
                .prepopulated_conversation
                .set_next_prompt(&params.prompt);
            params
                .prepopulated_conversation
                .render_turn(false, params.system_prompt.clone())
                .await
                .unwrap();
            params
                .prepopulated_conversation
                .finalize(1, "session_chat reply", &[], None);
            Ok(CompletedRun {
                exit_code: 0,
                text_result: Some("session_chat reply".into()),
                summary: None,
                events: None,
                cost_usd: None,
                token_usage: Some(UsageStats {
                    prompt_tokens: Some(6),
                    turns: 1,
                    completion_tokens: None,
                    cached_input_tokens: None,
                    cache_creation_input_tokens: None,
                    reasoning_tokens: None,
                }),
                conversation: Some(params.prepopulated_conversation),
            })
        }

        fn model(&self) -> &str {
            "session_chat-test"
        }
    }

    const SESSION_CHAT_CORRECTION: &str = "Use the corrected cobalt lighthouse coordinates.";

    struct NativeLoopBackend {
        baselines: Mutex<Vec<u64>>,
        locals: Mutex<Vec<u64>>,
        inputs: Option<crate::clients::execution_input::InputRegistry>,
    }

    #[async_trait::async_trait]
    impl Backend for NativeLoopBackend {
        async fn run(
            &self,
            params: RunParams,
            interactive: InteractiveSession,
        ) -> Result<CompletedRun, ClientError> {
            use crate::clients::agent::agent_loop::{loop_opts, run_agent_loop, test_ctx};
            use crate::clients::agent::task::{TaskModel, TaskModelSelector};
            use rig_core::completion::Usage;
            use rig_core::test_utils::{MockCompletionModel, MockStreamEvent as E};
            assert!(params.defer_done, "executor owns native completion");
            let baseline = params
                .usage_baseline
                .as_ref()
                .and_then(|u| u.prompt_tokens)
                .unwrap_or(0);
            self.baselines.lock().unwrap().push(baseline);
            let first_turn = self.locals.lock().unwrap().is_empty();
            let correction = if first_turn {
                self.inputs.as_ref().map(|inputs| {
                    let corr = params.output_corr_id.as_ref().unwrap();
                    let (_, sender) = inputs
                        .resolve(
                            corr.run().unwrap(),
                            corr.stage_run_id().unwrap(),
                            corr.stage(),
                        )
                        .expect("correction is pinned to this invocation A");
                    sender
                        .try_send("correction-A".into(), SESSION_CHAT_CORRECTION.into())
                        .unwrap()
                })
            } else {
                if self.inputs.is_some() {
                    let history =
                        serde_json::to_string(&params.prepopulated_conversation.messages())
                            .unwrap();
                    assert_eq!(history.matches(SESSION_CHAT_CORRECTION).count(), 1);
                }
                None
            };
            let done = || E::tool_call("done", "Done", serde_json::json!({"summary": "finished"}));
            let model = if first_turn {
                MockCompletionModel::from_stream_turns([
                    vec![
                        E::tool_call(
                            "task",
                            "Task",
                            serde_json::json!({"kind": "Scout", "prompt": "inspect"}),
                        ),
                        E::final_response(Usage::new().input_tokens(100)),
                    ],
                    vec![done(), E::final_response(Usage::new().input_tokens(50))],
                ])
            } else {
                MockCompletionModel::from_stream_turns([vec![
                    done(),
                    E::final_response(Usage::new().input_tokens(20)),
                ]])
            };
            let child = MockCompletionModel::from_stream_turns([vec![
                done(),
                E::final_response(Usage::new().input_tokens(30)),
            ]]);
            let factory = crate::clients::agent::task_models::sync_factory(move |_| {
                Ok(TaskModel {
                    model: child.clone().erase(),
                    backend: None,
                    temperature: None,
                    max_tokens: None,
                    extra: None,
                    preserve_reasoning: None,
                })
            });
            let mut ctx = test_ctx(params.cwd.clone(), None);
            let prompt = params.prompt.clone();
            ctx.prepopulated_conversation = params.prepopulated_conversation.clone();
            ctx.params = params;
            ctx.params.task_models = TaskModelSelector::new(
                HashMap::from([(TaskKind::Scout, "test/child".into())]),
                factory,
            );
            ctx.idle_timeout = 5.0;
            let result = run_agent_loop(
                model.erase(),
                &prompt,
                ctx,
                crate::clients::CancelToken::new(),
                loop_opts(None),
                interactive,
            )
            .await?;
            if let Some(receipt) = correction {
                assert_eq!(
                    receipt.await.unwrap(),
                    crate::clients::execution_input::Delivery::Delivered
                );
                let history =
                    serde_json::to_string(&result.conversation.as_ref().unwrap().messages())
                        .unwrap();
                assert_eq!(history.matches(SESSION_CHAT_CORRECTION).count(), 1);
            }
            self.locals
                .lock()
                .unwrap()
                .push(result.token_usage.as_ref().unwrap().prompt_tokens.unwrap());
            Ok(result)
        }

        fn model(&self) -> &str {
            "native-loop-test"
        }
    }

    #[tokio::test]
    async fn session_chat_real_loop_correction_replay_and_next_turn_usage() {
        use crate::executor::stage_boundary::StageInvocation;
        let stage = AgentBuilder::new("chat-0001")
            .prompt(CHAT_A)
            .build()
            .unwrap();
        let (_sandbox, mut gremlin) = test_gremlin(stage);
        gremlin.state.patch(
            &[],
            serde_json::json!({
                "kind": "chat", "token_usage": {"prompt_tokens": 900}
            })
            .as_object()
            .unwrap(),
        );
        let spec = "anthropic/session_chat-real-loop-usage";
        let backend = Arc::new(NativeLoopBackend {
            baselines: Mutex::new(vec![]),
            locals: Mutex::new(vec![]),
            inputs: Some(gremlin.execution_inputs.clone()),
        });
        install_test_backend(&ModelURI::parse(spec).unwrap().to_string(), backend.clone());
        gremlin.model_override = Some(spec.into());
        let registry = Arc::new(crate::executor::supervisor::run_registry::RunRegistry::new(
            tokio::sync::watch::channel(false).0,
        ));
        gremlin = registry
            .register_chat_gremlin(
                gremlin.id.to_string(),
                gremlin,
                crate::clients::CancelToken::new(),
            )
            .await;
        let (_guard, mut rx) = gremlins_magic_bus::subscribe_run(gremlin.id.as_str());
        let mut executions = vec![];
        for turn in 1..=2 {
            let mut node = chat_node(turn, CHAT_A).await;
            let ExecutorStage::Agent { model, .. } = &mut node else {
                unreachable!()
            };
            *model = Some(ModelURI::parse(spec).unwrap());
            let invocation = StageInvocation::enter(gremlin.id.as_str(), node.name(), "agent");
            run_agent(&node, &mut gremlin, &invocation, "", "", None, None)
                .await
                .unwrap();
            executions.push(invocation.stage_run_id.clone());
            let mut records = vec![];
            let mut terminal = vec![];
            let mut live = vec![];
            let mut missed_live = 0;
            while let Ok(event) = rx.try_recv() {
                match event.kind() {
                    EventKind::ModelCallUsage { usage } => {
                        records.push((event.corr_id().clone(), usage.clone()))
                    }
                    EventKind::Done { usage, .. } => {
                        terminal.push(usage.as_ref().unwrap().prompt_tokens.unwrap())
                    }
                    EventKind::TokenUsage { usage, .. } => {
                        if turn == 1 && usage.prompt_tokens == Some(1050) {
                            missed_live += 1;
                            continue;
                        }
                        live.push(usage.prompt_tokens.unwrap());
                    }
                    _ => {}
                }
            }
            assert!(
                gremlin
                    .execution_inputs
                    .resolve(
                        gremlin.id.as_str(),
                        &invocation.stage_run_id,
                        Some(node.name())
                    )
                    .is_none(),
                "completed invocation A must not accept corrections on turn B"
            );
            let (history, _) = read_chat_conversation_with_context(&gremlin, None).await;
            assert_eq!(
                serde_json::to_string(&history)
                    .unwrap()
                    .matches(SESSION_CHAT_CORRECTION)
                    .count(),
                1
            );
            assert_eq!(terminal, [if turn == 1 { 1050 } else { 1070 }]);
            if turn == 1 {
                // The viewer misses the last live snapshot, but receives Done.
                assert_eq!(missed_live, 1);
                assert_ne!(live.last(), Some(&1050));
                let mut displayed = live.last().copied();
                assert_ne!(displayed, Some(1050));
                displayed = terminal.last().copied();
                assert_eq!(displayed, Some(1050));
            } else {
                assert_eq!(live.last(), Some(&1070));
            }
            assert_eq!(
                gremlin.state.read_field("token_usage").unwrap()["prompt_tokens"],
                if turn == 1 { 1080 } else { 1100 }
            );
            assert_eq!(
                gremlin
                    .state
                    .read_field("session_chat_outer_usage")
                    .unwrap()["prompt_tokens"],
                if turn == 1 { 1050 } else { 1070 }
            );
            if turn == 1 {
                assert_eq!(records.len(), 3);
                assert_eq!(
                    records
                        .iter()
                        .map(|(_, u)| &u.request_id)
                        .collect::<std::collections::HashSet<_>>()
                        .len(),
                    3
                );
                assert!(
                    records
                        .iter()
                        .all(|(corr, _)| corr.stage_run_id()
                            == Some(invocation.stage_run_id.as_str()))
                );
                let before = gremlin.state.read_field("token_usage").unwrap();
                let rows_before =
                    std::fs::read(config::state_root().join("gr-test-0001/token_usage.jsonl"))
                        .unwrap();
                let (client, server) = tokio::net::UnixStream::pair().unwrap();
                let (read, write) = server.into_split();
                let writer = gremlins_magic_bus::connection::SharedWriter::new(write);
                let request = serde_json::json!({
                    "op": "watch", "id": gremlin.id.as_str(),
                    "stage_run_id": invocation.stage_run_id, "replay": true
                });
                let watch_registry = registry.clone();
                let watch = tokio::spawn(async move {
                    crate::executor::supervisor::attachment::handle_watch(
                        &request,
                        &watch_registry,
                        tokio::io::BufReader::new(read),
                        &writer,
                    )
                    .await;
                });
                let mut reader = tokio::io::BufReader::new(client);
                let mut replay_done = vec![];
                loop {
                    let frame = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        crate::executor::socket::read_json_line(&mut reader),
                    )
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                    if frame["type"] == "done" {
                        assert_eq!(frame["stage_run_id"], invocation.stage_run_id);
                        replay_done.push(frame["usage"]["prompt_tokens"].clone());
                    }
                    if frame["type"] == "replay_checkpoint" {
                        break;
                    }
                }
                assert_eq!(replay_done, [serde_json::json!(1050)]);
                watch.abort();
                let _ = watch.await;
                assert_eq!(gremlin.state.read_field("token_usage").unwrap(), before);
                assert_eq!(
                    std::fs::read(config::state_root().join("gr-test-0001/token_usage.jsonl"))
                        .unwrap(),
                    rows_before
                );
                assert_eq!(
                    session_chat_native_usage(&gremlin).unwrap().prompt_tokens,
                    Some(1050)
                );
                // Reopen with the cached fallback deliberately absent.
                gremlin.state.patch(
                    &[],
                    serde_json::json!({"session_chat_outer_usage": null})
                        .as_object()
                        .unwrap(),
                );
                gremlin.state = StateData::open("gr-test").unwrap();
                assert_eq!(
                    session_chat_native_usage(&gremlin).unwrap().prompt_tokens,
                    Some(1050)
                );
                while rx.try_recv().is_ok() {}
            }
        }
        let rows =
            std::fs::read_to_string(config::state_root().join("gr-test-0001/token_usage.jsonl"))
                .unwrap();
        let rows: Vec<serde_json::Value> = rows
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        assert_ne!(executions[0], executions[1]);
        for (execution, tokens) in executions.iter().zip([180, 20]) {
            let row = rows
                .iter()
                .find(|row| row["stage_run_id"] == *execution)
                .unwrap();
            assert_eq!(row["prompt_tokens"], tokens);
        }
        assert_eq!(*backend.baselines.lock().unwrap(), [900, 1050]);
        assert_eq!(*backend.locals.lock().unwrap(), [150, 20]);
    }

    #[tokio::test]
    async fn native_pipeline_done_waits_for_commit_and_accounting() {
        use crate::executor::stage_boundary::StageInvocation;
        for missing_output in [false, true] {
            let stage = AgentBuilder::new("chat-0001")
                .prompt(CHAT_A)
                .build()
                .unwrap();
            let (_sandbox, mut gremlin) = test_gremlin(stage);
            let spec = "anthropic/pipeline-done-barrier";
            install_test_backend(
                &ModelURI::parse(spec).unwrap().to_string(),
                Arc::new(NativeLoopBackend {
                    baselines: Mutex::new(vec![]),
                    locals: Mutex::new(vec![]),
                    inputs: None,
                }),
            );
            let mut node = chat_node(1, CHAT_A).await;
            let ExecutorStage::Agent { stage, model, .. } = &mut node else {
                unreachable!()
            };
            *model = Some(ModelURI::parse(spec).unwrap());
            if missing_output {
                stage
                    .outputs_map
                    .insert("missing".into(), "artifact://missing-output".into());
            }
            let invocation = StageInvocation::enter(gremlin.id.as_str(), node.name(), "agent");
            let (_guard, mut rx) = gremlins_magic_bus::subscribe_run(gremlin.id.as_str());
            let result = run_agent(&node, &mut gremlin, &invocation, "", "", None, None).await;
            assert_eq!(result.is_err(), missing_output);
            assert_eq!(
                gremlin.state.read_field("token_usage").unwrap()["prompt_tokens"],
                180
            );
            let mut done = 0;
            while let Ok(event) = rx.try_recv() {
                if let EventKind::Done { usage, .. } = event.kind() {
                    assert_eq!(event.corr_id(), &invocation.corr_id());
                    assert_eq!(usage.as_ref().unwrap().prompt_tokens, Some(150));
                    done += 1;
                }
            }
            assert_eq!(done, usize::from(!missing_output));
        }
    }

    #[tokio::test]
    async fn session_chat_native_failure_flushes_corrected_and_task_usage_before_error() {
        use crate::executor::stage_boundary::StageInvocation;
        let stage = AgentBuilder::new("chat-0001")
            .prompt(CHAT_A)
            .build()
            .unwrap();
        let (_sandbox, mut gremlin) = test_gremlin(stage);
        gremlin.state.patch(
            &[],
            serde_json::json!({"kind": "chat"}).as_object().unwrap(),
        );
        let spec = "anthropic/session_chat-failed-usage";
        install_test_backend(
            &ModelURI::parse(spec).unwrap().to_string(),
            Arc::new(SessionChatFailureBackend(SessionChatUsageBackend {
                baselines: Mutex::new(vec![]),
            })),
        );
        gremlin.model_override = Some(spec.into());
        let mut node = chat_node(1, CHAT_A).await;
        let ExecutorStage::Agent { model, .. } = &mut node else {
            unreachable!()
        };
        *model = Some(ModelURI::parse(spec).unwrap());
        let invocation = StageInvocation::enter(gremlin.id.as_str(), node.name(), "agent");
        let (_guard, mut rx) = gremlins_magic_bus::subscribe_run(gremlin.id.as_str());
        let error = run_agent(&node, &mut gremlin, &invocation, "", "", None, None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains(CAPTURED), "{error}");
        assert_eq!(
            gremlin
                .state
                .read_field("session_chat_outer_usage")
                .unwrap()["prompt_tokens"],
            6
        );
        assert_eq!(
            session_chat_native_usage(&gremlin).unwrap().prompt_tokens,
            Some(6)
        );
        assert_eq!(
            gremlin.state.read_field("token_usage").unwrap()["prompt_tokens"],
            106
        );
        while let Ok(event) = rx.try_recv() {
            assert!(!matches!(event.kind(), EventKind::Done { .. }));
        }
        // Reopening must recover the failed attempt without trusting the cache.
        gremlin.state.patch(
            &[],
            serde_json::json!({"session_chat_outer_usage": {"prompt_tokens": 999}})
                .as_object()
                .unwrap(),
        );
        gremlin.state = StateData::open("gr-test").unwrap();
        let retry = Arc::new(SessionChatUsageBackend {
            baselines: Mutex::new(vec![]),
        });
        install_test_backend(&ModelURI::parse(spec).unwrap().to_string(), retry.clone());
        let invocation = StageInvocation::enter(gremlin.id.as_str(), node.name(), "agent");
        run_agent(&node, &mut gremlin, &invocation, "", "", None, None)
            .await
            .unwrap();
        assert_eq!(*retry.baselines.lock().unwrap(), [6]);
        assert_eq!(
            gremlin
                .state
                .read_field("session_chat_outer_usage")
                .unwrap()["prompt_tokens"],
            12
        );
        assert_eq!(
            gremlin.state.read_field("token_usage").unwrap()["prompt_tokens"],
            212
        );
        let mut done = vec![];
        while let Ok(event) = rx.try_recv() {
            if let EventKind::Done { usage, .. } = event.kind() {
                done.push(usage.as_ref().unwrap().prompt_tokens.unwrap());
            }
        }
        assert_eq!(done, [12]);
    }

    #[derive(Default)]
    struct CaptureBackend(Mutex<Option<RunParams>>);

    #[async_trait::async_trait]
    impl Backend for CaptureBackend {
        async fn run(
            &self,
            params: RunParams,
            _interactive: InteractiveSession,
        ) -> Result<CompletedRun, ClientError> {
            *self.0.lock().unwrap() = Some(params);
            Err(ClientError::Runtime {
                message: CAPTURED.into(),
            })
        }

        fn model(&self) -> &str {
            ""
        }
    }

    pub(super) fn test_gremlin(stage: StageSpec) -> (Sandbox, Gremlin) {
        let sandbox = Sandbox::new();
        let default_model = "cmd/run-agent-old-default";
        let state_dir = config::state_root().join("gr-test-0001");
        std::fs::create_dir_all(state_dir.join("artifacts")).unwrap();
        let data = serde_json::json!({
            "id": "gr-test", "attempt": "test-0001", "stage": "starting",
            "model": default_model
        });
        state::write_state(&state_dir, data.as_object().unwrap()).unwrap();
        let gremlin = Gremlin {
            id: validate_gremlin_id("gr-test").unwrap(),
            definition: Box::new(StaticDefinition::new(
                "test".into(),
                PathBuf::from("test.yaml"),
                ModelURI::parse(default_model).unwrap(),
                Bootstrap::default(),
                vec![stage],
                None,
                vec![],
                serde_json::Value::Null,
                None,
            )),
            definition_path: None,
            model_override: None,
            workdir: None,
            project_root: sandbox.path().to_path_buf(),
            state: StateData::open("gr-test").unwrap(),
            env: HashMap::new(),
            client: Client::parse(default_model).unwrap(),
            loop_iter: "1".into(),
            stage_inputs: HashMap::new(),
            runtime_config: RuntimeConfig::snapshot(None),
            cancel_token: None,
            interactive_sessions: None,
            execution_inputs: Default::default(),
            clean_cmds: vec![],
            enclosing_task_models: None,
            enclosing_model: None,
            discovered_skills: None,
            extra_sandbox_roots: vec![],
            native_definition: None,
            accounting: Default::default(),
        };
        (sandbox, gremlin)
    }

    const CHAT_A: &str = "A: remember the cobalt lighthouse";
    const CHAT_B: &str = "B: investigate the interrupted amber bridge";
    const CHAT_C: &str = "C: continue with the violet observatory";

    fn signed_chat_reasoning() -> rig_core::completion::message::Reasoning {
        use rig_core::completion::message::{AssistantContent, Reasoning};
        let content = AssistantContent::Reasoning(Reasoning {
            text: "A: distinctive private reasoning".into(),
            redacted: false,
            native: None,
        })
        .with_native(serde_json::json!({
            "type": "thinking",
            "thinking": "A: distinctive private reasoning",
            "signature": "A-signed-reasoning-do-not-change"
        }));
        let AssistantContent::Reasoning(reasoning) = content else {
            unreachable!()
        };
        reasoning
    }

    #[derive(Default)]
    struct ChatCancellationBackend {
        requests: Mutex<Vec<(String, Vec<Message>)>>,
        started: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl Backend for ChatCancellationBackend {
        async fn run(
            &self,
            mut params: RunParams,
            _interactive: InteractiveSession,
        ) -> Result<CompletedRun, ClientError> {
            assert!(
                params.defer_done,
                "session_chat completion belongs to executor"
            );
            let conversation = &mut params.prepopulated_conversation;
            conversation.set_next_prompt(&params.prompt);
            let script = conversation
                .render_turn(false, params.system_prompt.clone())
                .await
                .unwrap();
            let mut messages = script.messages;
            messages.push(script.next_user_prompt);
            {
                let mut requests = self.requests.lock().unwrap();
                requests.push((params.prompt.clone(), messages));
            }
            if params.label == "chat-0001" {
                conversation.set_preserve_reasoning(true);
                conversation.finalize(
                    1,
                    "A: the cobalt lighthouse is recorded",
                    &[signed_chat_reasoning()],
                    None,
                );
                return Ok(CompletedRun {
                    exit_code: 0,
                    text_result: Some("A: the cobalt lighthouse is recorded".into()),
                    summary: None,
                    events: None,
                    cost_usd: None,
                    token_usage: None,
                    conversation: Some(params.prepopulated_conversation),
                });
            }
            if let Some(cancel_token) = &params.cancel_token {
                self.started.notify_one();
                cancel_token.cancelled().await;
                return Err(ClientError::Cancelled);
            }
            Err(ClientError::Runtime {
                message: CAPTURED.into(),
            })
        }

        fn model(&self) -> &str {
            ""
        }
    }

    pub(super) async fn chat_node(turn: usize, prompt: &str) -> ExecutorStage {
        let stage = AgentBuilder::new(format!("chat-{turn:04}"))
            .prompt(prompt)
            .build()
            .unwrap();
        let mut definition = StaticDefinition::new(
            "chat-test".into(),
            PathBuf::from("test.yaml"),
            ModelURI::parse("cmd/run-agent-chat-cancellation").unwrap(),
            Bootstrap::default(),
            vec![stage],
            None,
            vec![],
            serde_json::Value::Null,
            None,
        );
        crate::definition::GremlinDefinition::next_stage(&mut definition)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn native_chat_commit_failure_emits_no_done() {
        use crate::executor::stage_boundary::StageInvocation;
        let stage = AgentBuilder::new("chat-0001")
            .prompt(CHAT_A)
            .build()
            .unwrap();
        let (_sandbox, mut gremlin) = test_gremlin(stage);
        gremlin.state.patch(
            &[],
            serde_json::json!({"kind": "chat"}).as_object().unwrap(),
        );
        let spec = "anthropic/native-chat-commit-failure";
        let backend = Arc::new(SessionChatUsageBackend {
            baselines: Mutex::new(vec![]),
        });
        install_test_backend(&ModelURI::parse(spec).unwrap().to_string(), backend);
        gremlin.model_override = Some(spec.into());
        let mut node = chat_node(1, CHAT_A).await;
        let ExecutorStage::Agent { stage, model, .. } = &mut node else {
            unreachable!()
        };
        *model = Some(ModelURI::parse(spec).unwrap());
        stage
            .outputs_map
            .insert("missing".into(), "artifact://missing-output".into());
        let invocation = StageInvocation::enter(gremlin.id.as_str(), node.name(), "agent");
        let (_guard, mut rx) = gremlins_magic_bus::subscribe_run(gremlin.id.as_str());
        let result = run_agent(&node, &mut gremlin, &invocation, "", "", None, None).await;
        assert!(result.is_err(), "missing output must fail commit");
        while let Ok(event) = rx.try_recv() {
            assert!(!matches!(event.kind(), EventKind::Done { .. }));
        }
    }

    struct CmdUsageBackend;

    #[async_trait::async_trait]
    impl Backend for CmdUsageBackend {
        async fn run(
            &self,
            params: RunParams,
            _interactive: InteractiveSession,
        ) -> Result<CompletedRun, ClientError> {
            let usage = usage_baseline_from_state(Some(&serde_json::json!({
                "turns": 2, "prompt_tokens": 11, "completion_tokens": 7,
                "cached_input_tokens": 0, "reasoning_tokens": 3
            })));
            // A live snapshot must not be added again at completion.
            emit_corr_event(
                params.output_corr_id.as_ref().unwrap(),
                EventKind::TokenUsage {
                    id: params.gremlin_id.unwrap(),
                    usage: usage.clone(),
                    cost_usd: None,
                    last_prompt_tokens: Some(11),
                    last_cached_input_tokens: Some(0),
                },
            );
            Ok(CompletedRun {
                exit_code: 0,
                text_result: Some("cmd usage reply".into()),
                summary: None,
                events: None,
                cost_usd: None,
                token_usage: Some(usage),
                conversation: None,
            })
        }

        fn model(&self) -> &str {
            "cmd-usage"
        }
        fn name(&self) -> &'static str {
            "cmd"
        }
    }

    #[tokio::test]
    async fn session_chat_legacy_native_cmd_native_preserves_separate_balances() {
        use crate::executor::stage_boundary::StageInvocation;
        for legacy_field in ["session_chat_outer_usage", "token_usage"] {
            let stage = AgentBuilder::new("chat-0001")
                .prompt(CHAT_A)
                .build()
                .unwrap();
            let (_sandbox, mut gremlin) = test_gremlin(stage);
            let mut fields =
                serde_json::json!({"kind": "chat", "token_usage": {"prompt_tokens": 900}});
            fields[legacy_field] = serde_json::json!({"prompt_tokens": 40});
            gremlin.state.patch(&[], fields.as_object().unwrap());
            let spec = format!("anthropic/session_chat-transition-{legacy_field}");
            let backend = Arc::new(SessionChatUsageBackend {
                baselines: Mutex::new(vec![]),
            });
            install_test_backend(
                &ModelURI::parse(&spec).unwrap().to_string(),
                backend.clone(),
            );
            let cmd = format!("cmd/session_chat-transition-{legacy_field}");
            install_test_backend(&cmd, Arc::new(CmdUsageBackend));
            let (_guard, mut rx) = gremlins_magic_bus::subscribe_run(gremlin.id.as_str());
            for (turn, model) in [(1, spec.as_str()), (2, cmd.as_str()), (3, spec.as_str())] {
                gremlin.model_override = Some(model.into());
                gremlin.client = Client::parse(model).unwrap();
                let node = chat_node(turn, CHAT_A).await;
                let invocation = StageInvocation::enter(gremlin.id.as_str(), node.name(), "agent");
                run_agent(&node, &mut gremlin, &invocation, "", "", None, None)
                    .await
                    .unwrap();
                if turn == 2 {
                    persist_chat_conversation_turn(&gremlin, turn, &[Message::user(CHAT_A)])
                        .await
                        .unwrap();
                }
            }
            assert_eq!(*backend.baselines.lock().unwrap(), [40, 57]);
            assert_eq!(
                gremlin
                    .state
                    .read_field("session_chat_starting_usage")
                    .unwrap()["prompt_tokens"],
                40
            );
            assert_eq!(
                gremlin.state.read_field("session_chat_cmd_usage").unwrap()["prompt_tokens"],
                11
            );
            assert_eq!(
                session_chat_native_usage(&gremlin).unwrap().prompt_tokens,
                Some(63)
            );
            let mut terminal = vec![];
            while let Ok(event) = rx.try_recv() {
                if let EventKind::Done { usage, .. } = event.kind() {
                    terminal.push(usage.as_ref().unwrap().prompt_tokens.unwrap());
                }
            }
            assert_eq!(terminal, [46, 57, 63]);
        }
    }

    #[test]
    fn session_chat_usage_migration_preserves_new_keys_and_is_idempotent() {
        let stage = AgentBuilder::new("chat-0001")
            .prompt(CHAT_A)
            .build()
            .unwrap();
        let (_sandbox, gremlin) = test_gremlin(stage);
        let mut fields = serde_json::Map::new();
        for suffix in ["starting_usage", "cmd_usage", "outer_usage"] {
            fields.insert(
                format!("home_{suffix}"),
                serde_json::json!({"prompt_tokens": 999}),
            );
            fields.insert(
                format!("session_chat_{suffix}"),
                serde_json::json!({"prompt_tokens": 7}),
            );
        }
        gremlin.state.patch(&[], &fields);
        migrate_session_chat_usage(&gremlin);
        migrate_session_chat_usage(&gremlin);
        for suffix in ["starting_usage", "cmd_usage", "outer_usage"] {
            assert!(gremlin
                .state
                .read_field(&format!("home_{suffix}"))
                .is_none());
            assert_eq!(
                gremlin
                    .state
                    .read_field(&format!("session_chat_{suffix}"))
                    .unwrap(),
                serde_json::json!({"prompt_tokens": 7})
            );
        }
    }

    #[tokio::test]
    async fn session_chat_migrates_home_balances_with_existing_native_journal() {
        use crate::executor::accounting::UsageRecord;
        use crate::executor::stage_boundary::StageInvocation;
        let stage = AgentBuilder::new("chat-0001")
            .prompt(CHAT_A)
            .build()
            .unwrap();
        let (_sandbox, mut gremlin) = test_gremlin(stage);
        gremlin.state.patch(
            &[],
            serde_json::json!({
                "kind": "chat",
                "token_usage": {"prompt_tokens": 900},
                "home_starting_usage": {"prompt_tokens": 40},
                "home_cmd_usage": {"prompt_tokens": 11},
                "home_outer_usage": {"prompt_tokens": 57}
            })
            .as_object()
            .unwrap(),
        );
        let record = UsageRecord {
            corr_id: gremlins_magic_bus::CorrId::new()
                .with_run(gremlin.id.as_str())
                .with_stage("chat-previous")
                .with_stage_run_id("chat-previous"),
            stage_path: "chat-previous".into(),
            usage: serde_json::from_value(serde_json::json!({
                "request_id": "previous", "provider": "test", "model": "test",
                "agent_id": "", "request_kind": "", "reported": true,
                "completed": true, "raw": {}, "usage": {"turns": 1, "prompt_tokens": 6}
            }))
            .unwrap(),
        };
        gremlin
            .state
            .store_ref()
            .native_accounting(Some(&record))
            .unwrap();
        assert!(gremlin.state.store_ref().exists("native_accounting.json"));
        assert_eq!(
            session_chat_native_usage(&gremlin).unwrap().prompt_tokens,
            Some(57)
        );
        for (suffix, tokens) in [
            ("starting_usage", 40),
            ("cmd_usage", 11),
            ("outer_usage", 57),
        ] {
            assert!(gremlin
                .state
                .read_field(&format!("home_{suffix}"))
                .is_none());
            assert_eq!(
                gremlin
                    .state
                    .read_field(&format!("session_chat_{suffix}"))
                    .unwrap()["prompt_tokens"],
                tokens
            );
        }
        let spec = "anthropic/session_chat-home-migration";
        let backend = Arc::new(SessionChatUsageBackend {
            baselines: Mutex::new(vec![]),
        });
        install_test_backend(&ModelURI::parse(spec).unwrap().to_string(), backend.clone());
        let cmd = "cmd/session_chat-home-migration";
        install_test_backend(cmd, Arc::new(CmdUsageBackend));
        let (_guard, mut rx) = gremlins_magic_bus::subscribe_run(gremlin.id.as_str());
        for (turn, model) in [(1, spec), (2, cmd), (3, spec)] {
            gremlin.model_override = Some(model.into());
            gremlin.client = Client::parse(model).unwrap();
            let node = chat_node(turn, CHAT_A).await;
            let invocation = StageInvocation::enter(gremlin.id.as_str(), node.name(), "agent");
            run_agent(&node, &mut gremlin, &invocation, "", "", None, None)
                .await
                .unwrap();
            if turn == 2 {
                persist_chat_conversation_turn(&gremlin, turn, &[Message::user(CHAT_A)])
                    .await
                    .unwrap();
            }
        }
        assert_eq!(*backend.baselines.lock().unwrap(), [57, 74]);
        assert_eq!(
            gremlin.state.read_field("session_chat_cmd_usage").unwrap()["prompt_tokens"],
            22
        );
        let mut terminal = vec![];
        while let Ok(event) = rx.try_recv() {
            if let EventKind::Done { usage, .. } = event.kind() {
                terminal.push(usage.as_ref().unwrap().prompt_tokens.unwrap());
            }
        }
        assert_eq!(terminal, [63, 74, 80]);
        assert_eq!(
            session_chat_native_usage(&gremlin).unwrap().prompt_tokens,
            Some(80)
        );
    }

    #[tokio::test]
    async fn session_chat_terminal_excludes_explicit_compaction_and_ledger_blocks_legacy_import() {
        use crate::executor::accounting::UsageRecord;
        use crate::executor::stage_boundary::StageInvocation;
        let stage = AgentBuilder::new("chat-0001")
            .prompt(CHAT_A)
            .build()
            .unwrap();
        let (_sandbox, mut gremlin) = test_gremlin(stage);
        gremlin.state.patch(
            &[],
            serde_json::json!({"kind": "chat", "session_chat_outer_usage": {"prompt_tokens": 900}})
                .as_object()
                .unwrap(),
        );
        let corr = gremlins_magic_bus::CorrId::new()
            .with_run(gremlin.id.as_str())
            .with_stage("compact")
            .with_stage_run_id("compact");
        let record = UsageRecord {
            corr_id: corr,
            stage_path: "compact".into(),
            usage: serde_json::from_value(serde_json::json!({
                "request_id": "compact", "provider": "test", "model": "test",
                "agent_id": "", "request_kind": "compaction", "reported": true,
                "completed": true, "raw": {}, "usage": {"turns": 1, "prompt_tokens": 500}
            }))
            .unwrap(),
        };
        gremlin
            .state
            .store_ref()
            .native_accounting(Some(&record))
            .unwrap();
        let spec = "anthropic/session_chat-compaction-terminal";
        install_test_backend(
            &ModelURI::parse(spec).unwrap().to_string(),
            Arc::new(SessionChatUsageBackend {
                baselines: Mutex::new(vec![]),
            }),
        );
        gremlin.model_override = Some(spec.into());
        let mut node = chat_node(1, CHAT_A).await;
        let ExecutorStage::Agent { model, .. } = &mut node else {
            unreachable!()
        };
        *model = Some(ModelURI::parse(spec).unwrap());
        let invocation = StageInvocation::enter(gremlin.id.as_str(), node.name(), "agent");
        let (_guard, mut rx) = gremlins_magic_bus::subscribe_run(gremlin.id.as_str());
        run_agent(&node, &mut gremlin, &invocation, "", "", None, None)
            .await
            .unwrap();
        let mut terminal = vec![];
        while let Ok(event) = rx.try_recv() {
            if let EventKind::Done { usage, .. } = event.kind() {
                terminal.push(usage.as_ref().unwrap().prompt_tokens.unwrap());
            }
        }
        assert_eq!(terminal, [6]);
        assert_eq!(
            gremlin.state.read_field("token_usage").unwrap()["prompt_tokens"],
            606
        );
    }

    #[tokio::test]
    async fn cmd_completed_usage_records_one_stage_row_and_one_run_increment() {
        use crate::executor::stage_boundary::StageInvocation;
        let stage = AgentBuilder::new("chat-0001")
            .prompt(CHAT_A)
            .build()
            .unwrap();
        let (_sandbox, mut gremlin) = test_gremlin(stage);
        let prior = serde_json::json!({"turns": 4, "prompt_tokens": 100, "completion_tokens": 20});
        gremlin.state.patch(
            &[],
            serde_json::json!({"kind": "chat", "token_usage": prior, "session_chat_outer_usage": prior})
                .as_object()
                .unwrap(),
        );
        let spec = "cmd/completed-usage-test";
        install_test_backend(spec, Arc::new(CmdUsageBackend));
        gremlin.model_override = Some(spec.into());
        gremlin.client = Client::parse(spec).unwrap();
        let node = chat_node(1, CHAT_A).await;
        let invocation = StageInvocation::enter(gremlin.id.as_str(), node.name(), "agent");
        let (_guard, mut rx) = gremlins_magic_bus::subscribe_run(gremlin.id.as_str());
        run_agent(&node, &mut gremlin, &invocation, "", "nested", None, None)
            .await
            .unwrap();
        let expected = serde_json::json!({"turns": 6, "prompt_tokens": 111, "completion_tokens": 27, "cached_input_tokens": 0, "reasoning_tokens": 3});
        assert_eq!(gremlin.state.read_field("token_usage").unwrap(), expected);
        let mut rows_blob = gremlin
            .state
            .store_ref()
            .open("token_usage.jsonl", state::BlobMode::Read)
            .unwrap();
        let mut rows = String::new();
        std::io::Read::read_to_string(&mut rows_blob, &mut rows).unwrap();
        let rows: Vec<serde_json::Value> = rows
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            rows,
            [
                serde_json::json!({"stage": "nested.chat-0001", "turns": 2, "prompt_tokens": 11, "completion_tokens": 7, "cached_input_tokens": 0, "reasoning_tokens": 3})
            ]
        );
        let mut done = 0;
        while let Ok(event) = rx.try_recv() {
            if let EventKind::Done { usage, text, .. } = event.kind() {
                assert_eq!(event.corr_id(), &invocation.corr_id());
                assert_eq!(text, "cmd usage reply");
                assert_eq!(serde_json::to_value(usage).unwrap(), expected);
                done += 1;
            }
        }
        assert_eq!(done, 1);
    }

    struct FailedCmdUsageBackend {
        snapshots: bool,
        cancelled: bool,
    }

    #[async_trait::async_trait]
    impl Backend for FailedCmdUsageBackend {
        async fn run(
            &self,
            params: RunParams,
            _interactive: InteractiveSession,
        ) -> Result<CompletedRun, ClientError> {
            if self.snapshots {
                // These are cumulative snapshots, not independent deltas.
                for (turns, prompt, completion, reasoning) in [(1, 5, 2, 1), (2, 11, 7, 3)] {
                    let usage = usage_baseline_from_state(Some(&serde_json::json!({
                        "turns": turns, "prompt_tokens": prompt,
                        "completion_tokens": completion, "cached_input_tokens": 0,
                        "reasoning_tokens": reasoning
                    })));
                    emit_corr_event(
                        params.output_corr_id.as_ref().unwrap(),
                        EventKind::TokenUsage {
                            id: params.gremlin_id.clone().unwrap(),
                            usage,
                            cost_usd: None,
                            last_prompt_tokens: Some(prompt),
                            last_cached_input_tokens: Some(0),
                        },
                    );
                }
            }
            if self.cancelled {
                Err(ClientError::Cancelled)
            } else {
                Err(ClientError::Runtime {
                    message: "command failed after partial usage".into(),
                })
            }
        }

        fn model(&self) -> &str {
            "failed-cmd-usage"
        }

        fn name(&self) -> &'static str {
            "cmd"
        }
    }

    async fn assert_failed_cmd_usage(snapshots: bool, cancelled: bool) {
        use crate::executor::stage_boundary::StageInvocation;
        let stage = AgentBuilder::new("chat-0001")
            .prompt(CHAT_A)
            .build()
            .unwrap();
        let (_sandbox, mut gremlin) = test_gremlin(stage);
        let prior_run = serde_json::json!({
            "turns": 4, "prompt_tokens": 100, "completion_tokens": 20
        });
        let prior_cmd = serde_json::json!({
            "turns": 3, "prompt_tokens": 30, "completion_tokens": 9
        });
        let prior_outer = serde_json::json!({
            "turns": 8, "prompt_tokens": 200, "completion_tokens": 40,
            "cached_input_tokens": 12
        });
        gremlin.state.patch(
            &[],
            serde_json::json!({
                "kind": "chat", "token_usage": prior_run,
                "session_chat_cmd_usage": prior_cmd, "session_chat_outer_usage": prior_outer
            })
            .as_object()
            .unwrap(),
        );
        let spec = format!("cmd/failed-usage-{snapshots}-{cancelled}");
        install_test_backend(
            &spec,
            Arc::new(FailedCmdUsageBackend {
                snapshots,
                cancelled,
            }),
        );
        gremlin.model_override = Some(spec.clone());
        gremlin.client = Client::parse(&spec).unwrap();
        let node = chat_node(1, CHAT_A).await;
        let invocation = StageInvocation::enter(gremlin.id.as_str(), node.name(), "agent");
        let (_guard, mut rx) = gremlins_magic_bus::subscribe_run(gremlin.id.as_str());
        let error = run_agent(&node, &mut gremlin, &invocation, "", "nested", None, None)
            .await
            .unwrap_err();
        if cancelled {
            assert!(matches!(error, RunError::Cancelled), "{error}");
        } else {
            assert!(
                error
                    .to_string()
                    .contains("command failed after partial usage"),
                "{error}"
            );
        }
        assert_eq!(
            gremlin
                .state
                .read_field("session_chat_outer_usage")
                .unwrap(),
            prior_outer
        );
        let expected_run = if snapshots {
            serde_json::json!({
                "turns": 6, "prompt_tokens": 111, "completion_tokens": 27,
                "cached_input_tokens": 0, "reasoning_tokens": 3
            })
        } else {
            prior_run
        };
        let expected_cmd = if snapshots {
            serde_json::json!({
                "turns": 5, "prompt_tokens": 41, "completion_tokens": 16,
                "cached_input_tokens": 0, "reasoning_tokens": 3
            })
        } else {
            prior_cmd
        };
        assert_eq!(
            gremlin.state.read_field("token_usage").unwrap(),
            expected_run
        );
        assert_eq!(
            gremlin.state.read_field("session_chat_cmd_usage").unwrap(),
            expected_cmd
        );
        let rows_blob = gremlin
            .state
            .store_ref()
            .open("token_usage.jsonl", state::BlobMode::Read);
        if snapshots {
            let mut rows_blob = rows_blob.unwrap();
            let mut rows = String::new();
            std::io::Read::read_to_string(&mut rows_blob, &mut rows).unwrap();
            let rows: Vec<serde_json::Value> = rows
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(
                rows,
                [serde_json::json!({
                    "stage": "nested.chat-0001", "turns": 2,
                    "prompt_tokens": 11, "completion_tokens": 7,
                    "cached_input_tokens": 0, "reasoning_tokens": 3
                })]
            );
        } else {
            match rows_blob {
                Err(state::StateError::Io(error)) => assert_eq!(
                    error.kind(),
                    std::io::ErrorKind::NotFound,
                    "unexpected error opening usage ledger: {error}"
                ),
                Err(error) => panic!("unexpected error opening usage ledger: {error}"),
                Ok(_) => panic!("no snapshot must not create a usage ledger"),
            }
        }
        let mut seen_snapshots = 0;
        loop {
            match rx.try_recv() {
                Ok(event) => match event.kind() {
                    EventKind::Done { .. } => panic!("failed command emitted Done"),
                    EventKind::TokenUsage { .. } => seen_snapshots += 1,
                    _ => {}
                },
                Err(gremlins_magic_bus::TryRecvError::Empty) => break,
                Err(error) => panic!("event subscription failed: {error}"),
            }
        }
        assert_eq!(seen_snapshots, if snapshots { 2 } else { 0 });
    }

    #[tokio::test]
    async fn cmd_failure_accumulates_only_last_usage_snapshot_once() {
        assert_failed_cmd_usage(true, false).await;
    }

    #[tokio::test]
    async fn cmd_failure_without_usage_preserves_balances_and_has_no_row() {
        assert_failed_cmd_usage(false, false).await;
    }

    #[tokio::test]
    async fn cmd_cancellation_accumulates_only_last_usage_snapshot_once() {
        assert_failed_cmd_usage(true, true).await;
    }

    #[tokio::test]
    async fn cmd_cancellation_without_usage_preserves_balances_and_has_no_row() {
        assert_failed_cmd_usage(false, true).await;
    }

    async fn assert_cmd_done_events(spec: &str, is_chat: bool, succeeds: bool) {
        use crate::executor::stage_boundary::StageInvocation;
        use gremlins_magic_bus::subscribe_run;

        let stage = AgentBuilder::new("chat-0001")
            .prompt("This prompt is not the command output")
            .build()
            .unwrap();
        let (_sandbox, mut gremlin) = test_gremlin(stage);
        gremlin.runtime_config.default_task_models = None;
        gremlin.runtime_config.resolved_model_selection = None;
        gremlin.client = Client::parse(spec).unwrap();
        gremlin.model_override = Some(spec.into());
        assert_eq!(gremlin.client.get_or_build_backend().unwrap().name(), "cmd");
        if is_chat {
            gremlin.state.patch(
                &[],
                serde_json::json!({"kind": "chat", "session_chat_outer_usage": {
                    "turns": 3, "prompt_tokens": 17, "completion_tokens": 0
                }})
                .as_object()
                .unwrap(),
            );
        }
        let node = chat_node(1, "This prompt is not the command output").await;
        let invocation = StageInvocation::enter(gremlin.id.as_str(), node.name(), "agent");
        let corr_id = invocation.corr_id();
        let (sub, mut rx) = subscribe_run(gremlin.id.as_str());
        let _guard = gremlins_magic_bus::connection::BusSubGuard::new(sub);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            run_agent(&node, &mut gremlin, &invocation, "", "", None, None),
        )
        .await
        .expect("command stage timed out");
        if succeeds {
            result.unwrap();
        } else {
            let error = result.unwrap_err().to_string();
            assert!(error.contains("exited 7"), "{error}");
        }

        assert!(
            gremlin.state.read_field("token_usage").is_none(),
            "plain commands must not invent model usage"
        );
        if is_chat {
            assert_eq!(
                gremlin
                    .state
                    .read_field("session_chat_outer_usage")
                    .unwrap()["prompt_tokens"],
                17
            );
        }
        let mut done = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(event) => {
                    if let EventKind::Done {
                        text,
                        summary,
                        usage,
                    } = event.kind()
                    {
                        let usage = usage.as_ref().expect("prior chat usage is retained");
                        assert_eq!(usage.turns, 3);
                        assert_eq!(usage.prompt_tokens, Some(17));
                        assert_eq!(usage.completion_tokens, Some(0));
                        assert_eq!(event.corr_id(), &corr_id);
                        assert!(summary.is_empty(), "command output must not be duplicated");
                        done.push(text.clone());
                    }
                }
                Err(gremlins_magic_bus::TryRecvError::Empty) => break,
                Err(error) => panic!("event subscription failed: {error}"),
            }
        }
        if is_chat && succeeds {
            assert_eq!(done, ["actual command reply"]);
        } else {
            assert!(done.is_empty(), "unexpected Done events: {done:?}");
        }
    }

    #[tokio::test]
    async fn cmd_chat_success_emits_one_done_with_output_and_invocation() {
        assert_cmd_done_events("cmd/sh -c 'printf actual\\ command\\ reply'", true, true).await;
    }

    #[tokio::test]
    async fn cmd_chat_failure_emits_no_done() {
        assert_cmd_done_events(
            "cmd/sh -c 'printf failed-command-output; exit 7'",
            true,
            false,
        )
        .await;
    }

    #[tokio::test]
    async fn cmd_pipeline_success_emits_no_done() {
        assert_cmd_done_events("cmd/sh -c 'printf actual\\ command\\ reply'", false, true).await;
    }

    async fn assert_cancelled_chat_recovery(retry: bool) {
        let stage = AgentBuilder::new("chat-0001")
            .prompt(CHAT_A)
            .build()
            .unwrap();
        let (_sandbox, mut gremlin) = test_gremlin(stage);
        gremlin.runtime_config.default_task_models = None;
        gremlin.runtime_config.resolved_model_selection = None;
        let spec = if retry {
            "cmd/chat-cancel-retry"
        } else {
            "cmd/chat-cancel-next"
        };
        gremlin.client = Client::parse(spec).unwrap();
        gremlin.model_override = Some(spec.into());
        gremlin.state.patch(
            &[],
            serde_json::json!({"kind": "chat"}).as_object().unwrap(),
        );
        let backend = Arc::new(ChatCancellationBackend::default());
        install_test_backend(spec, backend.clone());
        let a = chat_node(1, CHAT_A).await;
        let invocation = crate::executor::stage_boundary::StageInvocation::enter(
            gremlin.id.as_str(),
            a.name(),
            "agent",
        );
        run_agent(&a, &mut gremlin, &invocation, "", "", None, None)
            .await
            .unwrap();
        let a_json = gremlin
            .state
            .content(&conversation_uri(1), None)
            .await
            .unwrap();
        let a_messages = messages_from_conversation(&a_json).unwrap();
        assert_eq!(a_messages.len(), 2);
        let a_prompt = backend.requests.lock().unwrap()[0].0.clone();
        assert_eq!(
            serde_json::to_value(&a_messages[0]).unwrap(),
            serde_json::to_value(Message::user(&a_prompt)).unwrap()
        );
        assert!(a_prompt.ends_with(CHAT_A));
        assert!(a_json.contains("A-signed-reasoning-do-not-change"));
        assert!(a_json.contains("A: the cobalt lighthouse is recorded"));

        let b = chat_node(2, CHAT_B).await;
        let token = crate::clients::cancel::CancelToken::new();
        gremlin.cancel_token = Some(token.clone());
        let cancel_after_request = async {
            backend.started.notified().await;
            token.cancel();
        };
        let invocation = crate::executor::stage_boundary::StageInvocation::enter(
            gremlin.id.as_str(),
            b.name(),
            "agent",
        );
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(
                run_agent(&b, &mut gremlin, &invocation, "", "", None, None),
                cancel_after_request
            )
        })
        .await
        .expect("backend must start and observe cancellation");
        assert!(matches!(result, Err(RunError::Cancelled)), "{result:?}");
        let b_prompt = backend.requests.lock().unwrap()[1].0.clone();
        assert!(b_prompt.ends_with(CHAT_B));
        let b_json = gremlin
            .state
            .content(&conversation_uri(2), None)
            .await
            .unwrap();
        assert_eq!(
            gremlin
                .state
                .content(&conversation_uri(1), None)
                .await
                .unwrap(),
            a_json
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&b_json).unwrap(),
            serde_json::to_value([Message::user(&b_prompt)]).unwrap()
        );

        gremlin.cancel_token = None;
        let next = if retry {
            chat_node(2, CHAT_B).await
        } else {
            chat_node(3, CHAT_C).await
        };
        let invocation = crate::executor::stage_boundary::StageInvocation::enter(
            gremlin.id.as_str(),
            next.name(),
            "agent",
        );
        let error = run_agent(&next, &mut gremlin, &invocation, "", "", None, None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains(CAPTURED), "{error}");
        let requests = backend.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 3);
        assert!(requests[2].0.ends_with(if retry { CHAT_B } else { CHAT_C }));
        let mut expected = a_messages;
        expected.push(Message::user(&b_prompt));
        expected.push(Message::user(&requests[2].0));
        assert_eq!(
            serde_json::to_value(&requests[2].1).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        assert_eq!(
            gremlin
                .state
                .content(&conversation_uri(1), None)
                .await
                .unwrap(),
            a_json
        );
        assert_eq!(
            gremlin
                .state
                .content(&conversation_uri(2), None)
                .await
                .unwrap(),
            b_json
        );
    }

    #[tokio::test]
    async fn cancelled_chat_next_turn_replays_each_message_once() {
        assert_cancelled_chat_recovery(false).await;
    }

    #[tokio::test]
    async fn cancelled_chat_retry_preserves_signed_history_and_turn_local_artifact() {
        assert_cancelled_chat_recovery(true).await;
    }

    async fn capture_dispatch(name: &str, replacement: bool, expected: &str) -> RunParams {
        let stage = AgentBuilder::new(name)
            .prompt("hi")
            .option("model", "old-prepared-model")
            .build()
            .unwrap();
        let (_sandbox, mut gremlin) = test_gremlin(stage);
        gremlin.runtime_config.default_task_models = None;
        gremlin.runtime_config.resolved_model_selection =
            replacement.then(|| ResolvedModelSelection {
                requested: "profile/replacement".into(),
                default_model: "cmd/run-agent-new-default".into(),
                stage_models_exact: [("review-exact".into(), "cmd/run-agent-new-exact".into())]
                    .into(),
                stage_models_prefix: [("review-".into(), "cmd/run-agent-new-prefix".into())].into(),
                task_models: None,
            });
        let backend = Arc::new(CaptureBackend::default());
        let spec = if replacement {
            format!("cmd/{expected}")
        } else {
            "cmd/run-agent-old-default".into()
        };
        install_test_backend(&spec, backend.clone());
        let node = gremlin.definition.next_stage().await.unwrap();
        let invocation = crate::executor::stage_boundary::StageInvocation::enter(
            gremlin.id.as_str(),
            node.name(),
            node.stage_type(),
        );
        let error = run_agent(&node, &mut gremlin, &invocation, "", "", None, None)
            .await
            .expect_err("capture backend deliberately fails");
        assert!(error.to_string().contains(CAPTURED), "{error}");
        let params = backend
            .0
            .lock()
            .unwrap()
            .take()
            .expect("backend was invoked");
        assert_eq!(params.model.as_deref(), Some(expected));
        params
    }

    async fn assert_replacement_dispatch(name: &str, expected: &str) {
        let params = capture_dispatch(name, true, expected).await;
        let selector = params
            .task_models
            .expect("replacement installs a selector even without task overrides");
        let parent = TaskModel {
            model: rig_core::test_utils::MockCompletionModel::from_turns([]).erase(),
            backend: None,
            temperature: None,
            max_tokens: None,
            extra: None,
            preserve_reasoning: None,
        };
        // A cmd fallback cannot build a task model; its error exposes the selected full spec.
        let error = selector
            .model_for(TaskKind::Implement, &parent)
            .await
            .unwrap_err();
        assert!(error.contains(&format!("cmd/{expected}")), "{error}");
    }

    #[tokio::test]
    async fn replacement_default_overrides_prepared_model_at_dispatch() {
        assert_replacement_dispatch("writer", "run-agent-new-default").await;
    }

    #[tokio::test]
    async fn replacement_exact_overrides_prepared_model_at_dispatch() {
        assert_replacement_dispatch("review-exact", "run-agent-new-exact").await;
    }

    #[tokio::test]
    async fn replacement_prefix_overrides_prepared_model_at_dispatch() {
        assert_replacement_dispatch("review-leaf", "run-agent-new-prefix").await;
    }

    #[tokio::test]
    async fn no_replacement_preserves_prepared_model_at_dispatch() {
        let params = capture_dispatch("writer", false, "old-prepared-model").await;
        assert!(params.task_models.is_none());
    }
}
