//! Passive home display transport, separate from the enqueue-only request socket.
use super::*;

#[cfg(test)]
#[path = "home_tests.rs"]
mod tests;

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

struct HomeWatchContext {
    session_write: Arc<Mutex<WriteState>>,
    out: Arc<OutState>,
    generation: u64,
    run: String,
    turn: Option<String>,
}

pub(super) struct Display {
    forward: tokio::task::JoinHandle<()>,
}

impl Drop for Display {
    fn drop(&mut self) {
        self.forward.abort();
    }
}

impl DaemonClient {
    pub(super) async fn send_home_or_session(
        &self,
        request: Value,
        generation: Option<u64>,
    ) -> Result<Value, String> {
        if let Some(expected) = generation {
            if self.session.current_generation().await != expected {
                return Err("session was recreated".into());
            }
        }
        let home = request["op"] == "chat" && request.get("target").is_none();
        if !home {
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
                write.home_run_id.clone().unwrap_or_default(),
            )
        };
        let turn = request["turn"].as_str().map(str::to_owned);
        // Existing runs can subscribe before enqueue. The initial reservation
        // is not live yet; its first turn is recovered from atomic bus replay.
        let prepared = self.prepare_home(&id, true).await;
        // Retire the previous turn's forwarder before new output can arrive.
        {
            let mut display = self.home_display.lock().await;
            let write = self.session.write.lock().await;
            if write.generation != expected_generation {
                return Err("home display belongs to a retired session".into());
            }
            display.take();
        }
        let response = match self
            .session
            .send_on_session_for_generation(request, expected_generation)
            .await
        {
            Ok(response) => response,
            Err(error) => {
                if let Ok((_, reader, _)) = prepared {
                    reader.abort();
                }
                return Err(error);
            }
        };
        if response["type"] == "error" {
            if let Ok((_, reader, _)) = prepared {
                reader.abort();
            }
            return Ok(response);
        }
        let mut write = self.session.write.lock().await;
        if write.generation != expected_generation
            || write.session_id.as_deref() != Some(&session_id)
            || response["session_id"]
                .as_str()
                .is_some_and(|id| id != session_id)
        {
            return Err("home acknowledgment belongs to a retired session".into());
        }
        if response["type"] != "ok" {
            return Err("unexpected home enqueue acknowledgment".into());
        }
        let actual = response["id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "home enqueue acknowledgment has no run id".to_owned())?
            .to_owned();
        write.home_run_id = Some(actual.clone());
        let _ = self
            .out
            .raw_tx
            .send(json!({"type":"home_run_identity", "id":actual,
            "turn":turn, "session_id":session_id,
            "generation":expected_generation}));
        drop(write);
        let stream = match prepared {
            Ok(stream) if actual == id => Ok(stream),
            Ok((_, reader, _)) => {
                reader.abort();
                self.prepare_home(&actual, false).await
            }
            Err(_) => self.prepare_home(&actual, false).await,
        };
        // A failed watch is independent of the accepted submission. Retry only
        // the passive replay subscription, never the chat request.
        let stream = match stream {
            Ok(stream) => Ok(stream),
            Err(_) => self.prepare_home(&actual, false).await,
        };
        let mut initial_warning = None;
        let (rx, reader, gaps) = match stream {
            Ok(stream) => stream,
            Err(error) => {
                initial_warning = Some(format!("home display unavailable: {error}"));
                let (tx, rx) = mpsc::channel(1);
                drop(tx);
                (rx, Reader(tokio::spawn(async {})), Vec::new())
            }
        };
        let mut display = self.home_display.lock().await;
        let write = self.session.write.lock().await;
        if write.generation != expected_generation {
            return Err("home display belongs to a retired session".into());
        }
        let session_write = Arc::clone(&self.session.write);
        let out = Arc::clone(&self.out);
        #[cfg(test)]
        let recovery_streams = Arc::clone(&self.aux_streams);
        let forward = tokio::spawn(forward_home_stream(
            rx,
            reader,
            gaps,
            HomeWatchContext {
                session_write,
                out,
                generation: expected_generation,
                run: actual.clone(),
                turn,
            },
            initial_warning,
            move || {
                let session_id = session_id.clone();
                let actual = actual.clone();
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
                        follow_run(stream, &session_id, &actual),
                    )
                    .await
                    .map_err(|_| "home watch acknowledgment timed out".to_owned())??;
                    Ok((rx, Reader(reader)))
                }
            },
        ));
        *display = Some(Display { forward });
        Ok(response)
    }

    async fn prepare_home(
        &self,
        id: &str,
        discard_history: bool,
    ) -> Result<(mpsc::Receiver<Value>, Reader, Vec<Value>), String> {
        let stream = self.open_aux_stream().await?;
        let session = self.session_id().await;
        let (mut rx, reader) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            follow_run(stream, &session, id),
        )
        .await
        .map_err(|_| "home watch acknowledgment timed out".to_owned())??;
        let reader = Reader(reader);
        let mut gaps = Vec::new();
        if discard_history {
            let cut = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while let Some(frame) = rx.recv().await {
                    if matches!(frame["type"].as_str(), Some("history_gap" | "lagged")) {
                        gaps.push(frame.clone());
                    }
                    if frame["type"] == "replay_checkpoint" {
                        return Ok(());
                    }
                }
                Err("home watch closed before replay checkpoint".to_owned())
            })
            .await;
            if !matches!(cut, Ok(Ok(()))) {
                reader.abort();
                return Err("home watch did not complete replay".into());
            }
        }
        Ok((rx, reader, gaps))
    }
}

async fn forward_home_stream<F, Fut>(
    mut rx: mpsc::Receiver<Value>,
    mut reader: Reader,
    gaps: Vec<Value>,
    context: HomeWatchContext,
    initial_warning: Option<String>,
    mut recover: F,
) where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(mpsc::Receiver<Value>, Reader), String>>,
{
    let HomeWatchContext {
        session_write,
        out,
        generation,
        run,
        turn,
    } = context;
    let mut gaps = gaps.into_iter();
    let mut warned = false;
    let mut backoff = std::time::Duration::from_millis(50);
    let mut sequences = std::collections::HashSet::new();
    loop {
        let frame = if let Some(gap) = gaps.next() {
            gap
        } else if let Some(frame) = rx.recv().await {
            frame
        } else {
            {
                let write = session_write.lock().await;
                if write.generation != generation {
                    return;
                }
            }
            reader.abort();
            if let Ok((next_rx, next_reader)) = recover().await {
                rx = next_rx;
                reader = next_reader;
                continue;
            }
            if !warned {
                let write = session_write.lock().await;
                if write.generation != generation {
                    return;
                }
                home_watch_warning(
                    &out,
                    turn.clone(),
                    initial_warning
                        .as_deref()
                        .unwrap_or("home display connection closed before turn completion"),
                );
                warned = true;
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(std::time::Duration::from_secs(5));
            continue;
        };
        let frame_turn = home_turn(&frame);
        if frame_turn.is_some() && frame_turn != turn {
            continue;
        }
        let write = session_write.lock().await;
        if write.generation != generation {
            return;
        }
        let terminal = frame["run_id"] == run
            && matches!(frame["type"].as_str(), Some("done" | "ended"))
            && frame_turn == turn;
        if frame["run_id"] == run {
            if let Some(seq) = frame["seq"].as_u64() {
                if !sequences.insert(seq) {
                    continue;
                }
            }
        }
        forward_home(&out, frame, &run, turn.as_deref());
        if terminal {
            return;
        }
    }
}

fn home_turn(frame: &Value) -> Option<String> {
    frame["correlation_turn"]
        .as_str()
        .or_else(|| frame["turn"].as_str())
        .or_else(|| frame["chat_turn"].as_str())
        .map(str::to_owned)
}

fn home_watch_warning(out: &OutState, turn: Option<String>, message: &str) {
    let _ = out.chat_tx.send((
        EventKind::SystemWarning {
            turn: None,
            message: message.into(),
        },
        turn,
        None,
    ));
}

fn forward_home(out: &OutState, frame: Value, run: &str, submitted_turn: Option<&str>) {
    let turn = home_turn(&frame);
    if matches!(frame["type"].as_str(), Some("lagged" | "history_gap")) {
        let _ = out.chat_tx.send((
            EventKind::SystemWarning {
                turn: None,
                message: "home display history discontinuity: output may be missing".into(),
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
