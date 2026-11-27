//! Native snapshot/live replay, with persisted history for incomplete snapshots.

use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::tui::client::DaemonClient;
use crate::tui::AbortOnDrop;

fn find_in_tree(stages: &Value, name: &str) -> Option<String> {
    let children = stages.as_array()?;
    for stage in children {
        if stage.get("name").and_then(Value::as_str) == Some(name) {
            return stage.get("type").and_then(Value::as_str).map(String::from);
        }
    }
    children
        .iter()
        .find_map(|stage| find_in_tree(stage.get("body")?, name))
}

fn safe_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .replace("..", "__")
}

fn format_tool_use_str(block: &Value) -> String {
    let name = block.get("name").and_then(Value::as_str).unwrap_or("");
    match block.get("input") {
        Some(Value::Object(map)) if !map.is_empty() => {
            let parts: Vec<_> = map
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect();
            format!("{name}({})", parts.join(", "))
        }
        Some(Value::String(s)) => format!("{name}({s})"),
        _ => name.to_owned(),
    }
}

fn log_line(id: &str, line: &str) -> Value {
    json!({"type": "log_line", "id": id, "line": line})
}

fn decode_agent_stream_line(line: &str, run: &str) -> Vec<Value> {
    let Ok(parsed) = serde_json::from_str::<Value>(line) else {
        return vec![log_line(run, line)];
    };
    let kind = parsed.get("type").and_then(Value::as_str).unwrap_or("");
    if !matches!(kind, "assistant" | "user") {
        return if matches!(
            kind,
            "stream_chunk"
                | "reasoning_chunk"
                | "tool_output_chunk"
                | "tool_result"
                | "turn_complete"
                | "done"
                | "stream_reset"
                | "task_progress"
                | "operator_message"
        ) {
            vec![parsed]
        } else {
            vec![log_line(run, line)]
        };
    }
    let Some(content) = parsed.pointer("/message/content").and_then(Value::as_array) else {
        return vec![log_line(run, line)];
    };
    content.iter().filter_map(|block| {
        let text = block.get("text").and_then(Value::as_str).unwrap_or("");
        match block.get("type").and_then(Value::as_str) {
            Some("text") if kind == "assistant" => Some(json!({"type": "stream_chunk", "text": text})),
            Some("thinking") if kind == "assistant" => Some(json!({"type": "reasoning_chunk", "text": block.get("thinking").and_then(Value::as_str).unwrap_or("")})),
            Some("tool_use") if kind == "assistant" => Some(json!({
                "type": "tool_output_chunk", "id": block.get("id").and_then(Value::as_str).unwrap_or(""),
                "name": block.get("name").and_then(Value::as_str).unwrap_or(""),
                "call": format_tool_use_str(block), "text": ""
            })),
            Some("tool_result") if kind == "user" => Some(json!({
                "type": "tool_result", "id": block.get("tool_use_id").and_then(Value::as_str).unwrap_or(""),
                "name": "", "call": "", "output": block.get("content").and_then(Value::as_str).unwrap_or("")
            })),
            _ => None,
        }
    }).collect()
}

pub(super) fn decode_history(lines: Vec<String>, blob: &str, id: &str) -> Vec<Value> {
    let mut header = false;
    lines.into_iter().flat_map(|line| {
        if blob.starts_with("artifacts/stream-") {
            decode_agent_stream_line(&line, id)
        } else if blob.starts_with("command_logs/exec-") {
            // proc writes a structured preamble and footer around raw output.
            // Keep these visible as log lines.
            if line.starts_with("=== ") && line.ends_with(" ===") { header = true; }
            let metadata = header || (line.starts_with("--- exit: ") && line.ends_with(" ---"));
            if header && !line.starts_with("=== ") && !line.starts_with("cwd: ")
                && !line.starts_with("timeout: ") && !line.starts_with("cmds: ")
                && !matches!(line.as_str(), "env:" | "substitution_env:" | "commands:")
                && !line.starts_with("  ") { header = false; }
            if metadata && header || line.starts_with("--- exit: ") && line.ends_with(" ---") {
                vec![json!({"type":"log_line", "id":id, "line":line, "replay_metadata":true})]
            } else {
                vec![json!({"type": "exec_output", "id": id, "is_stderr": false, "text": line, "ts": ""})]
            }
        } else {
            vec![log_line(id, line.split_once("] ").map(|(_, msg)| msg).unwrap_or(&line))]
        }
    }).collect()
}

const MAX_SNAPSHOT_RECORDS: usize = 65_536;

pub(super) struct Snapshot {
    pub(super) values: Vec<Value>,
    pub(super) fallback: Option<&'static str>,
}

fn fallback_reason(value: &Value) -> Option<&'static str> {
    if value["type"] == "history_gap" {
        Some("history_gap: native replay history is incomplete")
    } else if value["type"] == "replay_checkpoint" && value["complete"] == false {
        Some("native replay checkpoint is incomplete")
    } else {
        None
    }
}

/// Collect the finite native replay, including retained history after a gap.
pub(super) async fn collect_checkpoint(rx: &mut mpsc::Receiver<Value>) -> Snapshot {
    let mut values = Vec::new();
    let mut fallback = None;
    while let Some(value) = rx.recv().await {
        fallback = fallback.or_else(|| fallback_reason(&value));
        let checkpoint = value["type"] == "replay_checkpoint";
        values.push(value);
        if checkpoint {
            return Snapshot { values, fallback };
        }
        if values.len() == MAX_SNAPSHOT_RECORDS {
            return Snapshot {
                values,
                fallback: Some("native replay snapshot limit reached"),
            };
        }
    }
    Snapshot {
        values,
        fallback: Some("run stream closed before replay_checkpoint"),
    }
}

fn send_line(tx: &mpsc::UnboundedSender<Value>, id: &str, generation: u64, value: Value) -> bool {
    let value = if value["type"] == "history_gap" {
        log_line(
            id,
            &format!("history_gap: native replay discontinuity ({value})"),
        )
    } else {
        value
    };
    tx.send(
        json!({"type": "view_log", "id": id, "generation": generation, "line": value.to_string()}),
    )
    .is_ok()
}

fn send_native(tx: &mpsc::UnboundedSender<Value>, id: &str, generation: u64, value: Value) -> bool {
    let line = if value["type"] == "history_gap" {
        log_line(
            id,
            &format!("history_gap: native replay discontinuity ({value})"),
        )
    } else {
        value.clone()
    };
    tx.send(json!({"type":"view_log", "id":id, "generation":generation,
        "line":line.to_string(), "native":value}))
        .is_ok()
}

/// Disk tails have no execution-scoped coverage contract. Preserve every native
/// frame, even when its content happens to equal persisted history.
fn send_disk_history(
    tx: &mpsc::UnboundedSender<Value>,
    id: &str,
    generation: u64,
    history: Vec<Value>,
    stage: &str,
) -> bool {
    let scope = if stage.is_empty() {
        ""
    } else {
        "; stage history is not execution-scoped and may include other executions"
    };
    send_line(tx, id, generation, log_line(id, &format!(
        "history discontinuity: bounded persisted tail (up to 500 lines); persisted tail and original stream may overlap; possible overlap is preserved, not deduplicated{scope}"
    ))) && history.into_iter().all(|value| send_line(tx, id, generation, value))
}

#[cfg(test)]
pub(crate) async fn replay_and_dedup(
    client: Arc<DaemonClient>,
    follow_id: String,
    _current_stage: String,
    replay_stage: String,
    generation: u64,
    log_tx: mpsc::UnboundedSender<Value>,
) {
    replay_selection(client, follow_id, None, replay_stage, generation, log_tx).await;
}

pub(crate) async fn replay_selection(
    client: Arc<DaemonClient>,
    follow_id: String,
    execution: Option<super::app::LiveExecution>,
    replay_stage: String,
    generation: u64,
    log_tx: mpsc::UnboundedSender<Value>,
) {
    let send_error = |msg: &str| {
        let _ = log_tx.send(json!({"type": "view_error", "id": follow_id, "generation": generation, "message": msg}));
    };
    let diagnostic = |msg: &str| {
        send_line(&log_tx, &follow_id, generation, log_line(&follow_id, msg));
    };
    let subscription = match &execution {
        Some(execution) => {
            client
                .follow_watch(
                    &follow_id,
                    Some(&execution.stage),
                    Some(&execution.stage_run_id),
                )
                .await
        }
        None => client.follow_run(&follow_id).await,
    };
    match subscription {
        Ok((mut rx, handle)) => {
            let _guard = AbortOnDrop(handle);
            if !send_native(
                &log_tx,
                &follow_id,
                generation,
                json!({"type":"watched", "id":follow_id, "replay":true,
                    "stage":execution.as_ref().map(|e| &e.stage),
                    "stage_run_id":execution.as_ref().map(|e| &e.stage_run_id)}),
            ) {
                return;
            }
            let snapshot = collect_checkpoint(&mut rx).await;
            if let Some(reason) = snapshot.fallback {
                diagnostic(reason);
                if execution.is_none() {
                    match read_disk(&client, &follow_id, &replay_stage).await {
                        Ok(history) => {
                            if !send_disk_history(
                                &log_tx,
                                &follow_id,
                                generation,
                                history,
                                &replay_stage,
                            ) {
                                return;
                            }
                        }
                        Err(error) => diagnostic(&error),
                    }
                } else {
                    diagnostic("history discontinuity: exact invocation history unavailable; persisted stage history is not execution-scoped");
                }
            }
            for value in snapshot.values {
                if !send_native(&log_tx, &follow_id, generation, value) {
                    return;
                }
            }
            while let Some(value) = rx.recv().await {
                if !send_native(&log_tx, &follow_id, generation, value) {
                    return;
                }
            }
            send_error("run stream closed");
            return;
        }
        Err(error) if execution.is_some() => {
            let _ = log_tx.send(json!({"type":"view_error", "id":follow_id,
                "generation":generation, "force":true,
                "message":format!("exact invocation follow unavailable: {error}; no execution-scoped persisted history")}));
            return;
        }
        Err(error) => diagnostic(&format!(
            "native run follow unavailable: {error}; bounded persisted tail only (no live follow)"
        )),
    }

    match read_disk(&client, &follow_id, &replay_stage).await {
        Ok(history) => {
            if !send_disk_history(&log_tx, &follow_id, generation, history, &replay_stage) {
                return;
            }
        }
        Err(error) => diagnostic(&error),
    }
    let _ = log_tx.send(json!({"type":"view_error", "id":follow_id,
        "generation":generation, "force":true,
        "message":"native run follow unavailable; showing bounded persisted tail only, no live updates"}));
}

async fn read_disk(
    client: &DaemonClient,
    follow_id: &str,
    replay_stage: &str,
) -> Result<Vec<Value>, String> {
    let blob = if replay_stage.is_empty() {
        "log".to_owned()
    } else {
        let definition = client.read_definition(follow_id).await.ok();
        match definition
            .as_ref()
            .and_then(|d| find_in_tree(d.get("stages")?, replay_stage))
            .as_deref()
        {
            Some("agent") => format!("artifacts/stream-{}.jsonl", safe_name(replay_stage)),
            Some("exec") => format!("command_logs/exec-{}.log", safe_name(replay_stage)),
            _ => "log".to_owned(),
        }
    };
    let (lines, _, _) = client
        .read_blob_checkpoint(follow_id, &blob, 500)
        .await
        .map_err(|error| format!("history unavailable ({blob}): {error}"))?;
    Ok(decode_history(lines, &blob, follow_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn inline_exact_watch_waits_for_ack_filters_siblings_and_never_reads_disk() {
        use crate::tui::{
            app::{App, LiveExecution},
            apply_watch_frame, close_watch_follow,
            screen::LiveArea,
            start_watch_follow,
        };
        use gremlins::executor::socket;
        use tokio::{
            io::BufReader,
            net::UnixStream,
            time::{timeout, Duration},
        };
        let (native, peer) = UnixStream::pair().unwrap();
        let (client, [_control, session]) = DaemonClient::fake_with_aux_streams(
            json!({"session_id":"owner", "chat_run_id":"session_chat"}),
            vec![native],
        );
        let client = Arc::new(client);
        let mut app = App::new();
        let execution = LiveExecution {
            run_id: "r".into(),
            stage: "review".into(),
            stage_type: "exec".into(),
            stage_run_id: "review#1".into(),
        };
        let key = execution.key();
        app.live_executions.insert(key.clone(), execution);
        let mut live = LiveArea::with_backend(ratatui::backend::TestBackend::new(80, 24), 80, 24);
        live.show_runs();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut follow = None;
        let mut generation = 0;
        start_watch_follow(
            &mut app,
            &mut live,
            &client,
            key,
            &tx,
            &mut follow,
            &mut generation,
        );
        let mut peer = BufReader::new(peer);
        let request = socket::read_json_line(&mut peer).await.unwrap().unwrap();
        assert_eq!(request["stage"], "review");
        assert_eq!(request["stage_run_id"], "review#1");
        assert!(live.runs_view_mut().unwrap().watch_pending);
        assert!(timeout(Duration::from_millis(30), rx.recv()).await.is_err());
        assert!(live.runs_view_mut().unwrap().watch_pending);
        socket::write_json_line(peer.get_mut(), &json!({"type":"watched", "scope":"run", "id":"r", "replay":true, "stage":"review", "stage_run_id":"review#1"})).await.unwrap();
        let ack = rx.recv().await.unwrap();
        apply_watch_frame(&mut app, &mut live, &ack, generation, &mut follow);
        assert!(!live.runs_view_mut().unwrap().watch_pending);
        for (stage, invocation) in [("build", "build#1"), ("review", "review#2")] {
            let frame = json!({"type":"view_log", "id":"r", "generation":generation,
                "native":{"type":"stream_chunk", "run_id":"r", "stage":stage, "stage_run_id":invocation, "text":"wrong"}, "line":"wrong"});
            apply_watch_frame(&mut app, &mut live, &frame, generation, &mut follow);
        }
        assert!(!live.runs_view_mut().unwrap().stream_contains("wrong"));
        let frame = json!({"type":"view_log", "id":"r", "generation":generation,
            "native":{"type":"stream_chunk", "run_id":"r", "stage":"review", "stage_run_id":"review#1", "text":"right"},
            "line":json!({"type":"stream_chunk", "text":"right"}).to_string()});
        apply_watch_frame(&mut app, &mut live, &frame, generation, &mut follow);
        assert!(live.runs_view_mut().unwrap().stream_contains("right"));
        socket::write_json_line(
            peer.get_mut(),
            &json!({"type":"replay_checkpoint", "id":"r", "complete":false}),
        )
        .await
        .unwrap();
        let mut checkpoint_seen = false;
        while !checkpoint_seen {
            let frame = timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
            checkpoint_seen = frame["native"]["type"] == "replay_checkpoint";
            assert!(!frame["line"].as_str().unwrap_or("").contains("wrong"));
            apply_watch_frame(&mut app, &mut live, &frame, generation, &mut follow);
        }
        let mut session = BufReader::new(session);
        assert!(timeout(
            Duration::from_millis(30),
            socket::read_json_line(&mut session)
        )
        .await
        .is_err());
        close_watch_follow(&mut app, &mut live, &mut follow);
        assert!(!live.runs_view_mut().unwrap().watch_pending);
        client.shutdown().await;
    }

    #[tokio::test]
    async fn exact_rejection_never_reads_disk() {
        use gremlins::executor::socket;
        use tokio::{
            io::BufReader,
            net::UnixStream,
            time::{timeout, Duration},
        };
        let (native, peer) = UnixStream::pair().unwrap();
        let (client, [_control, session]) = DaemonClient::fake_with_aux_streams(
            json!({"session_id":"owner", "chat_run_id":"session_chat"}),
            vec![native],
        );
        let client = Arc::new(client);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let execution = super::super::app::LiveExecution {
            run_id: "r".into(),
            stage: "review".into(),
            stage_type: "exec".into(),
            stage_run_id: "review#1".into(),
        };
        let task = tokio::spawn(replay_selection(
            client.clone(),
            "r".into(),
            Some(execution),
            "review".into(),
            1,
            tx,
        ));
        let mut peer = BufReader::new(peer);
        socket::read_json_line(&mut peer).await.unwrap().unwrap();
        socket::write_json_line(
            peer.get_mut(),
            &json!({"type":"error", "message":"unavailable"}),
        )
        .await
        .unwrap();
        timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rx.recv().await.unwrap()["type"], "view_error");
        let mut session = BufReader::new(session);
        assert!(timeout(
            Duration::from_millis(30),
            socket::read_json_line(&mut session)
        )
        .await
        .is_err());
        client.shutdown().await;
    }

    #[test]
    fn inline_lifecycle_replay_observes_native_only() {
        use crate::tui::{app::App, apply_watch_frame, screen::LiveArea};
        let mut app = App::new();
        app.watch_following = Some("r".into());
        let mut live = LiveArea::with_backend(ratatui::backend::TestBackend::new(80, 24), 80, 24);
        live.show_runs();
        let mut follow = None;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let boundary = json!({"type":"stage_boundary", "run_id":"r", "stage":"work",
            "stage_type":"exec", "stage_run_id":"execution", "active":true});
        send_line(&tx, "r", 7, boundary.clone());
        apply_watch_frame(&mut app, &mut live, &rx.try_recv().unwrap(), 7, &mut follow);
        assert!(
            app.live_executions.is_empty(),
            "disk must not establish liveness"
        );
        for frame in [json!({"type":"watched", "replay":true}), boundary] {
            send_native(&tx, "r", 7, frame);
            apply_watch_frame(&mut app, &mut live, &rx.try_recv().unwrap(), 7, &mut follow);
            assert!(app.live_executions.is_empty());
        }
        send_native(
            &tx,
            "r",
            7,
            json!({"type":"replay_checkpoint", "id":"r", "complete":true}),
        );
        apply_watch_frame(&mut app, &mut live, &rx.try_recv().unwrap(), 7, &mut follow);
        assert_eq!(app.live_executions.len(), 1);
    }

    #[tokio::test]
    async fn rejection_reads_bounded_tail_without_replacement_follow() {
        use gremlins::executor::socket;
        use tokio::{
            io::BufReader,
            net::UnixStream,
            time::{timeout, Duration},
        };
        let (native, peer) = UnixStream::pair().unwrap();
        let (replacement, replacement_peer) = UnixStream::pair().unwrap();
        let (client, [_control, session]) = DaemonClient::fake_with_aux_streams(
            json!({"session_id":"owner", "chat_run_id":"session_chat"}),
            vec![native, replacement],
        );
        let client = Arc::new(client);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(replay_selection(
            client.clone(),
            "r".into(),
            None,
            "".into(),
            7,
            tx,
        ));
        let mut peer = BufReader::new(peer);
        let request = socket::read_json_line(&mut peer).await.unwrap().unwrap();
        assert_eq!(request["op"], "watch");
        socket::write_json_line(
            peer.get_mut(),
            &json!({"type":"error", "message":"unsupported"}),
        )
        .await
        .unwrap();
        let mut session = BufReader::new(session);
        let request = timeout(Duration::from_secs(2), socket::read_json_line(&mut session))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(request["op"], "read_blob");
        assert_eq!(request["tail"], 500);
        socket::write_json_line(
            session.get_mut(),
            &json!({"type":"blob", "lines":["same", "same"]}),
        )
        .await
        .unwrap();
        timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        let mut frames = Vec::new();
        while let Some(frame) = rx.recv().await {
            frames.push(frame);
        }
        assert!(frames.iter().all(|f| f.get("native").is_none()));
        assert_eq!(
            frames
                .iter()
                .filter(|f| f["line"].as_str().is_some_and(|s| s.contains("same")))
                .count(),
            2
        );
        assert_eq!(frames.last().unwrap()["type"], "view_error");
        assert_eq!(frames.last().unwrap()["force"], true);
        let mut replacement = BufReader::new(replacement_peer);
        assert!(timeout(
            Duration::from_millis(50),
            socket::read_json_line(&mut replacement)
        )
        .await
        .is_err());
        client.shutdown().await;
    }

    fn text(text: &str) -> Value {
        json!({"type": "stream_chunk", "text": text})
    }

    #[tokio::test]
    async fn checkpoint_preserves_order_duplicates_and_live_frames() {
        use std::future::{poll_fn, Future};
        use std::task::Poll;

        let (tx, mut rx) = mpsc::channel(256);
        let frames = vec![text("hé"), text("hé"), json!({"type":"stage_boundary"})];
        let snapshot = {
            let mut collector = std::pin::pin!(collect_checkpoint(&mut rx));
            // An empty, still-open receiver must wait, not end the snapshot.
            poll_fn(|cx| {
                assert!(collector.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            for frame in &frames {
                tx.send(frame.clone()).await.unwrap();
            }
            // Draining the available records must still wait for the marker.
            poll_fn(|cx| {
                assert!(collector.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            tx.send(json!({"type":"replay_checkpoint"})).await.unwrap();
            tx.send(text("live")).await.unwrap();
            collector.await
        };
        assert!(snapshot.fallback.is_none());
        let mut expected = frames;
        expected.push(json!({"type":"replay_checkpoint"}));
        assert_eq!(snapshot.values, expected);
        assert_eq!(rx.recv().await, Some(text("live")));
    }

    #[tokio::test]
    async fn checkpoint_requires_marker() {
        let (tx, mut rx) = mpsc::channel(256);
        tx.send(text("history")).await.unwrap();
        drop(tx);
        let snapshot = collect_checkpoint(&mut rx).await;
        assert!(snapshot.fallback.unwrap().contains("closed"));
        assert_eq!(snapshot.values, vec![text("history")]);
    }

    #[tokio::test]
    async fn checkpoint_collection_is_bounded() {
        let (tx, mut rx) = mpsc::channel(65538);
        for _ in 0..MAX_SNAPSHOT_RECORDS {
            tx.send(text("same")).await.unwrap();
        }
        tx.send(text("still live")).await.unwrap();
        let snapshot = collect_checkpoint(&mut rx).await;
        assert_eq!(snapshot.values.len(), MAX_SNAPSHOT_RECORDS);
        assert!(snapshot.fallback.unwrap().contains("limit"));
        assert_eq!(rx.recv().await, Some(text("still live")));
    }

    #[tokio::test]
    async fn gap_collects_retained_history_through_checkpoint() {
        let (tx, mut rx) = mpsc::channel(256);
        let gap = json!({"type":"history_gap", "evicted_through_sequence":42});
        tx.send(gap.clone()).await.unwrap();
        tx.send(text("retained")).await.unwrap();
        tx.send(json!({"type":"replay_checkpoint", "complete":false}))
            .await
            .unwrap();
        tx.send(text("retained")).await.unwrap();
        let snapshot = collect_checkpoint(&mut rx).await;
        assert!(snapshot.fallback.unwrap().contains("history_gap"));
        assert_eq!(
            snapshot.values,
            vec![
                gap,
                text("retained"),
                json!({"type":"replay_checkpoint", "complete":false})
            ]
        );
        assert_eq!(rx.recv().await, Some(text("retained")));
    }

    #[tokio::test]
    async fn incomplete_checkpoint_without_gap_preserves_snapshot_and_future_duplicates() {
        let (tx, mut rx) = mpsc::channel(256);
        // The incomplete marker alone must trigger fallback, without history_gap.
        tx.send(text("same")).await.unwrap();
        tx.send(json!({"type":"replay_checkpoint", "complete":false}))
            .await
            .unwrap();
        tx.send(text("same")).await.unwrap();
        tx.send(text("same")).await.unwrap();
        let snapshot = collect_checkpoint(&mut rx).await;
        assert!(snapshot.fallback.unwrap().contains("incomplete"));
        assert_eq!(
            snapshot.values,
            vec![
                text("same"),
                json!({"type":"replay_checkpoint", "complete":false})
            ]
        );
        assert_eq!(rx.recv().await, Some(text("same")));
        assert_eq!(rx.recv().await, Some(text("same")));
        assert!(fallback_reason(&json!({"type":"replay_checkpoint", "complete":true})).is_none());
    }

    #[test]
    fn gap_is_forwarded_as_visible_log_line() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(send_line(&tx, "r", 7, json!({"type":"history_gap"})));
        let wrapped = rx.try_recv().unwrap();
        let line: Value = serde_json::from_str(wrapped["line"].as_str().unwrap()).unwrap();
        assert_eq!(line["type"], "log_line");
        assert!(line["line"].as_str().unwrap().contains("history_gap"));
        assert_eq!(
            crate::tui::screen::classify_stream_value(&line).as_deref(),
            Some("log")
        );
    }

    #[test]
    fn native_forwarding_preserves_duplicates_and_checkpoint() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let frames = vec![
            text("same"),
            text("same"),
            json!({"type":"native_future_event", "data":42}),
            json!({"type":"replay_checkpoint", "complete":true}),
        ];
        for frame in frames {
            assert!(send_native(&tx, "r", 7, frame.clone()));
            let wrapped = rx.try_recv().unwrap();
            assert_eq!(wrapped["id"], "r");
            assert_eq!(wrapped["generation"], 7);
            assert_eq!(wrapped["native"], frame);
            assert_eq!(
                serde_json::from_str::<Value>(wrapped["line"].as_str().unwrap()).unwrap(),
                frame
            );
        }
        assert!(rx.try_recv().is_err());
    }

    // Persisted-history decoding only: these formats do not establish native
    // replay coverage or justify deduplicating matching live frames.
    #[test]
    fn decode_exec_metadata_and_output() {
        let history = decode_history(
            vec![
                "=== build ===".into(),
                "cwd: /tmp".into(),
                "cmds: 1".into(),
                "commands:".into(),
                "  original: cmd".into(),
                "  resolved: cmd".into(),
                "error".into(),
                "--- exit: 1 (duration: 0.1s) ---".into(),
            ],
            "command_logs/exec-build.log",
            "r",
        );
        assert_eq!(history.len(), 8);
        for index in [0, 1, 2, 3, 4, 5, 7] {
            assert_eq!(history[index]["type"], "log_line");
            assert_eq!(history[index]["replay_metadata"], true);
        }
        assert_eq!(
            history[6],
            json!({"type":"exec_output", "id":"r", "is_stderr":false, "text":"error", "ts":""})
        );
    }

    // Assistant aggregates are persisted transcript records, not coverage markers.
    #[test]
    fn decode_stream_transcript_and_log() {
        let record = json!({"type": "assistant", "message": {"content": [
            {"type": "text", "text": "hello"},
            {"type": "thinking", "thinking": "reason"},
            {"type": "tool_use", "id": "t", "name": "Bash", "input": {"command": "ls"}}
        ]}});
        let result = json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "t", "content": "ok"}]}});
        let decoded = decode_history(
            vec![
                text("native").to_string(),
                record.to_string(),
                result.to_string(),
                "raw".into(),
            ],
            "artifacts/stream-agent.jsonl",
            "r",
        );
        assert_eq!(decoded[0], text("native"));
        assert_eq!(decoded[1], text("hello"));
        assert_eq!(
            decoded[2],
            json!({"type":"reasoning_chunk", "text":"reason"})
        );
        assert_eq!(decoded[3]["type"], "tool_output_chunk");
        assert_eq!(decoded[3]["call"], "Bash(command=\"ls\")");
        assert_eq!(decoded[4]["output"], "ok");
        assert_eq!(decoded[5], log_line("r", "raw"));
        assert_eq!(
            decode_history(vec!["[now] hello".into(), "plain".into()], "log", "r"),
            vec![log_line("r", "hello"), log_line("r", "plain")]
        );
    }
}
