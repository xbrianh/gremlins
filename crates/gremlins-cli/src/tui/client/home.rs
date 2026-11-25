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

pub(super) struct Display {
    reader: Reader,
    forward: tokio::task::JoinHandle<()>,
}

impl Drop for Display {
    fn drop(&mut self) {
        self.reader.abort();
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
        let turn = request["turn"].as_str().map(str::to_owned);
        let id = self.home_run_id().await;
        // Existing runs can subscribe before enqueue. The initial reservation
        // is not live yet; its first turn is recovered from atomic bus replay.
        let prepared = self.prepare_home(&id, true).await;
        // Retire the previous turn's forwarder before new output can arrive.
        self.home_display.lock().await.take();
        let response = match self.session.send_on_session(request).await {
            Ok(response) => response,
            Err(error) => {
                if let Ok((_, reader)) = prepared {
                    reader.abort();
                }
                return Err(error);
            }
        };
        if response["type"] == "error" {
            if let Ok((_, reader)) = prepared {
                reader.abort();
            }
            return Ok(response);
        }
        let actual = response["id"].as_str().unwrap_or(&id).to_owned();
        let stream = match prepared {
            Ok((rx, reader)) if actual == id => Ok((rx, reader)),
            Ok((_, reader)) => {
                reader.abort();
                self.prepare_home(&actual, false).await
            }
            Err(_) => self.prepare_home(&actual, false).await,
        };
        let (mut rx, reader) = stream.map_err(|e| format!("home display unavailable: {e}"))?;
        let out = Arc::clone(&self.out);
        let forward = tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                let terminal = frame["run_id"] == actual
                    && matches!(frame["type"].as_str(), Some("done" | "ended"));
                forward_home(&out, frame, &actual, turn.as_deref());
                if terminal {
                    return;
                }
            }
            let _ = out.chat_tx.send((
                EventKind::Error {
                    turn: None,
                    message: "home display connection closed before turn completion".into(),
                },
                turn,
                None,
            ));
        });
        *self.home_display.lock().await = Some(Display { reader, forward });
        Ok(response)
    }

    async fn prepare_home(
        &self,
        id: &str,
        discard_history: bool,
    ) -> Result<(mpsc::Receiver<Value>, Reader), String> {
        let stream = self.open_aux_stream().await?;
        let session = self.session_id().await;
        let (mut rx, reader) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            follow_run(stream, &session, id),
        )
        .await
        .map_err(|_| "home watch acknowledgment timed out".to_owned())??;
        let reader = Reader(reader);
        if discard_history {
            let cut = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while let Some(frame) = rx.recv().await {
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
        Ok((rx, reader))
    }
}

fn forward_home(out: &OutState, frame: Value, run: &str, turn: Option<&str>) {
    if matches!(frame["type"].as_str(), Some("lagged" | "history_gap")) {
        let _ = out.chat_tx.send((
            EventKind::SystemWarning {
                turn: None,
                message: "home display history discontinuity: output may be missing".into(),
            },
            turn.map(str::to_owned),
            None,
        ));
        return;
    }
    if frame["run_id"].as_str() != Some(run) {
        return;
    }
    if let Ok(event) = serde_json::from_value::<EventKind>(frame) {
        if event.is_chat_event() {
            // Watch is observation only: it supplies no nonce/controller grant.
            let _ = out.chat_tx.send((event, turn.map(str::to_owned), None));
        }
    }
}
