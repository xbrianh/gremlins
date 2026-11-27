//! Passive session_chat display transport, separate from the enqueue-only request socket.
use super::*;

#[cfg(test)]
#[path = "session_chat_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "session_chat_reservation_tests.rs"]
mod reservation_tests;

#[cfg(test)]
#[path = "session_chat_live_warning_tests.rs"]
mod live_warning_tests;

#[cfg(test)]
#[path = "session_chat_review_tests.rs"]
mod review_tests;

struct Reader(tokio::task::JoinHandle<()>);

impl Reader {
    fn abort(&self) {
        self.0.abort();
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.abort();
    }
}

struct SessionChatWatchContext {
    session_write: Arc<Mutex<WriteState>>,
    out: Arc<OutState>,
    generation: u64,
    run: String,
    turn: Option<String>,
}

#[derive(Default)]
struct WatchGate {
    pending: bool,
    recovering: bool,
    acknowledged: bool,
    disconnected: bool,
    ready: Arc<tokio::sync::Notify>,
    turn: Option<String>,
    rejected: Option<String>,
    frames: VecDeque<Value>,
}

struct Submission(
    Arc<std::sync::Mutex<WatchGate>>,
    Arc<tokio::sync::Notify>,
    Option<String>,
    bool,
);

impl Drop for Submission {
    fn drop(&mut self) {
        if !self.3 {
            return;
        }
        let mut gate = self.0.lock().unwrap();
        if gate.pending {
            let rejected = gate.turn.clone();
            gate.frames.retain(|frame| {
                !matches!(
                    frame["type"].as_str(),
                    Some("lagged" | "history_gap" | "session_chat_transport_warning")
                ) && (session_chat_turn(frame).is_none() || session_chat_turn(frame) != rejected)
            });
            gate.rejected = rejected;
            gate.turn = self.2.clone();
            gate.pending = false;
        }
        self.1.notify_one();
    }
}

pub(super) struct Display {
    forward: tokio::task::JoinHandle<()>,
    generation: u64,
    run: String,
    gate: Arc<std::sync::Mutex<WatchGate>>,
    changed: Arc<tokio::sync::Notify>,
}

impl Drop for Display {
    fn drop(&mut self) {
        self.forward.abort();
        self.gate.lock().unwrap().ready.notify_waiters();
    }
}

impl DaemonClient {
    pub(super) async fn send_session_chat_or_session(
        &self,
        request: Value,
        generation: Option<u64>,
    ) -> Result<Value, String> {
        if let Some(expected) = generation {
            if self.session.current_generation().await != expected {
                return Err("session was recreated".into());
            }
        }
        let session_chat = request["op"] == "chat" && request.get("target").is_none();
        if !session_chat {
            return match generation {
                Some(generation) => {
                    self.session
                        .send_on_session_for_generation(request, generation)
                        .await
                }
                None => self.session.send_on_session(request).await,
            };
        }
        let (expected_generation, session_id, id) = {
            let write = self.session.write.lock().await;
            if generation.is_some_and(|expected| expected != write.generation) {
                return Err("session was recreated".into());
            }
            (
                write.generation,
                write.session_id.clone().unwrap_or_default(),
                write.chat_run_id.clone().unwrap_or_default(),
            )
        };
        let turn = request["turn"].as_str().map(str::to_owned);
        let reusable = loop {
            let guard = self.session_chat_display.lock().await;
            let Some(display) = guard.as_ref().filter(|display| {
                display.generation == expected_generation
                    && display.run == id
                    && !display.forward.is_finished()
            }) else {
                break false;
            };
            let ready = Arc::clone(&display.gate.lock().unwrap().ready);
            let notified = ready.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let gate = display.gate.lock().unwrap();
                if !gate.recovering {
                    break !gate.disconnected;
                }
            }
            drop(guard);
            // Do not hold the display lock while the recovery handshake runs.
            notified.await;
        };
        if !reusable {
            let gate = Arc::new(std::sync::Mutex::new(WatchGate {
                pending: true,
                turn: turn.clone(),
                ..WatchGate::default()
            }));
            let prepared = self
                .prepare_session_chat_gated(&id, false, Some(Arc::clone(&gate)))
                .await;
            self.install_session_chat(
                prepared,
                expected_generation,
                session_id.clone(),
                id.clone(),
                turn.clone(),
                gate,
            )
            .await?;
        }
        let mut submission = loop {
            let guard = self.session_chat_display.lock().await;
            let write = self.session.write.lock().await;
            if write.generation != expected_generation {
                return Err("session_chat display belongs to a retired session".into());
            }
            let display = guard.as_ref().ok_or("session_chat display retired")?;
            let ready = Arc::clone(&display.gate.lock().unwrap().ready);
            let notified = ready.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let submission = {
                let mut gate = display.gate.lock().unwrap();
                if gate.recovering {
                    None
                } else {
                    if gate.pending && reusable {
                        return Err("session_chat submission already pending".into());
                    }
                    let previous = gate.turn.clone();
                    gate.pending = true;
                    gate.turn = turn.clone();
                    Some(Submission(
                        Arc::clone(&display.gate),
                        Arc::clone(&display.changed),
                        previous,
                        true,
                    ))
                }
            };
            if let Some(submission) = submission {
                break submission;
            }
            drop(write);
            drop(guard);
            notified.await;
        };
        let response = match self
            .session
            .send_on_session_for_generation(request, expected_generation)
            .await
        {
            Ok(response) => response,
            Err(error) => return Err(error),
        };
        if response["type"] == "error" {
            return Ok(response);
        }
        let mut write = self.session.write.lock().await;
        if write.generation != expected_generation
            || write.session_id.as_deref() != Some(&session_id)
            || response["session_id"]
                .as_str()
                .is_some_and(|id| id != session_id)
        {
            return Err("session_chat acknowledgment belongs to a retired session".into());
        }
        if response["type"] != "ok" || response["enqueued"] != true {
            return Err("unexpected session_chat enqueue acknowledgment".into());
        }
        let actual = response["chat_run_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "session_chat enqueue acknowledgment has no run id".to_owned())?
            .to_owned();
        write.chat_run_id = Some(actual.clone());
        let _ = self
            .out
            .raw_tx
            .send(json!({"type":"session_chat_run_identity", "id":actual,
            "turn":turn, "session_id":session_id,
            "generation":expected_generation}));
        drop(write);
        if actual == id {
            let mut gate = submission.0.lock().unwrap();
            gate.pending = false;
            gate.acknowledged = true;
            // An enqueue acknowledgment does not repair a disconnected socket.
            // Only a successful passive recovery may clear `disconnected`.
            submission.3 = false;
            drop(gate);
            submission.1.notify_one();
        } else {
            {
                let mut display = self.session_chat_display.lock().await;
                let write = self.session.write.lock().await;
                if write.generation != expected_generation {
                    return Err("session_chat display belongs to a retired session".into());
                }
                display.take();
            }
            drop(submission);
            let gate = Arc::new(std::sync::Mutex::new(WatchGate {
                acknowledged: true,
                turn: turn.clone(),
                ..WatchGate::default()
            }));
            let stream = self
                .prepare_session_chat_gated(&actual, true, Some(Arc::clone(&gate)))
                .await;
            self.install_session_chat(stream, expected_generation, session_id, actual, turn, gate)
                .await?;
        }
        Ok(response)
    }

    async fn install_session_chat(
        &self,
        stream: Result<(mpsc::Receiver<Value>, Reader), String>,
        expected_generation: u64,
        session_id: String,
        actual: String,
        turn: Option<String>,
        gate: Arc<std::sync::Mutex<WatchGate>>,
    ) -> Result<(), String> {
        let mut initial_warning = None;
        let (rx, reader) = match stream {
            Ok(stream) => stream,
            Err(error) => {
                gate.lock().unwrap().disconnected = true;
                initial_warning = Some(format!("session_chat display unavailable: {error}"));
                let (tx, rx) = mpsc::channel(1);
                drop(tx);
                (rx, Reader(tokio::spawn(async {})))
            }
        };
        let mut display = self.session_chat_display.lock().await;
        let write = self.session.write.lock().await;
        // Validate before publishing anything; on failure Reader's Drop closes
        // the candidate socket without touching an installed successor display.
        if write.generation != expected_generation
            || write.session_id.as_deref() != Some(&session_id)
        {
            return Err("session_chat display belongs to a retired session".into());
        }
        if display.as_ref().is_some_and(|display| {
            let gate = display.gate.lock().unwrap();
            !display.forward.is_finished()
                && (gate.pending || gate.recovering || !gate.disconnected)
        }) {
            return Err("session_chat watch already installed".into());
        }
        let session_write = Arc::clone(&self.session.write);
        let out = Arc::clone(&self.out);
        #[cfg(test)]
        let recovery_streams = Arc::clone(&self.aux_streams);

        let changed = Arc::new(tokio::sync::Notify::new());
        let recovery_run = actual.clone();
        let recovery_gate = Arc::clone(&gate);
        let forward = tokio::spawn(forward_session_chat_gated(
            rx,
            reader,
            SessionChatWatchContext {
                session_write,
                out,
                generation: expected_generation,
                run: actual.clone(),
                turn,
            },
            initial_warning,
            Some((Arc::clone(&gate), Arc::clone(&changed))),
            move || {
                let session_id = session_id.clone();
                let gate = Arc::clone(&recovery_gate);
                let actual = recovery_run.clone();
                #[cfg(test)]
                let recovery_streams = Arc::clone(&recovery_streams);
                async move {
                    #[cfg(test)]
                    let stream = recovery_streams
                        .lock()
                        .await
                        .as_mut()
                        .and_then(VecDeque::pop_front)
                        .ok_or_else(|| "no fake auxiliary stream available".to_owned())?;
                    #[cfg(not(test))]
                    let stream = socket::connect_session_socket(
                        &gremlins::config::state_root(),
                        &session_id,
                    )
                    .await?;
                    let (rx, reader) = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        follow_watch_mapped(
                            stream,
                            &session_id,
                            &actual,
                            None,
                            None,
                            true,
                            session_chat_frame_receiver(true, Some(gate)),
                        ),
                    )
                    .await
                    .map_err(|_| "session_chat watch acknowledgment timed out".to_owned())??;
                    Ok((rx, Reader(reader)))
                }
            },
        ));
        *display = Some(Display {
            forward,
            generation: expected_generation,
            run: actual,
            gate,
            changed,
        });
        Ok(())
    }

    #[cfg(test)]
    async fn prepare_session_chat(
        &self,
        id: &str,
        replay: bool,
    ) -> Result<(mpsc::Receiver<Value>, Reader), String> {
        self.prepare_session_chat_gated(id, replay, None).await
    }

    async fn prepare_session_chat_gated(
        &self,
        id: &str,
        replay: bool,
        gate: Option<Arc<std::sync::Mutex<WatchGate>>>,
    ) -> Result<(mpsc::Receiver<Value>, Reader), String> {
        let stream = self.open_aux_stream().await?;
        let session = self.session_id().await;
        let (rx, reader) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            follow_watch_mapped(
                stream,
                &session,
                id,
                None,
                None,
                replay,
                session_chat_frame_receiver(replay, gate),
            ),
        )
        .await
        .map_err(|_| "session_chat watch acknowledgment timed out".to_owned())??;
        Ok((rx, Reader(reader)))
    }
}

#[cfg(test)]
async fn forward_session_chat_stream<F, Fut>(
    rx: mpsc::Receiver<Value>,
    reader: Reader,
    context: SessionChatWatchContext,
    initial_warning: Option<String>,
    recover: F,
) where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(mpsc::Receiver<Value>, Reader), String>>,
{
    forward_session_chat_gated(rx, reader, context, initial_warning, None, recover).await;
}

fn session_chat_frame_receiver(
    mut replay: bool,
    gate: Option<Arc<std::sync::Mutex<WatchGate>>>,
) -> impl FnMut(Value) -> Option<Value> + Send {
    move |mut frame| {
        if frame["type"] == "replay_checkpoint" {
            replay = false;
        }
        if !replay
            && matches!(frame["type"].as_str(), Some("lagged" | "history_gap"))
            && session_chat_turn(&frame).is_none()
        {
            if let Some(gate) = &gate {
                // Completed/idle turns have no warning owner. Drop these at
                // ingress so a later submission cannot adopt a stale warning.
                let turn = gate.lock().unwrap().turn.clone()?;
                frame["turn"] = json!(turn);
            }
        }
        Some(frame)
    }
}

type Gate = (Arc<std::sync::Mutex<WatchGate>>, Arc<tokio::sync::Notify>);

/// Bus sequences are monotonic across turns, so replay needs only one cursor.
#[derive(Default)]
struct ReplayCursor(Option<u64>);

impl ReplayCursor {
    fn accept(&mut self, sequence: u64) -> bool {
        if self.0.is_some_and(|seen| sequence <= seen) {
            return false;
        }
        self.0 = Some(sequence);
        true
    }
}

async fn forward_session_chat_gated<F, Fut>(
    mut rx: mpsc::Receiver<Value>,
    mut reader: Reader,
    context: SessionChatWatchContext,
    initial_warning: Option<String>,
    gate: Option<Gate>,
    mut recover: F,
) where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(mpsc::Receiver<Value>, Reader), String>>,
{
    let SessionChatWatchContext {
        session_write,
        out,
        generation,
        run,
        turn,
    } = context;
    let mut warned = false;
    let mut backoff = std::time::Duration::from_millis(50);
    let mut cursor = ReplayCursor::default();
    // Subscription gaps belong to the turn that lost the watch, even if a new
    // enqueue is accepted before replay has drained.
    let mut recovery_turn = gate.as_ref().and_then(|(state, _)| {
        let state = state.lock().unwrap();
        state.acknowledged.then(|| turn.clone())
    });
    let mut acquisition_retry = initial_warning.is_some();
    let mut finished_turn = None;
    loop {
        if let Some((state, changed)) = &gate {
            let full = {
                let state = state.lock().unwrap();
                state.pending && state.frames.len() >= 256
            };
            if full {
                changed.notified().await;
                continue;
            }
        }
        let queued = gate.as_ref().and_then(|(gate, _)| {
            let mut gate = gate.lock().unwrap();
            if gate.pending {
                None
            } else {
                gate.frames.pop_front()
            }
        });
        let from_queue = queued.is_some();
        let next = if let Some(frame) = queued {
            Some(frame)
        } else if let Some((_, changed)) = &gate {
            tokio::select! {
                frame = rx.recv() => frame,
                _ = changed.notified() => continue,
            }
        } else {
            rx.recv().await
        };
        let frame = if let Some(frame) = next {
            frame
        } else {
            {
                let write = session_write.lock().await;
                if write.generation != generation {
                    return;
                }
            }
            reader.abort();
            if let Some((state, changed)) = &gate {
                let pending = {
                    let mut state = state.lock().unwrap();
                    state.disconnected = true;
                    // A reserved run may not be watchable until chat starts it.
                    state.recovering = state.acknowledged;
                    recovery_turn = Some(state.turn.clone());
                    state.pending || !state.acknowledged
                };
                if pending {
                    changed.notified().await;
                    continue;
                }
            }
            // A refused reservation is expected before its first enqueue.
            // Give replay one chance before reporting an acquisition failure.
            if acquisition_retry {
                acquisition_retry = false;
                if let Ok((next_rx, next_reader)) = recover().await {
                    rx = next_rx;
                    reader = next_reader;
                    if let Some((state, _)) = &gate {
                        let mut state = state.lock().unwrap();
                        state.recovering = false;
                        state.disconnected = false;
                        state.ready.notify_waiters();
                    }
                    continue;
                }
            }
            if !warned {
                let write = session_write.lock().await;
                if write.generation != generation {
                    return;
                }
                let message = initial_warning
                    .as_deref()
                    .unwrap_or("session_chat display connection closed before turn completion");
                if let Some((gate, _)) = &gate {
                    let mut gate = gate.lock().unwrap();
                    if gate.pending {
                        let turn = gate.turn.clone();
                        gate.frames.push_back(json!({"type":"session_chat_transport_warning", "turn":turn, "message":message}));
                    } else if gate.turn.is_some() {
                        session_chat_watch_warning(&out, gate.turn.clone(), message);
                    }
                } else {
                    session_chat_watch_warning(&out, turn.clone(), message);
                }
                warned = true;
            }
            if let Ok((next_rx, next_reader)) = recover().await {
                rx = next_rx;
                reader = next_reader;
                if let Some((state, _)) = &gate {
                    let mut state = state.lock().unwrap();
                    state.recovering = false;
                    state.disconnected = false;
                    state.ready.notify_waiters();
                }
                continue;
            }
            if let Some((state, _)) = &gate {
                let mut state = state.lock().unwrap();
                if state.turn.is_none() && !state.pending {
                    // Failed idle recovery must not block the next enqueue.
                    state.recovering = false;
                    state.ready.notify_waiters();
                    return;
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(std::time::Duration::from_secs(5));
            continue;
        };
        let mut frame = frame;
        // Queued frames already passed checkpoint/warning attribution on
        // ingress below; never attribute them again after pending is released.
        if !from_queue {
            if frame["type"] == "replay_checkpoint" {
                recovery_turn = None;
            }
            if matches!(frame["type"].as_str(), Some("lagged" | "history_gap"))
                && session_chat_turn(&frame).is_none()
            {
                match &recovery_turn {
                    Some(None) => continue,
                    Some(Some(turn)) => frame["turn"] = json!(turn),
                    None => {}
                }
            }
        }
        let write = session_write.lock().await;
        if write.generation != generation {
            return;
        }
        if !from_queue {
            if let Some((state, _)) = &gate {
                let mut state = state.lock().unwrap();
                if !state.frames.is_empty() {
                    state.frames.push_back(frame);
                    continue;
                }
            }
        }
        let frame_turn = session_chat_turn(&frame);
        let active_turn = if let Some((gate, _)) = &gate {
            let mut gate = gate.lock().unwrap();
            if gate.pending {
                gate.frames.push_back(frame);
                continue;
            }
            if gate.rejected.is_some() && frame_turn == gate.rejected {
                continue;
            }
            gate.turn.clone()
        } else {
            turn.clone()
        };
        // A per-turn ended after completion is not a run terminal. While
        // idle it adds no information; late terminals during B keep A's identity.
        if frame["type"] == "ended"
            && active_turn.is_none()
            && frame_turn.is_some()
            && frame_turn == finished_turn
        {
            continue;
        }
        let terminal = (frame["run_id"] == run || frame["id"] == run)
            && (matches!(
                frame["type"].as_str(),
                Some("run_completed" | "run_failed" | "run_stopped")
            ) || (frame["type"] == "ended" && frame_turn.is_none()));
        let turn_finished = frame["run_id"] == run
            && matches!(frame["type"].as_str(), Some("done" | "ended"))
            && frame_turn == active_turn;
        if frame["run_id"] == run {
            // attachment::watch_frame emits `sequence` (not `seq`).
            if let Some(seq) = frame["sequence"].as_u64() {
                if !cursor.accept(seq) {
                    continue;
                }
            }
        }
        if matches!(frame["type"].as_str(), Some("lagged" | "history_gap"))
            && active_turn.is_none()
            && frame_turn.is_none()
        {
            continue;
        }
        if frame["type"] == "session_chat_transport_warning" {
            session_chat_watch_warning(
                &out,
                active_turn.clone(),
                frame["message"]
                    .as_str()
                    .unwrap_or("session_chat display unavailable"),
            );
        } else {
            forward_session_chat(&out, frame, &run, active_turn.as_deref());
        }
        if turn_finished {
            finished_turn = active_turn.clone();
            if let Some((state, _)) = &gate {
                let mut state = state.lock().unwrap();
                if !state.pending && state.turn == active_turn {
                    state.turn = None;
                }
            }
        }
        if terminal {
            return;
        }
    }
}

fn session_chat_turn(frame: &Value) -> Option<String> {
    frame["correlation_turn"]
        .as_str()
        .or_else(|| frame["turn"].as_str())
        .or_else(|| frame["chat_turn"].as_str())
        .map(str::to_owned)
}

fn session_chat_watch_warning(out: &OutState, turn: Option<String>, message: &str) {
    let _ = out.chat_tx.send((
        EventKind::SystemWarning {
            turn: None,
            message: message.into(),
        },
        turn,
        None,
    ));
}

fn forward_session_chat(out: &OutState, frame: Value, run: &str, submitted_turn: Option<&str>) {
    let turn = session_chat_turn(&frame);
    if matches!(frame["type"].as_str(), Some("lagged" | "history_gap")) {
        let _ = out.chat_tx.send((
            EventKind::SystemWarning {
                turn: None,
                message: "session_chat display history discontinuity: output may be missing".into(),
            },
            // Gap frames describe the subscription, not an individual bus event.
            // Bind them before queueing so a later reset cannot adopt the warning.
            turn.or_else(|| submitted_turn.map(str::to_owned)),
            None,
        ));
        return;
    }
    if frame["run_id"].as_str() != Some(run) {
        return;
    }
    let execution = if frame["stage_type"].as_str() == Some("agent") {
        Execution::from_bus_frame(&frame)
    } else {
        None
    };
    if let Ok(event) = serde_json::from_value::<EventKind>(frame) {
        if event.is_chat_event() {
            let _ = out.chat_tx.send((event, turn, execution));
        }
    }
}
