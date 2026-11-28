//! Replayable watch transport. The opener owns connection setup and replay requests.
use std::{future::Future, pin::Pin, time::Duration};

use serde_json::{json, Value};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

pub(crate) type Opened = (mpsc::Receiver<Value>, JoinHandle<()>);
const CAPACITY: usize = 256;
const RETRY: Duration = Duration::from_millis(100);

pub(crate) struct WatchStream {
    task: Option<JoinHandle<()>>,
    cancel: Option<oneshot::Sender<()>>,
}

impl WatchStream {
    /// Stop the watch and abort and join its owned socket reader.
    pub async fn cancel(&mut self) {
        if let Some(task) = self.task.as_mut() {
            if let Some(cancel) = self.cancel.take() {
                let _ = cancel.send(());
            }
            let _ = task.await;
            self.task.take();
        }
    }
}

impl Drop for WatchStream {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

// Dropping a JoinHandle alone detaches the socket reader, including when the
// outer watch task is aborted while waiting for another frame.
struct Connection(Opened);

impl Drop for Connection {
    fn drop(&mut self) {
        self.0 .1.abort();
    }
}

type Opening = Pin<Box<dyn Future<Output = Result<Opened, String>> + Send>>;
enum State {
    Connected(Connection),
    Opening(Opening),
    Done,
}

enum Event {
    Frame(Option<Value>),
    Opened(Result<Opened, String>),
}

impl State {
    async fn close(&mut self) {
        if let Self::Connected(mut connection) = std::mem::replace(self, Self::Done) {
            connection.0 .1.abort();
            let _ = (&mut connection.0 .1).await;
        }
    }

    async fn next(&mut self) -> Event {
        match self {
            Self::Connected(connection) => Event::Frame(connection.0 .0.recv().await),
            Self::Opening(opening) => Event::Opened(opening.await),
            Self::Done => std::future::pending().await,
        }
    }
}

/// Start with an already successful subscription. Every subsequent opener call
/// must return a fresh subscription including its replay. Only successfully
/// enqueued sequence numbers advance the replay cursor; unsequenced frames are
/// never content-deduplicated. No execution/input requests are made here.
pub(crate) fn spawn<F, Fut>(
    run: String,
    initial: Opened,
    mut opener: F,
) -> (WatchStream, mpsc::Receiver<Value>)
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = Result<Opened, String>> + Send + 'static,
{
    let (tx, rx) = mpsc::channel(CAPACITY);
    let (cancel_tx, mut cancel_rx) = oneshot::channel();
    // Install ownership before spawning so even immediate cancellation aborts
    // the initial reader.
    let initial = Connection(initial);
    let task = tokio::spawn(async move {
        let mut state = State::Connected(initial);
        let mut cursor = None;
        let mut gap = None;
        let mut pending_terminal = None;
        let mut failures = 0u32;
        let mut warned = false;
        let mut recovering = false;
        loop {
            if matches!(state, State::Done) && gap.is_none() && pending_terminal.is_none() {
                break;
            }
            tokio::select! {
                biased;
                _ = &mut cancel_rx => break,
                _ = tx.closed() => break,
                permit = tx.reserve(), if gap.is_some() || pending_terminal.is_some() => {
                    let Ok(permit) = permit else { break };
                    permit.send(gap.take().or_else(|| pending_terminal.take()).unwrap());
                }
                event = state.next() => match event {
                    Event::Frame(Some(frame)) => {
                        let terminal = is_terminal(&run, &frame);
                        let sequence = frame["sequence"].as_u64();
                        if sequence.zip(cursor).is_some_and(|(seq, last)| seq <= last) {
                            continue;
                        }
                        {
                            // Replay checkpoints and duplicate history do not prove recovery.
                            if frame["type"] != "replay_checkpoint" {
                                failures = 0;
                                warned = false;
                                recovering = false;
                            }
                            // Never let new output overtake a pending gap. Keep only
                            // the gap and (if encountered) one terminal outside the queue.
                            let sent = if gap.is_some() {
                                Err(mpsc::error::TrySendError::Full(frame))
                            } else {
                                tx.try_send(frame)
                            };
                            match sent {
                                Ok(()) => {
                                    if let Some(sequence) = sequence {
                                        cursor = Some(sequence);
                                    }
                                }
                                Err(mpsc::error::TrySendError::Full(frame)) => {
                                    if terminal {
                                        pending_terminal = Some(frame);
                                    }
                                    gap.get_or_insert_with(|| json!({
                                        "type":"history_gap", "id":run,
                                        "message":"watch output overflow: frames were dropped"
                                    }));
                                }
                                Err(mpsc::error::TrySendError::Closed(_)) => break,
                            }
                        }
                        if terminal {
                            state.close().await;
                        }
                    }
                    Event::Opened(Ok(opened)) => {
                        state = State::Connected(Connection(opened));
                    }
                    event => {
                        let error = match event {
                            Event::Opened(Err(error)) => Some(error),
                            _ if recovering => Some("reader closed before recovery".into()),
                            _ => None,
                        };
                        if let Some(error) = error {
                            failures = failures.saturating_add(1);
                            if !warned {
                                warned = true;
                                gap.get_or_insert_with(|| json!({
                                    "type":"history_gap", "id":run,
                                    "message":format!("watch transport disconnected; retrying: {error}")
                                }));
                            }
                        }
                        recovering = true;
                        state.close().await;
                        state = reopening(failures, opener());
                    }
                }
            }
        }
        state.close().await;
    });
    (
        WatchStream {
            task: Some(task),
            cancel: Some(cancel_tx),
        },
        rx,
    )
}

fn reopening<Fut>(failures: u32, opening: Fut) -> State
where
    Fut: Future<Output = Result<Opened, String>> + Send + 'static,
{
    let delay = (RETRY * (1u32 << failures.min(6))).min(Duration::from_secs(5));
    State::Opening(Box::pin(async move {
        tokio::time::sleep(delay).await;
        opening.await
    }))
}

fn is_terminal(run: &str, frame: &Value) -> bool {
    let terminal = match frame["type"].as_str() {
        Some("run_completed" | "run_failed" | "run_stopped") => true,
        Some("ended") => ["correlation_turn", "turn", "chat_turn"]
            .iter()
            .all(|key| frame[*key].is_null()),
        _ => false,
    };
    terminal && frame["id"].as_str().or_else(|| frame["run_id"].as_str()) == Some(run)
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
