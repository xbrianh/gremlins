//! Chat turn lifecycle: chat request handling, turn execution, cancellation,
//! and chat gremlin lifecycle.
//!
//! Terminal-path handling for a turn lives in [`turn_finalizer`].
//!
//! All session and run-map state lives in [`super::actor`]; this module reaches
//! it exclusively through [`ActorMsg`] messages. There is no shared session
//! handle and no `RUN_MAP` lock — the actor serializes every transition.

mod payload;
mod turn;
mod turn_finalizer;

// Re-exported for the supervisor test module, which reaches these leaf
// helpers as `super::chat::{add_usage_stats, usage_stats_from_state}`.
#[allow(unused_imports)]
pub(crate) use payload::{add_usage_stats, chat_payload, usage_stats_from_state};
pub(crate) use turn::{handle_cancel_chat, run_chat_turn, TurnCtx};

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};

use serde_json::{Map, Value};
use tokio::sync::{mpsc, watch};

use crate::clients::cancel::CancelToken;
use crate::clients::client::{Client as ModelClient, ModelURI};
use crate::config;
use crate::definition::ChatDefinition;
use crate::executor::gremlin::{validate_gremlin_id, Gremlin};
use crate::executor::state;
use gremlins_magic_bus::EventKind;

use super::actor_client::Client;
use super::{error_response, ok_response, CurrentTurn};
use gremlins_magic_bus::connection::SharedWriter;

/// Daemon process environment, captured once at startup. Requests without `env`
/// fall back to this snapshot.
static BASE_PROCESS_ENV: OnceLock<HashMap<String, String>> = OnceLock::new();

pub(crate) fn base_process_env() -> &'static HashMap<String, String> {
    BASE_PROCESS_ENV.get_or_init(|| std::env::vars().collect())
}

/// Resolve chat gremlin env: TUI-supplied JSON object wins, otherwise daemon baseline.
pub(crate) fn resolve_chat_env(
    request: &Value,
    base_env: &HashMap<String, String>,
) -> HashMap<String, String> {
    match request.get("env") {
        Some(Value::Object(map)) => map
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect(),
        _ => base_env.clone(),
    }
}

pub(crate) async fn handle_chat(
    request: &Value,
    writer: &SharedWriter,
    actor_tx: &mpsc::UnboundedSender<super::actor::ActorMsg>,
    session_id: &str,
) {
    log::debug!("handle_chat: received chat request");

    let client = Client::new(actor_tx.clone());

    // The daemon owns the conversation; the session owns the gremlin. The
    // front-end never sends a gremlin id — it only addresses the session.
    let text = request.get("text").and_then(|v| v.as_str()).unwrap_or("");
    if text.is_empty() {
        let resp = error_response("missing 'text' field");
        let _ = writer.write_json_line(&resp).await;
        return;
    }
    // Optional repository-context snapshot captured by the TUI. Kept separate
    // from `text` and appended to the system prompt as low-trust metadata.
    let context = request
        .get("context")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // Client-generated turn id, echoed on every chat event so the TUI can
    // drop stale events from a superseded turn.
    let turn = request
        .get("turn")
        .and_then(|v| v.as_str())
        .map(String::from);
    // The operator's project root, supplied by the TUI. When absent (non-TUI
    // callers), fall back to the daemon's resolved project root — resolved
    // *after* `init_global()` so `paths.project-root` is visible.
    let project_root = request
        .get("project_root")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from);

    // The operator's environment, supplied by the TUI as a JSON object. When
    // absent (non-TUI callers), fall back to the daemon's base process env
    // snapshot so commands still run with a sane environment. The map lives
    // only on the in-memory `Gremlin.env` — never persisted to state or logs.
    let env = resolve_chat_env(request, base_process_env());

    // Optional list of skill names to activate for this turn. Replaces the
    // session's in-memory active skill set (never persisted). The TUI always
    // sends the full active set on each chat request, so `/clear` resets it
    // by sending an empty array.
    let requested_skills: Vec<String> = request
        .get("skills")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // 1. Init config
    if let Err(e) = config::init_global() {
        let resp = error_response(&format!("config init: {e}"));
        let _ = writer.write_json_line(&resp).await;
        return;
    }

    let project_root = project_root.unwrap_or_else(config::project_root);

    let cfg = match config::global_config() {
        Ok(cfg) => cfg,
        Err(e) => {
            let resp = error_response(&format!("config error: {e}"));
            let _ = writer.write_json_line(&resp).await;
            return;
        }
    };

    // 2. Resolve the chat model through the TUI model resolver:
    //    tui.json -> tui-model -> default-model. The resolver returns a
    //    concrete spec (profile references already resolved), so only the
    //    `ModelURI` shape check remains before the spec goes into the
    //    per-turn chat definition.
    let chat_model = match config::resolve_tui_chat_model(&cfg) {
        Ok(Some((_source, spec))) => match ModelURI::parse(&spec) {
            Ok(uri) => uri,
            Err(e) => {
                let resp = error_response(&format!("invalid chat model {spec:?}: {e}"));
                let _ = writer.write_json_line(&resp).await;
                return;
            }
        },
        Ok(None) => {
            let resp = error_response("no model configured");
            let _ = writer.write_json_line(&resp).await;
            return;
        }
        Err(e) => {
            let resp = error_response(&format!("invalid chat model: {e}"));
            let _ = writer.write_json_line(&resp).await;
            return;
        }
    };

    // 3. Wait for any in-flight turn to finish so that LoadSkill pushes
    //    from the running turn are visible in the shared active set before
    //    we snapshot it. This avoids a race where a queued turn's system
    //    prompt is built from a stale snapshot.
    //
    //    The session owns at most one chat gremlin today, but
    //    representations keep a map for multiple.
    let mut id = reusable_chat_gremlin_id(actor_tx, session_id).await;
    if let Some(existing) = &id {
        // The gremlin may have been removed from the run map by a control-side
        // `stop`. Treat it as gone and let the next message start fresh.
        if !client.contains_run(existing).await {
            let _ = client.session_take_gremlin(session_id, existing).await;
            let _ = client.session_remove_chat_id(session_id, existing).await;
            id = None;
        }
    }

    // 4. Update the session's active skill set and build the system prompt.
    //    The supervisor queues the operator's prompt text onto the ChatDefinition
    //    before each turn; the definition constructs the Agent stage on demand
    //    via next_stage().
    //
    //    Skills are discovered from the project root on each turn so newly
    //    added skills take effect without a daemon restart. Discovery runs in
    //    spawn_blocking so filesystem I/O does not stall the async executor.
    //
    //    Operator-requested skills are stored in the session. Model-loaded
    //    skills arrive via SkillLoaded bus events during the turn and are
    //    appended to the session by run_chat_turn. At turn start we read both
    //    to build the combined active set.
    let project_root_for_skills = project_root.clone();
    let all_skills =
        tokio::task::spawn_blocking(move || crate::skills::discover(&project_root_for_skills))
            .await
            .unwrap_or_default();
    // Store the operator's requested skill set in the session.
    client
        .session_set_operator_skills(session_id, requested_skills.clone())
        .await;
    // Read the combined active set: operator-requested + model-loaded.
    let active_skills = match client.session_get_combined_skills(session_id).await {
        Some(skills) => skills,
        None => {
            let resp = error_response("session closed");
            let _ = writer.write_json_line(&resp).await;
            return;
        }
    };
    let (trusted_section, unresolved_skills) =
        crate::skills::build_system_prompt_section(&all_skills, &active_skills);
    let system_prompt = crate::clients::agent::system_prompts::chat_system_prompt().to_string();

    // 5. Get or create the session's chat gremlin (id was resolved in step 3
    //    after waiting for the in-flight turn).

    let id = match id {
        Some(id) => id,
        None => {
            let id = match client.reserve_chat_id().await {
                Some(id) => id,
                None => {
                    let resp = error_response("actor unavailable");
                    let _ = writer.write_json_line(&resp).await;
                    return;
                }
            };
            let gremlin =
                match create_chat_gremlin(&id, &chat_model, &system_prompt, &project_root, &env) {
                    Ok(g) => g,
                    Err(e) => {
                        let resp = error_response(&e);
                        let _ = writer.write_json_line(&resp).await;
                        return;
                    }
                };
            if client.session_is_closed(session_id).await {
                drop(gremlin);
                let resp = error_response("session closed");
                let _ = writer.write_json_line(&resp).await;
                return;
            }
            // `register_chat_gremlin` parks the gremlin and installs the
            // run-map entry; ownership is registered as part of that message.
            client.register_chat_gremlin(session_id, gremlin).await;
            id
        }
    };

    // Ensure the id is still marked as owned by this session (for the reuse
    // path it already is, and this keeps the invariant local).
    if client.session_is_closed(session_id).await {
        let resp = error_response("session closed");
        let _ = writer.write_json_line(&resp).await;
        return;
    }
    client.session_add_chat_id(session_id, &id).await;

    // 6. Take the gremlin out of the session, install this turn's definition
    //    and a fresh per-turn cancel token. The session is actor-owned, so
    //    other ops are never serialized behind a lock here.
    let turn_cancel = CancelToken::new();
    let mut gremlin = match client.session_take_gremlin(session_id, &id).await {
        Some(g) => g,
        None => {
            let resp = error_response(&format!("unknown chat gremlin {id:?}"));
            let _ = writer.write_json_line(&resp).await;
            return;
        }
    };
    // Rebuild the parsed model handle for the reused gremlin so a `/model`
    // change takes effect on the next turn rather than only after the chat
    // gremlin is recreated. Build the client before mutating the gremlin so
    // a failure re-inserts it unchanged (old definition, old client).
    let llm_client = match ModelClient::from_uri(chat_model.clone()) {
        Ok(client) => client,
        Err(e) => {
            let _ = client
                .session_reinsert_gremlin(session_id, &id, gremlin)
                .await;
            let resp = error_response(&format!("invalid chat model '{chat_model}': {e}"));
            let _ = writer.write_json_line(&resp).await;
            return;
        }
    };
    gremlin.client = llm_client;
    // Inject the dynamic preamble (repo context + skills) on every turn so
    // newly activated skills and refreshed repository snapshots reach the model.
    // The preamble is part of the user turn, not the system prompt, so the
    // static system prompt stays cache-stable.
    let mut text = text.to_string();
    let dynamic_preamble =
        crate::clients::agent::system_prompts::build_chat_preamble(context, &trusted_section);
    if !dynamic_preamble.is_empty() {
        text = format!("{dynamic_preamble}\n---\n{text}");
    }
    // Queue this turn's prompt onto the ChatDefinition so that
    // next_stage() builds the agent stage on demand.
    gremlin
        .definition
        .as_any_mut()
        .and_then(|a| a.downcast_mut::<ChatDefinition>())
        .expect("chat gremlin must carry a ChatDefinition")
        .set_pending(&text, &chat_model, &system_prompt);
    // Keep the state.json `model` field in sync too.
    let state_patch = serde_json::json!({"model": chat_model.to_string()});
    gremlin.state.patch(&[], state_patch.as_object().unwrap());
    gremlin.cancel_token = Some(turn_cancel.clone());
    // Refresh the environment on every turn: the chat gremlin is persistent
    // and reused across turns, so a later turn's operator environment must
    // replace the one captured at creation rather than freeze at first contact.
    gremlin.env = env;
    // Wire up skills for the LoadSkill tool.
    gremlin.discovered_skills = Some(Arc::new(all_skills));

    // Point the run-map handle's cancel token at this turn so `stop` cancels
    // the in-flight stage rather than a stale persistent token.
    client.set_run_cancel(&id, turn_cancel.clone()).await;

    let stage = match gremlin.definition.next_stage().await {
        Ok(s) => s,
        Err(e) => {
            // Re-insert the gremlin (it is still owned by this session)
            // and report the error. The id remains owned so session
            // teardown still shuts it down.
            let _ = client
                .session_reinsert_gremlin(session_id, &id, gremlin)
                .await;
            let resp = error_response(&format!("definition error: {e}"));
            let _ = writer.write_json_line(&resp).await;
            return;
        }
    };

    // Invalidate the current turn's per-turn artifact for a fresh turn.
    // After rollback via `goto("chat-0003")`, a stale `conversation/0004.json`
    // from the abandoned branch would otherwise be loaded by `run_agent` and
    // leak discarded messages into the replacement turn's context.
    // Clearing here only affects fresh turns; the cancel path decides
    // whether to preserve the artifact via `TurnFinalizer::cancel`.
    if let Some(turn_num) = crate::executor::run_agent::chat_turn_from_stage_name(stage.name()) {
        crate::executor::run_agent::clear_chat_turn_artifact(&gremlin, turn_num).await;
    }

    // 7. Run the stage in the background. Events are forwarded as
    //    unsolicited JSON lines to this connection while the dispatch
    //    loop stays live for cancel_chat and close_session.
    let (done_tx, done_rx) = watch::channel(false);
    let done_tx = Arc::new(done_tx);
    let hard = Arc::new(AtomicBool::new(false));
    client
        .session_set_current_turn(
            session_id,
            CurrentTurn {
                id: id.clone(),
                turn: turn.clone(),
                cancel: turn_cancel.clone(),
                done: done_rx,
                hard: hard.clone(),
            },
        )
        .await;

    let writer_for_turn = writer.clone();
    let turn_ctx = TurnCtx {
        id: id.clone(),
        turn: turn.clone(),
        turn_cancel: turn_cancel.clone(),
        actor_tx: actor_tx.clone(),
        session_id: session_id.to_string(),
        done: done_tx,
        hard,
    };
    tokio::spawn(async move {
        run_chat_turn(turn_ctx, stage, gremlin, writer_for_turn).await;
    });

    // Acknowledge the request so the TUI's `send_request` completes.
    // Chat events themselves are unsolicited lines written by the task
    // above and classified separately by the client read loop.
    let _ = writer
        .write_json_line(&ok_response(
            serde_json::json!({"id": id, "status": "streaming"}),
        ))
        .await;

    // Surface unresolved skill names as a non-fatal warning so the
    // operator knows a previously-activated skill is no longer available.
    if !unresolved_skills.is_empty() {
        let msg = format!("skill(s) not found: {}", unresolved_skills.join(", "));
        let payload = chat_payload(
            &turn,
            serde_json::to_value(EventKind::SystemWarning {
                message: msg,
                turn: turn.clone(),
            })
            .unwrap_or_default(),
        );
        let _ = writer.write_json_line(&payload).await;
    }
}

/// Return the session's parked chat gremlin id, waiting for an in-flight
/// turn to re-insert.
pub(crate) async fn reusable_chat_gremlin_id(
    actor_tx: &mpsc::UnboundedSender<super::actor::ActorMsg>,
    session_id: &str,
) -> Option<String> {
    loop {
        // Parked gremlin wins; otherwise look at the in-flight turn.
        let client = Client::new(actor_tx.clone());
        let (ids, turn) = tokio::join!(
            client.session_get_chat_gremlin_keys(session_id),
            client.session_get_current_turn(session_id),
        );

        if let Some(id) = ids.into_iter().next() {
            return Some(id);
        }

        let done = match turn {
            Some(turn) if !*turn.done.borrow() => Some(turn.done),
            // A turn with `done == true` completed but left no parked gremlin
            // (e.g. a stop removed it after a soft interrupt). Waiting on it
            // would spin forever, so report "no reusable gremlin".
            Some(_) => {
                // The turn finished but its done watch may have fired
                // before the gremlin was re-parked. Recheck.
                let ids = client.session_get_chat_gremlin_keys(session_id).await;
                return ids.into_iter().next();
            }
            None => {
                // A gremlin may have been parked between the two lookups.
                // Recheck before giving up.
                let ids = client.session_get_chat_gremlin_keys(session_id).await;
                return ids.into_iter().next();
            }
        };

        let mut done = done.expect("in-flight turn checked above");
        if done.wait_for(|v| *v).await.is_err() {
            // Sender dropped without publishing. Re-check the session: the
            // turn may have been cleared by another path and re-inserted the
            // gremlin in the meantime.
            return Client::new(actor_tx.clone())
                .session_get_chat_gremlin_keys(session_id)
                .await
                .into_iter()
                .next();
        }
        // The turn finished — loop to re-check the parked set.
    }
}

pub(crate) fn create_chat_gremlin(
    id: &str,
    default_model: &ModelURI,
    system_prompt: &str,
    project_root: &Path,
    env: &HashMap<String, String>,
) -> Result<Gremlin, String> {
    use crate::executor::gremlin::RuntimeConfig;
    use crate::executor::state::{BlobMode, StateData};

    let project_root = project_root.to_path_buf();

    // Id is pre-reserved through the actor, so no collision check needed.
    let gremlin_id = validate_gremlin_id(id).map_err(|e| format!("invalid id: {e}"))?;

    // Build initial state and create StateData (which writes state.json).
    let initial: Map<String, Value> = serde_json::json!({
        "id": gremlin_id.as_str(),
        "kind": "chat",
        "project_root": project_root.to_string_lossy(),
        "workdir": "",
        "status": "running",
        "started_at": state::now_stamp(),
        "model": default_model.to_string(),
        "stage": "chat",
        "pid": std::process::id(),
        "attempt": "",
        "exit_code": null,
        "definition_path": "",
        "description": "",
        "parent_id": "",
        "definition_args": [],
        "stage_inputs": {},
        "group_name": "",
        "child_key": "",
        "metadata": {},
    })
    .as_object()
    .unwrap()
    .clone();

    let state = StateData::new(gremlin_id.as_str(), &initial, /* ephemeral */ true)
        .map_err(|e| format!("failed to write state: {e}"))?;

    // Chat definitions are never serialized — skip the definition.json blob.

    // Create empty log file via state blob.
    let _ = state
        .open_blob("log", BlobMode::Write)
        .map_err(|e| format!("failed to create log: {e}"))?;

    let runtime_config = RuntimeConfig::snapshot(None);
    let client = ModelClient::from_uri(default_model.clone())
        .map_err(|e| format!("invalid default model '{default_model}': {e}"))?;

    Ok(Gremlin {
        id: gremlin_id,
        definition_path: None,
        model_override: None,
        definition: Box::new(ChatDefinition::new(
            "chat".to_string(),
            PathBuf::from("."),
            default_model.clone(),
            system_prompt.to_string(),
        )),
        workdir: None,
        project_root,
        state,
        env: env.clone(),
        client,
        loop_iter: "1".to_string(),
        stage_inputs: HashMap::new(),
        runtime_config,
        cancel_token: None,
        interactive_session: None,
        clean_cmds: Vec::new(),
        enclosing_task_models: None,
        enclosing_model: None,
        discovered_skills: None,
        extra_sandbox_roots: Vec::new(),
    })
}
