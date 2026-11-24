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
    let accepts_commands = backend.accepts_interactive_commands();
    // Chat input keeps its prepared session identity, independently of output correlation.
    let session = if is_chat {
        gremlin.next_interactive_session_with_commands(accepts_commands)
    } else {
        gremlin.next_pipeline_interactive_session(&invocation.stage_run_id, accepts_commands)
    };
    let interactive = match session {
        Ok(session) => session,
        Err(error) => return Err(stage_failed(&agent.name, error)),
    };
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

    let params = RunParams {
        output_corr_id: Some(corr_id.clone()),
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

    let run_result = client.run(params, interactive).await;
    // Pipeline retries redispatch a fresh invocation via run_stage, not legacy resume().
    client.release_invocation(&corr_id);
    // Close our subscription so the prompt-dump task drains any
    // already-buffered events and then exits.
    drop(prompt_dumper_guard);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), prompt_dumper_task).await;

    let completed = match run_result {
        Ok(c) => c,
        Err(error) => {
            // Persist partial token usage from failed agent-loop turns so
            // the Gremlin::finish() snapshot has accurate totals rather
            // than the previous stage's stale values.
            if let Some(usage) = stage_usage.lock().unwrap().take() {
                let mut delta: HashMap<String, i64> = HashMap::new();
                delta.insert("turns".to_string(), usage.turns as i64);
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
                    .filter_map(|(k, v)| v.map(|v| (k.to_string(), v as i64))),
                );
                gremlin.state.accumulate_token_usage(&delta);
                state::emit_stage_token_usage(gremlin.state.store_ref(), &stage_path, &delta);
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

    if let Some(usage) = &completed.token_usage {
        let mut delta: HashMap<String, i64> = HashMap::new();
        delta.insert("turns".to_string(), usage.turns as i64);
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
            .filter_map(|(k, v)| v.map(|v| (k.to_string(), v as i64))),
        );
        gremlin.state.accumulate_token_usage(&delta);
        state::emit_stage_token_usage(gremlin.state.store_ref(), &stage_path, &delta);
    }

    if is_chat {
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

    Ok(())
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
            request_context: Option<ResolvedRequestContext>,
        },
    }

    match serde_json::from_str(json)
        .map_err(|e| format!("chat: failed to parse conversation artifact: {e}"))?
    {
        ConversationArtifact::Legacy(messages) => Ok((messages, None)),
        ConversationArtifact::Envelope {
            messages,
            request_context,
        } => Ok((messages, request_context)),
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

    fn test_gremlin(stage: StageSpec) -> (Sandbox, Gremlin) {
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
            clean_cmds: vec![],
            enclosing_task_models: None,
            enclosing_model: None,
            discovered_skills: None,
            extra_sandbox_roots: vec![],
            native_definition: None,
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

    async fn chat_node(turn: usize, prompt: &str) -> ExecutorStage {
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
        run_agent(&a, &mut gremlin, "", "", None, None)
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
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(
                run_agent(&b, &mut gremlin, "", "", None, None),
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
        let error = run_agent(&next, &mut gremlin, "", "", None, None)
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
