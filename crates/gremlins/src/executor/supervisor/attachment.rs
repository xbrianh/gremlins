//! Read-only, correlation-filtered watch streams for one gremlin.
//!
//! Native history and live display share ordering, metadata, and terminal rules.
//! Streams are bounded and lag-tolerant, with explicit gap reports.

use std::sync::Arc;

use serde_json::Value;
use tokio::io::BufReader;

use super::error_response;
use super::run_registry::RunRegistry;
use crate::executor::gremlin::validate_gremlin_id;
use crate::executor::socket;
use gremlins_magic_bus::connection::{BusSubGuard, SharedWriter};
use gremlins_magic_bus::{DispatchEvent, EventKind, HistorySnapshot, Recv};
use tokio::net::unix::OwnedReadHalf;

// ── Wire types ──────────────────────────────────────────────────────────

/// Lifecycle and lag frames for a watch subscriber.
#[derive(Debug, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RunEvent {
    StageBoundary {
        id: String,
        stage_run_id: String,
        stage: String,
        stage_type: String,
        active: bool,
    },
    StageTransition {
        id: String,
        stage: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        sub_stage: Option<String>,
    },
    RunCompleted {
        id: String,
        exit_code: i32,
    },
    RunFailed {
        id: String,
        exit_code: i32,
        error: Option<String>,
    },
    RunStopped {
        id: String,
    },
    Lagged {
        skipped: u64,
    },
}

impl RunEvent {
    fn from_kind(kind: &EventKind) -> Option<Self> {
        match kind {
            EventKind::StageBoundary {
                id,
                stage_run_id,
                stage,
                stage_type,
                active,
            } => Some(Self::StageBoundary {
                id: id.clone(),
                stage_run_id: stage_run_id.clone(),
                stage: stage.clone(),
                stage_type: stage_type.clone(),
                active: *active,
            }),
            EventKind::StageTransition {
                id,
                stage,
                sub_stage,
            } => Some(Self::StageTransition {
                id: id.clone(),
                stage: stage.clone(),
                sub_stage: sub_stage.clone(),
            }),
            EventKind::RunCompleted { id, exit_code } => Some(Self::RunCompleted {
                id: id.clone(),
                exit_code: *exit_code,
            }),
            EventKind::RunFailed {
                id,
                exit_code,
                error,
            } => Some(Self::RunFailed {
                id: id.clone(),
                exit_code: *exit_code,
                error: error.clone(),
            }),
            EventKind::RunStopped { id } => Some(Self::RunStopped { id: id.clone() }),
            _ => None,
        }
    }
}

/// Watch display frames retain the bus wire schema; lifecycle frames remain RunEvent.
fn run_display_kind(kind: &EventKind) -> bool {
    kind.is_chat_event()
        || matches!(
            kind,
            EventKind::StreamReset
                | EventKind::SkillLoaded { .. }
                | EventKind::ExecOutput { .. }
                | EventKind::LogLine { .. }
        )
}

fn run_kind_frame(kind: &EventKind) -> Option<Value> {
    if run_display_kind(kind) {
        serde_json::to_value(kind).ok()
    } else {
        RunEvent::from_kind(kind).and_then(|event| serde_json::to_value(event).ok())
    }
}

/// Protocol policy only: delivery ordering and I/O are shared below. Watch
/// display is selector-gated; parent lifecycle can end an invisible selection.
#[derive(Clone, Copy)]
enum PassiveStream<'a> {
    Watch(&'a gremlins_magic_bus::CorrId, &'a WatchMetadata),
}

impl PassiveStream<'_> {
    fn frame(self, dispatch: &DispatchEvent<EventKind>) -> Option<Value> {
        match self {
            Self::Watch(filter, metadata) => metadata.frame(dispatch, filter),
        }
    }

    fn terminal_update(self, dispatch: &DispatchEvent<EventKind>) -> Option<bool> {
        let Self::Watch(filter, _) = self;
        let id = filter.run().expect("watch run");
        if let EventKind::StageBoundary {
            stage,
            stage_run_id,
            active: false,
            ..
        } = dispatch.kind()
        {
            let corr = dispatch.corr_id();
            if filter.stage_run_id() == Some(stage_run_id.as_str())
                && corr.matches_prefix(filter)
                && corr
                    .run()
                    .is_some_and(|run| event_run_id_matches(dispatch.kind(), run))
                && corr.stage() == Some(stage.as_str())
                && corr.stage_run_id() == Some(stage_run_id.as_str())
            {
                return Some(true);
            }
        }
        if dispatch.corr_id().run() != Some(id) || !event_run_id_matches(dispatch.kind(), id) {
            return None;
        }
        match dispatch.kind() {
            EventKind::RunStarted { .. } => Some(false),
            _ if watch_parent_terminal(dispatch, id) => Some(true),

            _ => None,
        }
    }

    fn checkpoint(self) -> Value {
        match self {
            Self::Watch(filter, _) => serde_json::json!({
                "type": "replay_checkpoint", "id": filter.run(), "scope": "run",
                "stage": filter.stage(), "stage_run_id": filter.stage_run_id(),
            }),
        }
    }
}

async fn write_history(
    writer: &SharedWriter,
    stream: PassiveStream<'_>,
    history: HistorySnapshot<EventKind>,
) -> Result<bool, String> {
    if !history.complete {
        writer
            .write_json_line(&serde_json::json!({
                "type": "history_gap",
                "reason": "history_incomplete",
                "through_sequence": history.through_sequence,
                "evicted_through_sequence": history.evicted_through_sequence,
            }))
            .await?;
    }
    let mut run_terminal = false;
    let mut invocation_terminal = false;
    for dispatch in history.events {
        let frame = stream.frame(&dispatch);
        if let Some(update) = stream.terminal_update(&dispatch) {
            if matches!(dispatch.kind(), EventKind::StageBoundary { .. }) {
                invocation_terminal |= update;
            } else {
                run_terminal = update;
            }
        }
        if let Some(value) = frame {
            writer.write_json_line(&value).await?;
        }
    }
    let mut checkpoint = stream.checkpoint();
    checkpoint["complete"] = serde_json::json!(history.complete);
    checkpoint["through_sequence"] = serde_json::json!(history.through_sequence);
    checkpoint["evicted_through_sequence"] = serde_json::json!(history.evicted_through_sequence);
    writer.write_json_line(&checkpoint).await?;
    Ok(run_terminal || invocation_terminal)
}

/// Returns true after a terminal event or closure; delivery failures propagate.
async fn forward_passive(
    writer: &SharedWriter,
    stream: PassiveStream<'_>,
    event: Recv<EventKind>,
) -> Result<bool, String> {
    match event {
        Recv::Event(dispatch) => {
            let frame = stream.frame(&dispatch);
            let terminal = stream.terminal_update(&dispatch) == Some(true);
            if let Some(frame) = frame {
                writer.write_json_line(&frame).await?;
            }
            Ok(terminal)
        }
        Recv::Lagged(skipped) => {
            let frame = serde_json::to_value(RunEvent::Lagged { skipped })
                .map_err(|error| error.to_string())?;
            writer.write_json_line(&frame).await?;
            Ok(false)
        }
        Recv::Closed => Ok(true),
    }
}

pub(super) fn retired_target_error(request: &Value) -> Option<Value> {
    super::input_wire::retired_target_message(request).map(|message| error_response(&message))
}

// ── Read-only, correlation-filtered watch ─────────────────────────────

fn watch_filter(request: &Value) -> Result<(gremlins_magic_bus::CorrId, bool), String> {
    let fields = request
        .as_object()
        .ok_or("watch request must be an object")?;
    for key in fields.keys() {
        if !matches!(
            key.as_str(),
            "op" | "id" | "session_id" | "req_id" | "scope" | "stage" | "stage_run_id" | "replay"
        ) {
            return Err(format!("unsupported watch field '{key}'"));
        }
    }
    let id = request
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or("missing or invalid 'id' field")?;
    validate_gremlin_id(id).map_err(|_| format!("invalid gremlin id {id:?}"))?;
    if let Some(scope) = request.get("scope") {
        if scope.as_str() != Some("run") {
            return Err("watch scope must be 'run'".into());
        }
    }
    let mut filter = gremlins_magic_bus::CorrId::new().with_run(id);
    for key in ["stage", "stage_run_id"] {
        if let Some(value) = request.get(key) {
            let value = value
                .as_str()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| format!("'{key}' must be a nonempty string"))?;
            filter = if key == "stage" {
                filter.with_stage(value)
            } else {
                filter.with_stage_run_id(value)
            };
        }
    }
    let replay = match request.get("replay") {
        None => false,
        Some(value) => value.as_bool().ok_or("'replay' must be a boolean")?,
    };
    Ok((filter, replay))
}

#[derive(Default)]
struct WatchMetadata {
    stage_types: std::sync::Mutex<std::collections::HashMap<(String, String, String), String>>,
}

impl WatchMetadata {
    fn record_boundary(
        &self,
        dispatch: &DispatchEvent<EventKind>,
        filter: &gremlins_magic_bus::CorrId,
    ) {
        let EventKind::StageBoundary {
            id,
            stage,
            stage_run_id,
            stage_type,
            ..
        } = dispatch.kind()
        else {
            return;
        };
        let corr = dispatch.corr_id();
        if corr.matches_prefix(filter)
            && corr.run() == Some(id.as_str())
            && corr.stage() == Some(stage.as_str())
            && corr.stage_run_id() == Some(stage_run_id.as_str())
        {
            self.stage_types
                .lock()
                .expect("watch metadata poisoned")
                .insert(
                    (id.clone(), stage.clone(), stage_run_id.clone()),
                    stage_type.clone(),
                );
        }
    }

    fn frame(
        &self,
        dispatch: &DispatchEvent<EventKind>,
        filter: &gremlins_magic_bus::CorrId,
    ) -> Option<Value> {
        let mut frame = watch_frame(dispatch, filter)?;
        self.record_boundary(dispatch, filter);
        let corr = dispatch.corr_id();
        if let (Some(run), Some(stage), Some(invocation)) =
            (corr.run(), corr.stage(), corr.stage_run_id())
        {
            let key = (run.to_owned(), stage.to_owned(), invocation.to_owned());
            let mut types = self.stage_types.lock().expect("watch metadata poisoned");
            if let Some(stage_type) = dispatch
                .stage_type()
                .or_else(|| types.get(&key).map(String::as_str))
            {
                frame["stage_type"] = serde_json::json!(stage_type);
            }
            if matches!(
                dispatch.kind(),
                EventKind::StageBoundary { active: false, .. }
            ) {
                types.remove(&key);
            }
        }
        Some(frame)
    }
}

fn watch_frame(
    dispatch: &DispatchEvent<EventKind>,
    filter: &gremlins_magic_bus::CorrId,
) -> Option<Value> {
    let corr = dispatch.corr_id();
    if !corr.matches_prefix(filter) {
        return None;
    }
    // A matching watch selector always includes a run.
    debug_assert!(corr.run().is_some());
    let run = corr
        .run()
        .filter(|run| event_run_id_matches(dispatch.kind(), run))?;
    if matches!(dispatch.kind(), EventKind::PromptRequest { .. }) {
        return None;
    }
    let mut frame = if matches!(dispatch.kind(), EventKind::RunStarted { .. }) {
        serde_json::to_value(dispatch.kind()).ok()?
    } else {
        run_kind_frame(dispatch.kind())?
    };
    frame["sequence"] = serde_json::json!(dispatch.sequence());
    frame["run_id"] = serde_json::json!(run);
    if let Some(stage) = corr.stage() {
        // Display transitions can name an enclosing stage. Keep that payload
        // intact and expose the leaf invocation route separately when needed.
        if frame.get("stage").is_some_and(|value| value != stage) {
            frame["correlation_stage"] = serde_json::json!(stage);
        } else {
            frame["stage"] = serde_json::json!(stage);
        }
    }
    if let Some(stage_run_id) = corr.stage_run_id() {
        frame["stage_run_id"] = serde_json::json!(stage_run_id);
    }
    if let Some(turn) = corr.turn_token() {
        frame["correlation_turn"] = serde_json::json!(turn);
    }
    Some(frame)
}

fn watch_accepts(dispatch: &DispatchEvent<EventKind>, filter: &gremlins_magic_bus::CorrId) -> bool {
    dispatch.corr_id().matches_prefix(filter)
        || (dispatch.corr_id().run() == filter.run()
            && event_run_id_matches(dispatch.kind(), filter.run().expect("watch run"))
            && matches!(
                dispatch.kind(),
                EventKind::RunStarted { .. }
                    | EventKind::RunCompleted { .. }
                    | EventKind::RunFailed { .. }
                    | EventKind::RunStopped { .. }
            ))
}

fn watch_parent_terminal(dispatch: &DispatchEvent<EventKind>, id: &str) -> bool {
    dispatch.corr_id().run() == Some(id)
        && event_run_id_matches(dispatch.kind(), id)
        && matches!(
            dispatch.kind(),
            EventKind::RunCompleted { .. }
                | EventKind::RunFailed { .. }
                | EventKind::RunStopped { .. }
        )
}

async fn write_watch_history(
    writer: &SharedWriter,
    filter: &gremlins_magic_bus::CorrId,
    history: HistorySnapshot<EventKind>,
    metadata: &WatchMetadata,
) -> Result<bool, String> {
    // A retained closing boundary remains authoritative when entry was evicted.
    // Collect before writing so earlier retained chunks carry the same identity.
    for dispatch in &history.events {
        metadata.record_boundary(dispatch, filter);
    }
    write_history(writer, PassiveStream::Watch(filter, metadata), history).await
}

/// Observe native history/live events without acquiring an input lease.
/// Shared entry point for control and verified auxiliary socket dispatch.
pub(crate) async fn handle_watch(
    request: &Value,
    registry: &Arc<RunRegistry>,
    mut reader: BufReader<OwnedReadHalf>,
    writer: &SharedWriter,
) {
    if let Some(error) = retired_target_error(request) {
        let _ = writer.write_json_line(&error).await;
        return;
    }
    let (filter, replay) = match watch_filter(request) {
        Ok(selection) => selection,
        Err(error) => {
            let _ = writer.write_json_line(&error_response(&error)).await;
            return;
        }
    };
    let id = filter.run().expect("validated watch id");
    if registry.get_run_store(id).await.is_none() {
        let _ = writer
            .write_json_line(&error_response(&format!("gremlin {id} is not running")))
            .await;
        return;
    }
    let bus = gremlins_magic_bus::init_bus();
    let registration = gremlins_magic_bus::SubscriberRegistration::lag_tolerant(
        gremlins_magic_bus::CorrId::new().with_run(id),
        256,
    );
    // Keep selector events and parent lifecycle in one ordered mailbox: the
    // latter can end an invisible selection, while sibling output must not
    // consume its lag-tolerant capacity.
    let selection = filter.clone();
    let accepts = move |event: &DispatchEvent<EventKind>| watch_accepts(event, &selection);
    let (sub, mut rx, history) = if replay {
        let (sub, rx, history) = bus
            .subscribe_with_history_filtered(registration, accepts)
            .expect("valid watch subscription");
        (sub, rx, Some(history))
    } else {
        let (sub, rx) = bus
            .subscribe_filtered(registration, accepts)
            .expect("valid watch subscription");
        (sub, rx, None)
    };
    let _guard = BusSubGuard::new(sub);
    if writer
        .write_json_line(&serde_json::json!({
            "type": "watched", "id": id, "scope": "run", "replay": replay,
            "stage": filter.stage(), "stage_run_id": filter.stage_run_id(),
        }))
        .await
        .is_err()
    {
        return;
    }
    let metadata = WatchMetadata::default();
    if let Some(history) = history {
        match write_watch_history(writer, &filter, history, &metadata).await {
            Ok(false) => {}
            Ok(true) | Err(_) => return,
        }
    }
    loop {
        tokio::select! {
            biased;
            command = socket::read_json_line(&mut reader) => {
                if let Ok(Some(command)) = command {
                    let error = retired_target_error(&command).unwrap_or_else(||
                        error_response("watch is read-only; commands are not accepted"));
                    let _ = writer.write_json_line(&error).await;
                }
                break;
            }
            event = rx.recv() => {
                if !matches!(
                    forward_passive(writer, PassiveStream::Watch(&filter, &metadata), event).await,
                    Ok(false)
                ) {
                    break;
                }
            }
        }
    }
}

/// Returns `true` when `kind` carries an id matching `target`.
/// Every `EventKind` variant must appear in one arm — nothing falls through.
fn event_run_id_matches(kind: &EventKind, target: &str) -> bool {
    match kind {
        EventKind::RunStarted { id, .. }
        | EventKind::RunCompleted { id, .. }
        | EventKind::RunFailed { id, .. }
        | EventKind::RunStopped { id }
        | EventKind::RunRemoved { id }
        | EventKind::TokenUsage { id, .. }
        | EventKind::StageTransition { id, .. }
        | EventKind::Bail { id, .. }
        | EventKind::LogLine { id, .. }
        | EventKind::StageBoundary { id, .. }
        | EventKind::ExecOutput { id, .. }
        | EventKind::SkillLoaded { id, .. }
        | EventKind::OperatorMessage { id, .. }
        | EventKind::PromptRequest { id, .. } => id == target,
        // Variants that carry no per-run id: match everything.
        EventKind::ModelCallUsage { .. }
        | EventKind::TurnStarted { .. }
        | EventKind::StreamChunk { .. }
        | EventKind::ReasoningChunk { .. }
        | EventKind::TurnComplete { .. }
        | EventKind::ToolOutputChunk { .. }
        | EventKind::ToolResult { .. }
        | EventKind::StreamReset
        | EventKind::Done { .. }
        | EventKind::Ended { .. }
        | EventKind::Error { .. }
        | EventKind::SystemWarning { .. }
        | EventKind::TaskProgress { .. } => true,
    }
}

#[cfg(test)]
mod routing_tests {
    use super::*;

    #[tokio::test]
    async fn passive_forwarding_preserves_selector_terminal_and_lag_rules() {
        use gremlins_magic_bus::CorrId;
        let filter = CorrId::new().with_run("parent").with_stage("build");
        let (client, server) = tokio::net::UnixStream::pair().unwrap();
        let (_, write) = server.into_split();
        let writer = SharedWriter::new(write);
        let mut reader = BufReader::new(client);
        let stopped =
            |corr, id: &str| DispatchEvent::new(corr, EventKind::RunStopped { id: id.into() });
        let parent = stopped(CorrId::new().with_run("parent"), "parent");
        let metadata = WatchMetadata::default();
        let watch = PassiveStream::Watch(&filter, &metadata);
        // Parent termination closes a stage watch without leaking the frame.
        assert!(forward_passive(&writer, watch, Recv::Event(parent.clone()))
            .await
            .unwrap());
        // Nor may the watch close on mismatched payload identity.
        let mismatched = stopped(filter.clone(), "other");
        assert!(!forward_passive(&writer, watch, Recv::Event(mismatched))
            .await
            .unwrap());
        assert!(!forward_passive(&writer, watch, Recv::Lagged(3))
            .await
            .unwrap());
        let frame = socket::read_json_line(&mut reader).await.unwrap().unwrap();
        assert_eq!(frame, serde_json::json!({"type": "lagged", "skipped": 3}));
        // Descendant terminal events are visible to watch, but are not terminal.
        let child = stopped(
            CorrId::new().with_run("parent:child").with_stage("build"),
            "parent:child",
        );
        assert!(!forward_passive(&writer, watch, Recv::Event(child))
            .await
            .unwrap());
        let frame = socket::read_json_line(&mut reader).await.unwrap().unwrap();
        assert_eq!(frame["run_id"], "parent:child");
        let parent = stopped(filter.clone(), "parent");
        assert!(forward_passive(&writer, watch, Recv::Event(parent))
            .await
            .unwrap());
        let frame = socket::read_json_line(&mut reader).await.unwrap().unwrap();
        assert_eq!(frame["stage"], "build");
        assert!(forward_passive(&writer, watch, Recv::Closed).await.unwrap());
    }

    #[test]
    fn watch_rejects_invalid_selectors_without_broadening() {
        for invalid in [
            serde_json::json!(null),
            serde_json::json!(""),
            serde_json::json!(7),
            serde_json::json!({}),
        ] {
            for key in ["id", "stage", "stage_run_id", "scope"] {
                let mut request = serde_json::json!({"id": "watch-test"});
                request[key] = invalid.clone();
                assert!(watch_filter(&request).is_err(), "{request}");
            }
        }
        assert!(
            watch_filter(&serde_json::json!({"id": "watch-test", "scope": "conversation"}))
                .is_err()
        );
        assert!(watch_filter(&serde_json::json!({"id": "watch-test", "target": {}})).is_err());
        assert!(
            retired_target_error(&serde_json::json!({"target": {"execution_id": "old"}})).is_some()
        );
    }

    #[tokio::test]
    async fn watch_native_replay_filters_descendants_and_preserves_tool_identity() {
        use gremlins_magic_bus::{Bus, CorrId, SubscriberRegistration, TryRecvError};
        let bus = Bus::new();
        let filter = CorrId::new()
            .with_run("watch-test")
            .with_stage("build")
            .with_stage_run_id("execution");
        let child = CorrId::new()
            .with_run("watch-test:child")
            .with_stage("build")
            .with_stage_run_id("execution");
        bus.emit(DispatchEvent::new(
            child.clone(),
            EventKind::RunStarted {
                id: "watch-test:child".into(),
                definition: "test".into(),
                stage: "build".into(),
            },
        ));
        bus.emit(DispatchEvent::new(
            child.clone(),
            EventKind::RunStopped {
                id: "watch-test".into(),
            },
        ));
        bus.emit(DispatchEvent::new(
            child.clone(),
            EventKind::PromptRequest {
                id: "watch-test:child".into(),
                json: "secret".into(),
            },
        ));
        bus.emit(DispatchEvent::new(
            child.clone(),
            EventKind::RunStopped {
                id: "watch-test:child".into(),
            },
        ));
        bus.emit(DispatchEvent::new(
            CorrId::new().with_run("watch-test:childish"),
            EventKind::StreamChunk {
                text: "wrong execution".into(),
            },
        ));
        let (_, mut rx, history) = bus
            .subscribe_with_history(SubscriberRegistration::lag_tolerant(filter.clone(), 256))
            .unwrap();
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
        bus.emit_async(DispatchEvent::new(
            child,
            EventKind::StreamChunk {
                text: "live".into(),
            },
        ))
        .await;
        let (client, server) = tokio::net::UnixStream::pair().unwrap();
        let (_, write) = server.into_split();
        let writer = SharedWriter::new(write);
        assert!(
            !write_watch_history(&writer, &filter, history, &WatchMetadata::default())
                .await
                .unwrap()
        );
        let live = watch_frame(&rx.try_recv().unwrap(), &filter).unwrap();
        writer.write_json_line(&live).await.unwrap();
        let mut reader = BufReader::new(client);
        for (kind, sequence) in [("run_started", 1), ("run_stopped", 4)] {
            let frame = socket::read_json_line(&mut reader).await.unwrap().unwrap();
            assert_eq!(frame["type"], kind);
            assert_eq!(frame["sequence"], sequence);
            assert_eq!(frame["run_id"], "watch-test:child");
            assert_eq!(frame["stage"], "build");
            assert_eq!(frame["stage_run_id"], "execution");
        }
        let checkpoint = socket::read_json_line(&mut reader).await.unwrap().unwrap();
        assert_eq!(checkpoint["type"], "replay_checkpoint");
        assert_eq!(checkpoint["through_sequence"], 5);
        assert_eq!(
            socket::read_json_line(&mut reader).await.unwrap().unwrap()["sequence"],
            6
        );
        // Tool ids are payload identities, not run ids.
        let tool: EventKind = serde_json::from_value(serde_json::json!({
            "type": "tool_output_chunk", "id": "tool-42", "name": "shell", "call": "ls", "text": "output"
        }))
        .unwrap();
        let frame = watch_frame(&DispatchEvent::new(filter.clone(), tool), &filter).unwrap();
        assert_eq!(frame["id"], "tool-42");
        assert_eq!(frame["run_id"], "watch-test");
    }

    #[tokio::test]
    async fn watch_running_invocation_type_survives_evicted_entry_and_turn_started() {
        use gremlins_magic_bus::{Bus, CorrId, SubscriberRegistration};
        for stage_type in [
            Some("agent"),
            Some("exec"),
            Some("sequence"),
            Some("parallel"),
            Some("unknown"),
            None,
        ] {
            let bus = Bus::with_history_limits(1, 1)
                .with_invocation_metadata(EventKind::invocation_metadata);
            let filter = CorrId::new().with_run("running-watch");
            let corr = filter
                .clone()
                .with_stage("worker")
                .with_stage_run_id("one")
                .with_turn_token("caller-turn");
            let boundary = |active| {
                DispatchEvent::new(
                    corr.clone(),
                    EventKind::StageBoundary {
                        id: "running-watch".into(),
                        stage: "worker".into(),
                        stage_run_id: "one".into(),
                        stage_type: stage_type.unwrap_or("unknown").into(),
                        active,
                    },
                )
            };
            if stage_type.is_some() {
                bus.emit(boundary(true));
            }
            bus.emit(DispatchEvent::new(
                corr.clone(),
                EventKind::TurnStarted {
                    stage_run_id: "one".into(),
                },
            ));
            bus.emit(DispatchEvent::new(
                corr.clone(),
                EventKind::StreamChunk {
                    text: "retained".into(),
                },
            ));
            let (_, mut rx, history) = bus
                .subscribe_with_history(SubscriberRegistration::lag_tolerant(filter.clone(), 256))
                .unwrap();
            assert!(!history.complete);
            assert_eq!(history.events.len(), 1);
            assert!(matches!(
                history.events[0].kind(),
                EventKind::StreamChunk { .. }
            ));
            let cut = history.through_sequence;
            let (client, server) = tokio::net::UnixStream::pair().unwrap();
            let (_, write) = server.into_split();
            let writer = SharedWriter::new(write);
            let metadata = WatchMetadata::default();
            assert!(!write_watch_history(&writer, &filter, history, &metadata)
                .await
                .unwrap());
            let mut reader = BufReader::new(client);
            let gap = socket::read_json_line(&mut reader).await.unwrap().unwrap();
            assert_eq!(gap["type"], "history_gap");
            let retained = socket::read_json_line(&mut reader).await.unwrap().unwrap();
            assert_eq!(retained["type"], "stream_chunk");
            assert_eq!(retained["sequence"], cut);
            assert_eq!(
                retained.get("stage_type").and_then(Value::as_str),
                stage_type
            );
            let checkpoint = socket::read_json_line(&mut reader).await.unwrap().unwrap();
            assert_eq!(checkpoint["type"], "replay_checkpoint");
            assert_eq!(checkpoint["complete"], false);
            assert_eq!(checkpoint["through_sequence"], cut);
            bus.emit_async(DispatchEvent::new(
                corr.clone(),
                EventKind::StreamChunk {
                    text: "live".into(),
                },
            ))
            .await;
            assert!(!forward_passive(
                &writer,
                PassiveStream::Watch(&filter, &metadata),
                rx.recv().await
            )
            .await
            .unwrap());
            let live = socket::read_json_line(&mut reader).await.unwrap().unwrap();
            assert_eq!(live["sequence"], cut + 1);
            assert_eq!(live.get("stage_type").and_then(Value::as_str), stage_type);
            assert_eq!(live["stage_run_id"], "one");
            assert_eq!(live["correlation_turn"], "caller-turn");
            // Other stages, invocations, and descendant runs cannot inherit this type.
            for unrelated in [
                corr.clone().with_stage("other"),
                corr.clone().with_stage_run_id("two"),
                corr.clone().with_run("running-watch:child"),
            ] {
                bus.emit(DispatchEvent::new(
                    unrelated,
                    EventKind::StreamChunk {
                        text: "unknown".into(),
                    },
                ));
                let gremlins_magic_bus::Recv::Event(event) = rx.recv().await else {
                    panic!("missing chunk")
                };
                assert!(metadata
                    .frame(&event, &filter)
                    .unwrap()
                    .get("stage_type")
                    .is_none());
            }
            if stage_type.is_some() {
                bus.emit(boundary(false));
                let _ = rx.recv().await;
                bus.emit(DispatchEvent::new(
                    corr.clone(),
                    EventKind::StreamChunk {
                        text: "after close".into(),
                    },
                ));
                let gremlins_magic_bus::Recv::Event(event) = rx.recv().await else {
                    panic!("missing chunk")
                };
                assert!(event.stage_type().is_none());
            }
        }
    }

    #[tokio::test]
    async fn watch_truncated_replay_uses_closing_boundary_without_guessing_agents() {
        use gremlins_magic_bus::{Bus, CorrId, SubscriberRegistration};
        for stage_type in ["agent", "exec", "sequence", "parallel", "unknown"] {
            let bus = Bus::with_history_limits(2, 2);
            let filter = CorrId::new().with_run("truncated-watch");
            let corr = filter
                .clone()
                .with_stage("worker")
                .with_stage_run_id("one")
                .with_turn_token("caller-turn");
            let boundary = |active| {
                DispatchEvent::new(
                    corr.clone(),
                    EventKind::StageBoundary {
                        id: "truncated-watch".into(),
                        stage: "worker".into(),
                        stage_run_id: "one".into(),
                        stage_type: stage_type.into(),
                        active,
                    },
                )
            };
            bus.emit(boundary(true));
            bus.emit(DispatchEvent::new(
                corr.clone(),
                EventKind::StreamChunk {
                    text: "retained".into(),
                },
            ));
            bus.emit(boundary(false));
            let (_, _, history) = bus
                .subscribe_with_history(SubscriberRegistration::lag_tolerant(filter.clone(), 256))
                .unwrap();
            assert!(!history.complete);
            assert_eq!(history.events.len(), 2);
            let (client, server) = tokio::net::UnixStream::pair().unwrap();
            let (_, write) = server.into_split();
            let metadata = WatchMetadata::default();
            assert!(
                !write_watch_history(&SharedWriter::new(write), &filter, history, &metadata)
                    .await
                    .unwrap()
            );
            let mut reader = BufReader::new(client);
            assert_eq!(
                socket::read_json_line(&mut reader).await.unwrap().unwrap()["type"],
                "history_gap"
            );
            let chunk = socket::read_json_line(&mut reader).await.unwrap().unwrap();
            assert_eq!(chunk["type"], "stream_chunk");
            assert_eq!(chunk["sequence"], 2);
            assert_eq!(chunk["stage_type"], stage_type);
            assert_eq!(chunk["stage_run_id"], "one");
            assert_eq!(chunk["correlation_turn"], "caller-turn");
            assert_eq!(
                socket::read_json_line(&mut reader).await.unwrap().unwrap()["active"],
                false
            );
            let checkpoint = socket::read_json_line(&mut reader).await.unwrap().unwrap();
            assert_eq!(checkpoint["type"], "replay_checkpoint");
            assert_eq!(checkpoint["complete"], false);
            assert_eq!(checkpoint["through_sequence"], 3);
            assert_eq!(checkpoint["evicted_through_sequence"], 1);
            // Retrospective closing evidence must not classify later live frames.
            let after_close = metadata
                .frame(
                    &DispatchEvent::new(
                        corr.clone(),
                        EventKind::StreamChunk {
                            text: "after close".into(),
                        },
                    ),
                    &filter,
                )
                .unwrap();
            assert!(after_close.get("stage_type").is_none());
            for unrelated in [
                filter.clone().with_stage("worker").with_stage_run_id("two"),
                CorrId::new()
                    .with_run("truncated-watch:child")
                    .with_stage("worker")
                    .with_stage_run_id("one"),
            ] {
                let frame = metadata
                    .frame(
                        &DispatchEvent::new(
                            unrelated,
                            EventKind::StreamChunk {
                                text: "unknown".into(),
                            },
                        ),
                        &filter,
                    )
                    .unwrap();
                assert!(frame.get("stage_type").is_none());
            }
        }
    }

    #[tokio::test]
    async fn watch_truncated_replay_without_type_evidence_stays_unclassified() {
        use gremlins_magic_bus::{Bus, CorrId, SubscriberRegistration};
        let bus = Bus::with_history_limits(1, 1);
        let corr = CorrId::new()
            .with_run("unknown-watch")
            .with_stage("worker")
            .with_stage_run_id("one")
            .with_turn_token("turn");
        bus.emit(DispatchEvent::new(
            corr.clone(),
            EventKind::StageBoundary {
                id: "unknown-watch".into(),
                stage: "worker".into(),
                stage_run_id: "one".into(),
                stage_type: "agent".into(),
                active: true,
            },
        ));
        bus.emit(DispatchEvent::new(
            corr.clone(),
            EventKind::StreamChunk {
                text: "retained".into(),
            },
        ));
        let (_, _, history) = bus
            .subscribe_with_history(SubscriberRegistration::lag_tolerant(corr.clone(), 256))
            .unwrap();
        assert!(!history.complete);
        let (client, server) = tokio::net::UnixStream::pair().unwrap();
        let (_, write) = server.into_split();
        write_watch_history(
            &SharedWriter::new(write),
            &corr,
            history,
            &WatchMetadata::default(),
        )
        .await
        .unwrap();
        let mut reader = BufReader::new(client);
        assert_eq!(
            socket::read_json_line(&mut reader).await.unwrap().unwrap()["type"],
            "history_gap"
        );
        let frame = socket::read_json_line(&mut reader).await.unwrap().unwrap();
        assert_eq!(frame["type"], "stream_chunk");
        assert!(frame.get("stage_type").is_none());
        assert_eq!(frame["stage_run_id"], "one");
        assert_eq!(frame["correlation_turn"], "turn");
    }

    #[test]
    fn watch_stamped_closing_boundary_clears_fallback_metadata() {
        use gremlins_magic_bus::{Bus, CorrId, SubscriberRegistration};
        let filter = CorrId::new().with_run("closed-watch");
        let corr = filter.clone().with_stage("worker").with_stage_run_id("one");
        let bus =
            Bus::with_history_limits(4, 4).with_invocation_metadata(EventKind::invocation_metadata);
        for active in [true, false] {
            bus.emit(DispatchEvent::new(
                corr.clone(),
                EventKind::StageBoundary {
                    id: "closed-watch".into(),
                    stage: "worker".into(),
                    stage_run_id: "one".into(),
                    stage_type: "exec".into(),
                    active,
                },
            ));
        }
        let (_, _, history) = bus
            .subscribe_with_history(SubscriberRegistration::lossless(filter.clone()))
            .unwrap();
        let metadata = WatchMetadata::default();
        for dispatch in &history.events {
            // Exercise the replay prepass as well as envelope-stamped boundaries.
            metadata.record_boundary(dispatch, &filter);
        }
        for dispatch in &history.events {
            assert_eq!(dispatch.stage_type(), Some("exec"));
            assert_eq!(
                metadata.frame(dispatch, &filter).unwrap()["stage_type"],
                "exec"
            );
        }
        let frame = metadata
            .frame(
                &DispatchEvent::new(
                    corr,
                    EventKind::StreamChunk {
                        text: "late".into(),
                    },
                ),
                &filter,
            )
            .unwrap();
        assert!(frame.get("stage_type").is_none());
        assert!(metadata.stage_types.lock().unwrap().is_empty());
    }

    #[test]
    fn watch_boundary_metadata_requires_consistent_identity() {
        use gremlins_magic_bus::CorrId;
        let filter = CorrId::new().with_run("metadata-watch");
        let corr = filter.clone().with_stage("worker").with_stage_run_id("one");
        for (run, stage, invocation) in [
            ("other", "worker", "one"),
            ("metadata-watch", "container", "one"),
            ("metadata-watch", "worker", "other"),
        ] {
            let metadata = WatchMetadata::default();
            let bus = gremlins_magic_bus::Bus::with_history_limits(1, 1)
                .with_invocation_metadata(EventKind::invocation_metadata);
            let boundary = DispatchEvent::new(
                corr.clone(),
                EventKind::StageBoundary {
                    id: run.into(),
                    stage: stage.into(),
                    stage_run_id: invocation.into(),
                    stage_type: "agent".into(),
                    active: true,
                },
            );
            metadata.record_boundary(&boundary, &filter);
            bus.emit(boundary);
            bus.emit(DispatchEvent::new(
                corr.clone(),
                EventKind::StreamChunk {
                    text: "chunk".into(),
                },
            ));
            let (_, _, history) =
                bus.subscribe_with_history(
                    gremlins_magic_bus::SubscriberRegistration::lag_tolerant(filter.clone(), 16),
                )
                .unwrap();
            assert!(history.events[0].stage_type().is_none());
            let frame = metadata
                .frame(
                    &DispatchEvent::new(
                        corr.clone(),
                        EventKind::StreamChunk {
                            text: "unclassified".into(),
                        },
                    ),
                    &filter,
                )
                .unwrap();
            assert!(frame.get("stage_type").is_none());
        }
    }

    #[tokio::test]
    async fn watch_selector_queue_excludes_sibling_flood_and_orders_parent_lifecycle() {
        use gremlins_magic_bus::{Bus, CorrId, SubscriberRegistration, TryRecvError};
        for replay in [false, true] {
            let bus = Bus::new();
            let run = CorrId::new().with_run("selector-queue");
            let filter = run.clone().with_stage("build").with_stage_run_id("chosen");
            let selection = filter.clone();
            let registration = SubscriberRegistration::lag_tolerant(run.clone(), 4);
            let accepts = move |event: &DispatchEvent<EventKind>| watch_accepts(event, &selection);
            let publish = || {
                bus.emit(DispatchEvent::new(
                    run.clone(),
                    EventKind::RunStarted {
                        id: "selector-queue".into(),
                        definition: "test".into(),
                        stage: "build".into(),
                    },
                ));
                for _ in 0..1024 {
                    bus.emit(DispatchEvent::new(
                        run.clone().with_stage("other"),
                        EventKind::StreamChunk {
                            text: "sibling".into(),
                        },
                    ));
                }
                bus.emit(DispatchEvent::new(
                    filter.clone(),
                    EventKind::StreamChunk {
                        text: "chosen".into(),
                    },
                ));
                bus.emit(DispatchEvent::new(
                    run.clone(),
                    EventKind::RunStopped {
                        id: "selector-queue".into(),
                    },
                ));
            };
            if replay {
                publish();
            }
            let (_sub, mut rx, history) = bus
                .subscribe_with_history_filtered(registration, accepts)
                .unwrap();
            let events = if replay {
                assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
                history.events
            } else {
                assert!(history.events.is_empty());
                publish();
                let mut events = Vec::new();
                for _ in 0..3 {
                    let Recv::Event(event) = rx.recv().await else {
                        panic!("sibling flood caused lag");
                    };
                    events.push(event);
                }
                assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
                events
            };
            assert_eq!(events.len(), 3);
            assert!(matches!(events[0].kind(), EventKind::RunStarted { .. }));
            assert!(
                matches!(events[1].kind(), EventKind::StreamChunk { text } if text == "chosen")
            );
            assert!(matches!(events[2].kind(), EventKind::RunStopped { .. }));
            assert!(events
                .windows(2)
                .all(|pair| pair[0].sequence() < pair[1].sequence()));
        }
    }

    #[tokio::test]
    async fn watch_replay_restart_clears_only_run_terminal() {
        use gremlins_magic_bus::{Bus, CorrId, SubscriberRegistration};
        for closed in [false, true] {
            let bus = Bus::new();
            let run = CorrId::new().with_run("restart-regression");
            let filter = run.clone().with_stage("build").with_stage_run_id("chosen");
            if closed {
                bus.emit(DispatchEvent::new(
                    filter.clone(),
                    EventKind::StageBoundary {
                        id: "restart-regression".into(),
                        stage: "build".into(),
                        stage_run_id: "chosen".into(),
                        stage_type: "exec".into(),
                        active: false,
                    },
                ));
            }
            bus.emit(DispatchEvent::new(
                run.clone(),
                EventKind::RunStopped {
                    id: "restart-regression".into(),
                },
            ));
            bus.emit(DispatchEvent::new(
                run.clone(),
                EventKind::RunStarted {
                    id: "restart-regression".into(),
                    definition: "test".into(),
                    stage: "build".into(),
                },
            ));
            let selection = filter.clone();
            let (_, _, history) = bus
                .subscribe_with_history_filtered(
                    SubscriberRegistration::lag_tolerant(run, 4),
                    move |event| watch_accepts(event, &selection),
                )
                .unwrap();
            let (client, server) = tokio::net::UnixStream::pair().unwrap();
            let (_, write) = server.into_split();
            let writer = SharedWriter::new(write);
            assert_eq!(
                write_watch_history(&writer, &filter, history, &WatchMetadata::default())
                    .await
                    .unwrap(),
                closed
            );
            drop(client);
        }
    }

    #[tokio::test]
    async fn watch_supervisor_parent_replay_preserves_distinct_child_sources() {
        use gremlins_magic_bus::{emit_corr_event, CorrId};
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let sandbox = tempfile::tempdir().unwrap();
            let id = format!("watch-parent-{}", crate::executor::state::token_hex(8));
            let gremlin = crate::executor::supervisor::chat::create_chat_gremlin(
                &id,
                &crate::prelude::ModelURI::parse("cmd/echo").unwrap(),
                "helpful",
                sandbox.path(),
                &Default::default(),
            )
            .unwrap();
            let registry = Arc::new(RunRegistry::new(tokio::sync::watch::channel(false).0));
            registry
                .register_chat_gremlin(
                    id.clone(),
                    gremlin,
                    crate::clients::cancel::CancelToken::new(),
                )
                .await;
            let children = [format!("{id}:left"), format!("{id}:right")];
            for child in &children {
                emit_corr_event(
                    &CorrId::new()
                        .with_run(child)
                        .with_stage("build")
                        .with_stage_run_id(format!("{child}-invocation")),
                    EventKind::StreamChunk {
                        text: child.clone(),
                    },
                );
            }
            let (client, server) = tokio::net::UnixStream::pair().unwrap();
            let (read, write) = server.into_split();
            let writer = SharedWriter::new(write);
            let task = tokio::spawn(async move {
                handle_watch(
                    &serde_json::json!({"op":"watch", "id":id, "stage":"build", "replay":true}),
                    &registry,
                    BufReader::new(read),
                    &writer,
                )
                .await;
            });
            let mut reader = BufReader::new(client);
            assert_eq!(
                socket::read_json_line(&mut reader).await.unwrap().unwrap()["type"],
                "watched"
            );
            let mut sources = Vec::new();
            let mut sequences = Vec::new();
            loop {
                let frame = socket::read_json_line(&mut reader).await.unwrap().unwrap();
                if frame["type"] == "replay_checkpoint" {
                    break;
                }
                if frame["type"] == "history_gap" {
                    continue;
                }
                assert_eq!(frame["type"], "stream_chunk");
                assert_eq!(frame["text"], frame["run_id"]);
                assert_eq!(
                    frame["stage_run_id"],
                    format!("{}-invocation", frame["run_id"].as_str().unwrap())
                );
                sources.push(frame["run_id"].as_str().unwrap().to_owned());
                sequences.push(frame["sequence"].as_u64().unwrap());
            }
            assert_eq!(sources, children);
            assert!(sequences[0] < sequences[1]);
            drop(reader);
            task.await.unwrap();
        })
        .await
        .unwrap();
    }

    #[test]
    fn watch_terminal_identity_and_restart_rules() {
        use gremlins_magic_bus::CorrId;
        let stage = CorrId::new().with_run("parent").with_stage("build");
        let exact = stage.clone().with_stage_run_id("chosen");
        let metadata = WatchMetadata::default();
        let stream = PassiveStream::Watch(&exact, &metadata);
        for (run, invocation, payload_invocation, expected) in [
            ("parent", "chosen", "chosen", Some(true)),
            ("parent:child", "chosen", "chosen", Some(true)),
            ("parent:childish", "other", "chosen", None),
            ("foreign", "chosen", "chosen", None),
            ("parent", "chosen", "other", None),
        ] {
            let dispatch = DispatchEvent::new(
                CorrId::new()
                    .with_run(run)
                    .with_stage("build")
                    .with_stage_run_id(invocation),
                EventKind::StageBoundary {
                    id: run.into(),
                    stage: "build".into(),
                    stage_run_id: payload_invocation.into(),
                    stage_type: "agent".into(),
                    active: false,
                },
            );
            assert_eq!(stream.terminal_update(&dispatch), expected);
            assert_eq!(
                PassiveStream::Watch(&stage, &metadata).terminal_update(&dispatch),
                None
            );
        }
        let started = DispatchEvent::new(
            CorrId::new().with_run("parent"),
            EventKind::RunStarted {
                id: "parent".into(),
                definition: "test".into(),
                stage: "build".into(),
            },
        );
        assert_eq!(stream.terminal_update(&started), Some(false));
        assert_eq!(
            PassiveStream::Watch(&stage, &metadata).terminal_update(&started),
            Some(false)
        );
    }

    #[tokio::test]
    async fn watch_run_wide_native_replay_and_live_termination() {
        use gremlins_magic_bus::{Bus, CorrId, SubscriberRegistration};
        use std::time::Duration;

        // Exercise the native subscription, not a fabricated history snapshot.
        // Both delivery paths must see lifecycle outside the display selector.
        for replay in [false, true] {
            for exact in [false, true] {
                for terminal_kind in 0..3 {
                    let bus = Bus::new();
                    let run = CorrId::new().with_run("watch-lifecycle");
                    let stage = run.clone().with_stage("build");
                    let selected = stage.clone().with_stage_run_id("selected");
                    let filter = if exact {
                        selected.clone()
                    } else {
                        stage.clone()
                    };
                    let sibling = run.clone().with_stage("test").with_stage_run_id("sibling");
                    let boundary = |corr, name: &str, invocation: &str| {
                        DispatchEvent::new(
                            corr,
                            EventKind::StageBoundary {
                                id: "watch-lifecycle".into(),
                                stage: name.into(),
                                stage_run_id: invocation.into(),
                                stage_type: "agent".into(),
                                active: false,
                            },
                        )
                    };
                    let mut events = vec![
                        DispatchEvent::new(
                            sibling.clone(),
                            EventKind::StreamChunk {
                                text: "never displayed".into(),
                            },
                        ),
                        boundary(sibling, "test", "sibling"),
                        DispatchEvent::new(
                            selected.clone(),
                            EventKind::StreamChunk {
                                text: "selected output".into(),
                            },
                        ),
                        boundary(selected, "build", "selected"),
                    ];
                    if !exact {
                        events.push(DispatchEvent::new(
                            stage.with_stage_run_id("next"),
                            EventKind::StreamChunk {
                                text: "after boundary".into(),
                            },
                        ));
                        let terminal = match terminal_kind {
                            0 => EventKind::RunStopped {
                                id: "watch-lifecycle".into(),
                            },
                            1 => EventKind::RunCompleted {
                                id: "watch-lifecycle".into(),
                                exit_code: 0,
                            },
                            _ => EventKind::RunFailed {
                                id: "watch-lifecycle".into(),
                                exit_code: 1,
                                error: None,
                            },
                        };
                        events.push(DispatchEvent::new(run.clone(), terminal));
                    }
                    if replay {
                        for event in &events {
                            bus.emit(event.clone());
                        }
                    }
                    let (_sub, mut rx, history) = bus
                        .subscribe_with_history(SubscriberRegistration::lag_tolerant(run, 256))
                        .unwrap();
                    let (client, server) = tokio::net::UnixStream::pair().unwrap();
                    let (_, write) = server.into_split();
                    let writer = SharedWriter::new(write);
                    let metadata = WatchMetadata::default();
                    if replay {
                        assert!(write_watch_history(&writer, &filter, history, &metadata)
                            .await
                            .unwrap());
                    } else {
                        assert!(!write_watch_history(&writer, &filter, history, &metadata)
                            .await
                            .unwrap());
                        for (index, event) in events.iter().enumerate() {
                            bus.emit_async(event.clone()).await;
                            let ended = forward_passive(
                                &writer,
                                PassiveStream::Watch(&filter, &metadata),
                                rx.recv().await,
                            )
                            .await
                            .unwrap();
                            assert_eq!(ended, index == events.len() - 1);
                        }
                    }
                    drop(writer);
                    let mut reader = BufReader::new(client);
                    let frames = tokio::time::timeout(Duration::from_secs(5), async {
                        let mut frames = Vec::new();
                        while let Some(frame) = socket::read_json_line(&mut reader).await.unwrap() {
                            frames.push(frame);
                        }
                        frames
                    })
                    .await
                    .unwrap();
                    let kinds: Vec<_> = frames
                        .iter()
                        .map(|frame| frame["type"].as_str().unwrap())
                        .collect();
                    let mut expected = vec!["stream_chunk", "stage_boundary"];
                    if !exact {
                        expected.push("stream_chunk");
                    }
                    if replay {
                        expected.push("replay_checkpoint");
                    } else {
                        expected.insert(0, "replay_checkpoint");
                    }
                    // No input protocol frames or sibling/parent frames leak.
                    assert_eq!(kinds, expected);
                    assert_eq!(
                        frames
                            .iter()
                            .filter(|f| f["text"] == "selected output")
                            .count(),
                        1
                    );
                    let checkpoint = frames
                        .iter()
                        .find(|f| f["type"] == "replay_checkpoint")
                        .unwrap();
                    assert_eq!(
                        checkpoint["through_sequence"],
                        if replay { events.len() } else { 0 }
                    );
                    if replay {
                        assert_eq!(frames.last().unwrap()["type"], "replay_checkpoint");
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn watch_gap_keeps_native_watermarks_and_parent_terminal() {
        use gremlins_magic_bus::{Bus, CorrId, SubscriberRegistration};
        let bus = Bus::with_history_limits(1, 1);
        let filter = CorrId::new().with_run("watch-gap");
        bus.emit(DispatchEvent::new(
            filter.clone(),
            EventKind::StreamChunk {
                text: "evicted".into(),
            },
        ));
        bus.emit(DispatchEvent::new(
            filter.clone(),
            EventKind::RunStopped {
                id: "watch-gap".into(),
            },
        ));
        let (_, _, history) = bus
            .subscribe_with_history(SubscriberRegistration::lag_tolerant(filter.clone(), 256))
            .unwrap();
        let (client, server) = tokio::net::UnixStream::pair().unwrap();
        let (_, write) = server.into_split();
        let writer = SharedWriter::new(write);
        assert!(
            write_watch_history(&writer, &filter, history, &WatchMetadata::default())
                .await
                .unwrap()
        );
        let mut reader = BufReader::new(client);
        let gap = socket::read_json_line(&mut reader).await.unwrap().unwrap();
        assert_eq!(gap["type"], "history_gap");
        assert_eq!(gap["through_sequence"], 2);
        assert_eq!(gap["evicted_through_sequence"], 1);
        let terminal = socket::read_json_line(&mut reader).await.unwrap().unwrap();
        assert_eq!(terminal["type"], "run_stopped");
        assert_eq!(terminal["sequence"], 2);
        let checkpoint = socket::read_json_line(&mut reader).await.unwrap().unwrap();
        assert_eq!(checkpoint["type"], "replay_checkpoint");
        assert_eq!(checkpoint["complete"], false);
        assert_eq!(checkpoint["through_sequence"], 2);
        assert_eq!(checkpoint["evicted_through_sequence"], 1);
    }

    #[tokio::test]
    async fn watch_native_replay_buffers_live_output_while_history_is_blocked() {
        use gremlins_magic_bus::{Bus, CorrId, SubscriberRegistration, TryRecvError};
        use std::time::Duration;

        for overflow in [false, true] {
            let bus = Bus::with_history_limits(2, 3);
            let filter = CorrId::new()
                .with_run("watch-blocked")
                .with_stage("build")
                .with_stage_run_id("execution");
            let foreign = CorrId::new().with_run("other-run");
            let payload = "repeated output\n".repeat(512 * 1024);
            let output = EventKind::StreamChunk {
                text: payload.clone(),
            };
            for (corr, kind) in [
                (
                    foreign.clone(),
                    EventKind::StreamChunk {
                        text: "evicted".into(),
                    },
                ),
                (filter.clone(), output.clone()),
                (
                    foreign.clone(),
                    EventKind::StreamChunk {
                        text: "foreign".into(),
                    },
                ),
                (
                    foreign.clone(),
                    EventKind::StreamChunk {
                        text: "foreign tail".into(),
                    },
                ),
            ] {
                bus.emit(DispatchEvent::new(corr, kind));
            }
            let (_sub, mut rx, history) = bus
                .subscribe_with_history(SubscriberRegistration::lag_tolerant(filter.clone(), 2))
                .unwrap();
            assert!(!history.complete);
            assert_eq!(history.through_sequence, 4);
            assert_eq!(history.evicted_through_sequence, 1);
            assert_eq!(history.events.len(), 1);
            assert_eq!(history.events[0].sequence(), 2);
            assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));

            let (client, server) = tokio::net::UnixStream::pair().unwrap();
            let (_, write) = server.into_split();
            let writer = SharedWriter::new(write);
            let replay_filter = filter.clone();
            let (blocked_tx, blocked_rx) = tokio::sync::oneshot::channel();
            let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
            let send = tokio::spawn(async move {
                let metadata = WatchMetadata::default();
                let replay = write_watch_history(&writer, &replay_filter, history, &metadata);
                tokio::pin!(replay);
                // Keep the same future alive: the undrained socket cannot fit this output.
                assert!(tokio::time::timeout(Duration::from_millis(50), &mut replay)
                    .await
                    .is_err());
                blocked_tx.send(()).unwrap();
                resume_rx.await.unwrap();
                assert!(!replay.await.unwrap());
                if overflow {
                    assert!(matches!(rx.recv().await, Recv::Lagged(1)));
                    writer
                        .write_json_line(&serde_json::json!({
                            "type": "lagged", "skipped": 1,
                        }))
                        .await
                        .unwrap();
                }
                for _ in 0..if overflow { 2 } else { 1 } {
                    let Recv::Event(dispatch) = rx.recv().await else {
                        panic!("expected native live output");
                    };
                    writer
                        .write_json_line(&watch_frame(&dispatch, &replay_filter).unwrap())
                        .await
                        .unwrap();
                }
                assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
            });
            tokio::time::timeout(Duration::from_secs(5), blocked_rx)
                .await
                .unwrap()
                .unwrap();
            let count = if overflow { 3 } else { 1 };
            for _ in 0..count {
                bus.emit_async(DispatchEvent::new(filter.clone(), output.clone()))
                    .await;
            }
            bus.emit_async(DispatchEvent::new(
                foreign,
                EventKind::StreamChunk {
                    text: "filtered live output".into(),
                },
            ))
            .await;
            resume_tx.send(()).unwrap();

            let mut reader = BufReader::new(client);
            let mut expected = vec![
                serde_json::json!({"type": "history_gap", "reason": "history_incomplete",
                    "through_sequence": 4, "evicted_through_sequence": 1}),
                serde_json::json!({"type": "stream_chunk", "text": payload, "sequence": 2,
                    "run_id": "watch-blocked", "stage": "build", "stage_run_id": "execution"}),
                serde_json::json!({"type": "replay_checkpoint", "id": "watch-blocked", "scope": "run",
                    "stage": "build", "stage_run_id": "execution", "complete": false,
                    "through_sequence": 4, "evicted_through_sequence": 1}),
            ];
            if overflow {
                expected.push(serde_json::json!({"type": "lagged", "skipped": 1}));
            }
            for sequence in if overflow { 6..=7 } else { 5..=5 } {
                expected.push(serde_json::json!({"type": "stream_chunk", "text": payload,
                    "sequence": sequence, "run_id": "watch-blocked", "stage": "build",
                    "stage_run_id": "execution"}));
            }
            tokio::time::timeout(Duration::from_secs(10), async {
                for frame in expected {
                    assert_eq!(
                        socket::read_json_line(&mut reader).await.unwrap().unwrap(),
                        frame
                    );
                }
                send.await.unwrap();
                assert!(socket::read_json_line(&mut reader).await.unwrap().is_none());
            })
            .await
            .unwrap();
        }
    }
}
