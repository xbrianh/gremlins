//! Session-owned passive chat watch and wire-to-presentation conversion.
use std::sync::{Arc, Weak};

use gremlins_magic_bus::EventKind;
use serde_json::{json, Value};
use tokio::{
    sync::{mpsc, watch, Mutex},
    task::JoinHandle,
};

use super::stream::{self, Opened, WatchStream};
use crate::tui::{
    app::Execution,
    client::{follow_watch, ChatEvent, DaemonClient},
};

pub(crate) const DISCONTINUITY_NOTICE: &str =
    "session_chat display history discontinuity: output may be missing";

#[cfg(test)]
type WatchOpener = Arc<
    dyn Fn(
            String,
            String,
            bool,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Opened, String>> + Send>>
        + Send
        + Sync,
>;

pub(crate) const RETIRED: &str = "session_chat watch belongs to a retired session";

pub(crate) struct ChatWatch {
    client: Weak<DaemonClient>,
    state: Mutex<State>,
    acquisition: Mutex<()>,
    lifecycle: watch::Sender<(u64, bool)>,
    #[cfg(test)]
    opener: std::sync::Mutex<Option<WatchOpener>>,
}

#[derive(Default)]
struct State {
    display: Option<Display>,
    closed: bool,
    replay_required: bool,
}

struct Display {
    generation: u64,
    run: String,
    stream: WatchStream,
    forward: JoinHandle<()>,
}

impl Drop for Display {
    fn drop(&mut self) {
        self.forward.abort();
    }
}

impl Display {
    async fn cancel(mut self) {
        self.forward.abort();
        let _ = (&mut self.forward).await;
        self.stream.cancel().await;
    }
}

// Own the initial reader across cancellation points before WatchStream takes it.
struct Initial(Option<Opened>);

impl Drop for Initial {
    fn drop(&mut self) {
        if let Some((_, reader)) = &self.0 {
            reader.abort();
        }
    }
}

impl ChatWatch {
    pub(crate) fn new(client: &Arc<DaemonClient>) -> Self {
        Self {
            client: Arc::downgrade(client),
            state: Mutex::new(State::default()),
            acquisition: Mutex::new(()),
            lifecycle: watch::channel((0, false)).0,
            #[cfg(test)]
            opener: std::sync::Mutex::new(None),
        }
    }

    #[cfg(test)]
    pub(crate) fn set_opener(&self, opener: WatchOpener) {
        *self.opener.lock().unwrap() = Some(opener);
    }

    pub(crate) async fn retire(&self) {
        // Signal before taking any async lock: an opener may be awaiting its ack.
        self.lifecycle.send_modify(|state| {
            state.0 += 1;
            state.1 = true;
        });
        let display = self.state.lock().await.display.take();
        if let Some(display) = display {
            display.cancel().await;
        }
    }

    pub(crate) async fn reset(&self) {
        let mut state = self.state.lock().await;
        if !state.closed {
            state.replay_required = false;
            self.lifecycle.send_modify(|state| {
                state.0 += 1;
                state.1 = false;
            });
        }
    }

    pub(crate) async fn close(&self) {
        self.lifecycle.send_modify(|state| {
            state.0 += 1;
            state.1 = true;
        });
        let display = {
            let mut state = self.state.lock().await;
            state.closed = true;
            state.display.take()
        };
        if let Some(display) = display {
            display.cancel().await;
        }
    }

    pub(crate) async fn ensure(
        &self,
        client: &DaemonClient,
        generation: u64,
        session: &str,
        run: &str,
    ) -> Result<(), String> {
        let mut lifecycle = self.lifecycle.subscribe();
        if lifecycle.borrow().1 {
            return Err(RETIRED.into());
        }
        tokio::select! {
            biased;
            _ = lifecycle.changed() => Err(RETIRED.into()),
            result = async {
                let _acquisition = self.acquisition.lock().await;
                let replay = self.state.lock().await.replay_required;
                self.prepare(client, generation, session, run, replay, false).await
            } => result,
        }
    }

    pub(crate) async fn confirmed(
        &self,
        client: &DaemonClient,
        generation: u64,
        session: &str,
        response: &Value,
        turn: Option<String>,
    ) -> Result<(), String> {
        let mut lifecycle = self.lifecycle.subscribe();
        if lifecycle.borrow().1 {
            return Err(RETIRED.into());
        }
        tokio::select! {
            biased;
            _ = lifecycle.changed() => Err(RETIRED.into()),
            result = async {
                let _acquisition = self.acquisition.lock().await;
                let run = client.confirm_chat_identity(generation, session, response, turn).await?;
                self.prepare(client, generation, session, &run, true, true).await
            } => result,
        }
    }

    async fn prepare(
        &self,
        client: &DaemonClient,
        generation: u64,
        session: &str,
        run: &str,
        replay: bool,
        confirmation: bool,
    ) -> Result<(), String> {
        let identity = (generation, session.to_owned(), run.to_owned());
        if self.lifecycle.borrow().1 || client.chat_identity().await != identity {
            return Err("session_chat watch belongs to a retired session".into());
        }
        let old = {
            let mut state = self.state.lock().await;
            if state.display.as_ref().is_some_and(|display| {
                display.generation == generation
                    && display.run == run
                    && (confirmation || !display.forward.is_finished())
            }) {
                // An ack for this reservation must not replay a pre-ack terminal.
                // Only a subsequent explicit ensure may reacquire the same run.
                return Ok(());
            }
            state.display.take()
        };
        // Never hold state while stopping readers or waiting on network IO.
        if let Some(display) = old {
            display.cancel().await;
        }
        #[cfg(test)]
        let opener = self.opener.lock().unwrap().clone();
        // A failed initial reservation must not leave a display installed.
        let mut request =
            json!({"op":"watch", "scope":"run", "id":run, "session_id":session, "replay":replay});
        let initial = match open(
            client,
            &request,
            #[cfg(test)]
            &opener,
        )
        .await
        {
            Ok(initial) => initial,
            Err(error) if !replay => {
                self.state.lock().await.replay_required = true;
                return Err(error);
            }
            Err(_) => {
                let (tx, rx) = mpsc::channel(1);
                drop(tx);
                (rx, tokio::spawn(async {}))
            }
        };
        let mut initial = Initial(Some(initial));
        if client.chat_identity().await != identity {
            return Err("session_chat watch belongs to a retired session".into());
        }
        let weak = self.client.clone();
        request["replay"] = true.into();
        let (stream, mut rx) =
            stream::spawn(run.to_owned(), initial.0.take().unwrap(), move || {
                let weak = weak.clone();
                let identity = identity.clone();
                let request = request.clone();
                #[cfg(test)]
                let opener = opener.clone();
                async move {
                    let client = weak.upgrade().ok_or("session client closed")?;
                    if client.chat_identity().await != identity {
                        return Err("session_chat watch retired".into());
                    }
                    open(
                        &client,
                        &request,
                        #[cfg(test)]
                        &opener,
                    )
                    .await
                }
            });
        let tx = client.chat_sender();
        let run = run.to_owned();
        let forward_run = run.clone();
        let forward = tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                forward_frame(&tx, frame, &forward_run);
            }
        });
        let mut state = self.state.lock().await;
        if self.lifecycle.borrow().1 {
            forward.abort();
            drop(stream);
            return Err(RETIRED.into());
        }
        state.replay_required = false;
        state.display = Some(Display {
            generation,
            run,
            stream,
            forward,
        });
        Ok(())
    }
}

async fn open(
    client: &DaemonClient,
    request: &Value,
    #[cfg(test)] opener: &Option<WatchOpener>,
) -> Result<Opened, String> {
    #[cfg(test)]
    if let Some(opener) = opener {
        return opener(
            request["session_id"].as_str().unwrap().to_owned(),
            request["id"].as_str().unwrap().to_owned(),
            request["replay"].as_bool().unwrap(),
        )
        .await;
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let socket = client.open_aux_stream().await?;
        follow_watch(
            socket,
            request["session_id"].as_str().unwrap(),
            request["id"].as_str().unwrap(),
            None,
            None,
            request["replay"].as_bool().unwrap(),
        )
        .await
        .map_err(|error| match error.as_str() {
            "executor closed connection before watch acknowledgment" => {
                "connection closed before watch acknowledgment".to_owned()
            }
            _ => error,
        })
    })
    .await
    .map_err(|_| "session_chat watch acknowledgment timed out".to_owned())?
}

fn forward_frame(tx: &mpsc::UnboundedSender<ChatEvent>, frame: Value, run: &str) {
    let turn = ["correlation_turn", "turn", "chat_turn"]
        .into_iter()
        .find_map(|key| frame[key].as_str().map(str::to_owned));
    let gap = matches!(frame["type"].as_str(), Some("lagged" | "history_gap"));
    // Ordinary wire warnings remain turn-scoped; only synthesized gaps are notices.
    if !gap && frame["type"] == "system_warning" && turn.is_none() {
        return;
    }
    if !gap && frame["run_id"].as_str() != Some(run) {
        return;
    }
    let field = |key: &str| frame[key].as_str().filter(|s| !s.trim().is_empty());
    let execution = field("run_id")
        .zip(field("stage_run_id"))
        .filter(|_| !gap && frame["stage_type"] == "agent")
        .map(|(run_id, stage_run_id)| Execution {
            run_id: run_id.to_owned(),
            stage_run_id: stage_run_id.to_owned(),
            // Historical bus frames carry no execution nonce; preserve compatibility.
            nonce: 0,
        });
    let event = if gap {
        Some(EventKind::SystemWarning {
            turn: None,
            message: DISCONTINUITY_NOTICE.into(),
        })
    } else {
        serde_json::from_value::<EventKind>(frame).ok()
    };
    if let Some(event) = event.filter(|event| gap || event.is_chat_event()) {
        let _ = tx.send((event, if gap { None } else { turn }, execution));
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::Notify;

    async fn interrupts_pending_open(close: bool) {
        let (client, _requests, _events, _chat, _raw) = DaemonClient::fake_with_requests(
            json!({"type":"ok", "session_id":"session", "chat_run_id":"run"}),
        );
        let client = Arc::new(client);
        let watch = Arc::new(ChatWatch::new(&client));
        let entered = Arc::new(Notify::new());
        let signal = entered.clone();
        watch.set_opener(Arc::new(move |_, _, _| {
            let signal = signal.clone();
            Box::pin(async move {
                signal.notify_one();
                std::future::pending().await
            })
        }));
        let task_watch = watch.clone();
        let task_client = client.clone();
        let pending =
            tokio::spawn(async move { task_watch.ensure(&task_client, 0, "session", "run").await });
        entered.notified().await;
        assert!(
            watch.state.try_lock().is_ok(),
            "network IO holds state lock"
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            if close {
                watch.close().await;
            } else {
                watch.retire().await;
            }
            assert_eq!(pending.await.unwrap(), Err(RETIRED.to_owned()));
        })
        .await
        .expect("lifecycle must interrupt pending acknowledgment");
        assert!(watch.state.lock().await.display.is_none());
    }

    #[tokio::test]
    async fn same_run_confirmation_does_not_replay_terminal_reservation() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (client, _requests, _events, _chat, _raw) = DaemonClient::fake_with_requests(
            json!({"type":"ok", "session_id":"session", "chat_run_id":"run"}),
        );
        let client = Arc::new(client);
        let watch = ChatWatch::new(&client);
        let opens = Arc::new(AtomicUsize::new(0));
        let count = opens.clone();
        watch.set_opener(Arc::new(move |_, run, _| {
            count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                let (tx, rx) = mpsc::channel(1);
                tx.send(json!({"type":"run_completed", "run_id":run}))
                    .await
                    .unwrap();
                Ok((rx, tokio::spawn(async {})))
            })
        }));
        watch.ensure(&client, 0, "session", "run").await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if watch
                    .state
                    .lock()
                    .await
                    .display
                    .as_ref()
                    .unwrap()
                    .forward
                    .is_finished()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        watch
            .confirmed(
                &client,
                0,
                "session",
                &json!({
                    "type":"ok", "enqueued":true, "session_id":"session", "chat_run_id":"run"
                }),
                None,
            )
            .await
            .unwrap();
        assert_eq!(opens.load(Ordering::SeqCst), 1);
        watch.ensure(&client, 0, "session", "run").await.unwrap();
        assert_eq!(opens.load(Ordering::SeqCst), 2);
        watch.close().await;
    }

    #[tokio::test]
    async fn close_interrupts_pending_ack_without_state_lock() {
        interrupts_pending_open(true).await;
    }

    #[tokio::test]
    async fn retire_interrupts_pending_ack_without_state_lock() {
        interrupts_pending_open(false).await;
    }
}
