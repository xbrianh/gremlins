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
use crate::clients::backend::RunParams;
use crate::clients::client::ModelURI;
use crate::config;
use crate::definition::ExecutorStage;
use crate::executor::agent_runner::{commit_agent, prepare_agent};
use crate::executor::gremlin::Gremlin;
use crate::executor::state;
use crate::executor::state::{Checkout, Collision, FileSystemStateStore};
use crate::executor::supervisor::{emit_event, get_run_map};
use crate::executor::EventKind;
use crate::executor::RunError;
use crate::task_kind::TaskKind;
use indexmap::IndexMap;
use rig_core::completion::message::{AssistantContent, UserContent};
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

    let task_models_by_kind = resolve_task_models(
        gremlin.runtime_config.default_task_models.as_ref(),
        enclosing_task_models,
        stage_task_models.as_ref(),
    )
    .map_err(|e| stage_failed(&agent.name, e))?;

    let client = resolve_client(node, gremlin, enclosing_model)?;
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

    let is_chat = gremlin.state.read_str("kind") == "chat";

    // Ensure the checkout artifact directory exists so the agent's
    // local state store can create files under it.
    std::fs::create_dir_all(&artifact_dir)?;

    let system_prompt = prepared.system_prompt();

    let stage_env = gremlin.env.clone();

    let task_models = crate::clients::agent::task::build_task_selector(
        std::sync::Arc::new(crate::clients::client::resolve_model),
        &task_models_by_kind,
    );

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
                )))
            }
        }

        (!all_msgs.is_empty()).then_some(all_msgs)
    } else {
        None
    };

    let backend = match client.get_or_build_backend() {
        Ok(b) => Some(b),
        Err(e) => {
            log::warn!("run_agent: failed to build backend: {e}");
            None
        }
    };
    let conv = crate::conversation::standard::StandardConversation::new(
        "",
        false,
        verbatim_history,
        backend,
    );

    // ── Live usage tracker for failure persistence ─────
    // Stores (combined_usage, stage_local_usage, cost_usd).
    // `combined_usage` = baseline + current stage (emitted to live UI).
    // `stage_local_usage` = just the current stage's delta (used on
    // failure to avoid double-counting the baseline).
    let live_usage = std::sync::Arc::new(std::sync::Mutex::new(
        None::<(
            crate::clients::protocol::UsageStats,
            crate::clients::protocol::UsageStats,
            Option<f64>,
        )>,
    ));

    // ── Build inversion-of-control hooks ─────
    // Wires cross-package dependencies (log, prompt-debug,
    // token-usage) into the agent loop without the loop importing
    // `crate::executor` or `crate::landscape`.
    let mut hooks = crate::clients::agent::plug::LoopHooks::new();
    {
        let store = gremlin.state.store_handle();
        hooks.register_prompt_debug(move |request| {
            if crate::executor::log_level_threshold() < log::LevelFilter::Debug {
                return;
            }
            let ts = crate::executor::state::now_stamp_millis();
            let ts_clean = ts
                .trim_matches(|c: char| c == '[' || c == ']')
                .replace(':', "-");
            let name = format!("prompt_{ts_clean}.txt");
            match store.open(&name, crate::executor::state::BlobMode::Write) {
                Ok(mut blob) => {
                    let json = serde_json::to_string_pretty(request).unwrap_or_default();
                    let _ = std::io::Write::write_all(&mut blob, json.as_bytes());
                    let _ = std::io::Write::write_all(&mut blob, b"\n");
                }
                Err(e) => {
                    log::warn!("dump_prompt_if_debug: failed to open {name}: {e}");
                }
            }
        });
    }
    {
        let gremlin_id = gremlin.id.to_string();
        let client_for_cost = client.clone();
        let baseline_usage = crate::executor::supervisor::usage_stats_from_state(
            gremlin.state.read_field("token_usage").as_ref(),
        );
        let live_usage_tracker = live_usage.clone();
        hooks.register_token_usage(
            move |totals, last_prompt_tokens, last_cached_input_tokens| {
                let usage = crate::executor::supervisor::add_usage_stats(&baseline_usage, &totals);
                let cost_usd = client_for_cost.total_cost_usd();
                *live_usage_tracker.lock().unwrap() =
                    Some((usage.clone(), totals.clone(), cost_usd));
                crate::executor::supervisor::emit_event(EventKind::TokenUsage {
                    id: gremlin_id.clone(),
                    usage,
                    cost_usd,
                    last_prompt_tokens,
                    last_cached_input_tokens,
                });
            },
        );
    }
    {
        let gremlin_id = gremlin.id.to_string();
        hooks.register_log_line(move |msg: &str| {
            if log::Level::Info <= crate::executor::log_level_threshold() {
                crate::executor::supervisor::emit_event(EventKind::LogLine {
                    id: gremlin_id.clone(),
                    line: msg.to_string(),
                });
            }
        });
    }
    let prepopulated_conversation: Box<dyn crate::clients::conversation::Conversation> =
        Box::new(conv);

    let params = RunParams {
        prompt: prepared.user_prompt(),
        label: prepared.name.clone(),
        model: prepared
            .model
            .clone()
            .or_else(|| Some(client.model().to_string())),
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
        expected_artifact_paths: prepared
            .expected_artifact_paths
            .iter()
            .map(PathBuf::from)
            .collect(),
        system_prompt: Some(system_prompt),
        gremlin_id: Some(gremlin.id.to_string()),
        base_env: Some(gremlin.env.clone()),
        allow_all: prepared.allow_all,
        task_models,
        cancel_token: gremlin.cancel_token.clone(),
        stream_events: gremlin.runtime_config.stream_events.clone(),
        prepopulated_conversation,
        skills_available: gremlin.discovered_skills.is_some(),
        discovered_skills: gremlin.discovered_skills.clone(),
        active_skills: gremlin.active_skills.clone(),
        token_usage_sink: crate::clients::token_usage::TokenUsageSink(None),
        extra_sandbox_roots: gremlin.extra_sandbox_roots.clone(),
        hooks: Some(hooks),
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
    emit_event(EventKind::LogLine {
        id: gremlin.id.to_string(),
        line: msg,
    });

    // Interactive session: take the pre-created handle or create a fresh one.
    let interactive = gremlin.interactive_session.take().or_else(|| {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let interactive = gremlin.runtime_config.interactive.as_ref();
        let pause = interactive.map(|h| h.pause.clone());
        if let Ok(mut map) = get_run_map().lock() {
            if let Some(handle) = map.get_mut(gremlin.id.as_str()) {
                handle.interactive.cmd_tx = tx;
            }
        }
        let pause = pause?;
        interactive.map(|h| crate::clients::interactive::InteractiveSession {
            cmd_rx: rx,
            evt_tx: h.evt_tx.clone(),
            pause,
        })
    });

    log::debug!(
        "agent stage '{}' (gremlin={}): invoking client.run (model={})",
        prepared.name,
        gremlin.id.as_str(),
        params.model.as_deref().unwrap_or("default")
    );

    let completed = match client.run(params, interactive).await {
        Ok(c) => c,
        Err(error) => {
            // Persist the live usage counters from partial (failed) agent-loop
            // turns so the Gremlin::finish() snapshot has accurate token
            // totals rather than the previous stage's stale values.
            if let Some((_combined, stage_local, _cost_usd)) = live_usage.lock().unwrap().take() {
                let mut delta: HashMap<String, i64> = HashMap::new();
                delta.insert("turns".to_string(), stage_local.turns as i64);
                delta.extend(
                    [
                        ("prompt_tokens", stage_local.prompt_tokens),
                        ("completion_tokens", stage_local.completion_tokens),
                        ("cached_input_tokens", stage_local.cached_input_tokens),
                        (
                            "cache_creation_input_tokens",
                            stage_local.cache_creation_input_tokens,
                        ),
                        ("reasoning_tokens", stage_local.reasoning_tokens),
                    ]
                    .into_iter()
                    .filter_map(|(k, v)| v.map(|v| (k.to_string(), v as i64))),
                );
                gremlin.state.accumulate_token_usage(&delta);
                state::emit_stage_token_usage(gremlin.state.store_ref(), &stage_path, &delta);
            }
            return Err(client_run_err(error, &prepared.name));
        }
    };

    log::info!(
        "[{name}][{model}] agent: completed (turns={turns})",
        name = prepared.name,
        model = effective_model,
        turns = completed.token_usage.as_ref().map(|u| u.turns).unwrap_or(0)
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
            if let Err(e) = persist_chat_conversation_turn(gremlin, turn_num, &diff).await {
                log::warn!("chat: failed to persist conversation turn {turn_num}: {e}");
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

/// Deserialize a conversation artifact (JSON array of rig `Message` values).
pub(crate) fn messages_from_conversation(
    json: &str,
) -> Result<Vec<rig_core::completion::Message>, String> {
    serde_json::from_str::<Vec<rig_core::completion::Message>>(json)
        .map_err(|e| format!("chat: failed to parse conversation artifact: {e}"))
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
    let json = serde_json::to_string(messages).map_err(|e| {
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
pub(crate) async fn read_chat_conversation(
    gremlin: &Gremlin,
    max_turn: Option<usize>,
) -> Vec<Message> {
    let mut all_msgs = Vec::new();
    for t in 1.. {
        if max_turn.is_some_and(|max| t > max) {
            break;
        }
        let uri = conversation_uri(t);
        match gremlin.state.content(&uri, None).await {
            Ok(json) => match messages_from_conversation(&json) {
                Ok(msgs) => all_msgs.extend(msgs),
                Err(_) => {
                    log::warn!("chat: conversation artifact {uri} unparseable; stopping walk");
                    break;
                }
            },
            Err(e)
                if e.downcast_ref::<crate::executor::state::MissingArtifact>()
                    .is_some() =>
            {
                break
            }
            Err(_) => continue,
        }
    }
    all_msgs
}

/// Append messages to a per-turn conversation artifact.
pub(crate) async fn append_chat_conversation_turn(
    gremlin: &Gremlin,
    turn: usize,
    messages: &[Message],
) {
    if messages.is_empty() {
        return;
    }
    let uri_str = conversation_uri(turn);
    let mut existing = match gremlin.state.content(&uri_str, None).await {
        Ok(json) => {
            match messages_from_conversation(&json) {
                Ok(msgs) => msgs,
                Err(e) => {
                    log::warn!("chat: current-turn artifact {uri_str} unparseable; refusing to append: {e}");
                    return;
                }
            }
        }
        Err(e)
            if e.downcast_ref::<crate::executor::state::MissingArtifact>()
                .is_some() =>
        {
            Vec::new()
        }
        Err(e) => {
            log::warn!("chat: failed to read current-turn artifact for append: {e}");
            return;
        }
    };
    existing.extend_from_slice(messages);
    if let Err(e) = persist_chat_conversation_turn(gremlin, turn, &existing).await {
        log::warn!("chat: failed to persist current-turn artifact after append: {e}");
    }
}

/// Overwrite per-turn conversation artifacts from a compacted conversation.
///
/// Writes the full compacted history as turn 0001 and clears every other
/// registered `conversation/NNNN.json` artifact. The replacement write must
/// succeed before any later artifact is cleared — otherwise a failure would
/// destroy the only stored copies of those messages while still reporting
/// success.
pub(crate) async fn persist_compacted_conversation(
    gremlin: &Gremlin,
    messages: &[Message],
) -> Result<(), Box<dyn std::error::Error>> {
    // Save the compacted replacement first. Only after it is durably bound do
    // we consider clearing the artifacts it subsumed.
    persist_chat_conversation_turn(gremlin, 1, messages).await?;

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

/// Strip trailing messages left dangling by a mid-tool-call cancellation.
pub(crate) fn strip_unpaired_tool_messages(messages: &mut Vec<Message>) {
    fn count_tool_calls(m: &Message) -> (bool, usize) {
        if let Message::Assistant { content, .. } = m {
            let n = content
                .iter()
                .filter(|i| matches!(i, AssistantContent::ToolCall(_)))
                .count();
            (n > 0, n)
        } else {
            (false, 0)
        }
    }
    fn count_tool_results(m: &Message) -> (bool, usize) {
        if let Message::User { content } = m {
            let n = content
                .iter()
                .filter(|i| matches!(i, UserContent::ToolResult(_)))
                .count();
            (n > 0, n)
        } else {
            (false, 0)
        }
    }
    loop {
        let before = messages.len();
        if let Some(last) = messages.last() {
            if count_tool_calls(last).0 {
                messages.pop();
                continue;
            }
        }
        if messages.last().is_some_and(|m| count_tool_results(m).0) {
            let mut idx = messages.len();
            let mut result_count = 0;
            while idx > 0 {
                let (is_result, n) = count_tool_results(&messages[idx - 1]);
                if !is_result {
                    break;
                }
                result_count += n;
                idx -= 1;
            }
            if idx > 0 {
                let (has_call, call_count) = count_tool_calls(&messages[idx - 1]);
                if has_call && call_count == result_count {
                    break;
                }
                if has_call {
                    idx -= 1;
                }
            }
            messages.truncate(idx);
            continue;
        }
        if messages.len() == before {
            break;
        }
    }
}
