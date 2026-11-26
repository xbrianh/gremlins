//! Passive JSONL observation. No sessions, leases, input, or detach commands.
use std::io::Write;
#[cfg(unix)]
use std::os::fd::AsRawFd;

use gremlins::executor::{gremlin::validate_gremlin_id, socket};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};

#[cfg(unix)]
fn consumer_closed(stdout: &std::io::Stdout) -> bool {
    let mut fd = libc::pollfd {
        fd: stdout.as_raw_fd(),
        events: libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: fd points to one initialized pollfd for the duration of poll.
    (unsafe { libc::poll(&mut fd, 1, 0) > 0 })
        && fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0
}

#[cfg(not(unix))]
fn consumer_closed(_stdout: &std::io::Stdout) -> bool {
    false
}

pub(crate) async fn run(
    id: &str,
    stage: Option<&str>,
    stage_run_id: Option<&str>,
    snapshot: bool,
) -> Result<(), String> {
    validate_gremlin_id(id).map_err(|_| format!("invalid gremlin id {id:?}"))?;
    for (key, value) in [("stage", stage), ("stage_run_id", stage_run_id)] {
        if value.is_some_and(|value| value.trim().is_empty()) {
            return Err(format!("'{key}' must be a nonempty string"));
        }
    }
    gremlins::config::init_global().map_err(|error| error.to_string())?;
    // SIGINT retains its default disposition: exiting closes the socket, with
    // no detach/control write, including while stdout is backpressured.
    let mut stream = crate::spawn::connect().await?;
    crate::spawn::check_version(&mut stream).await?;
    let mut request = json!({"op":"watch", "scope":"run", "id":id, "replay":true});
    for (key, value) in [("stage", stage), ("stage_run_id", stage_run_id)] {
        if let Some(value) = value {
            request[key] = json!(value);
        }
    }
    socket::write_json_line(&mut stream, &request).await?;
    let mut reader = BufReader::new(stream);
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    let mut acknowledged = false;
    let mut checkpoint_seen = false;
    let mut line = String::new();
    let mut closure_check = tokio::time::interval(std::time::Duration::from_millis(100));
    loop {
        let count = {
            let read = reader.read_line(&mut line);
            tokio::pin!(read);
            loop {
                tokio::select! {
                    result = &mut read => break result.map_err(|error| format!("read watch: {error}"))?,
                    _ = closure_check.tick() => {
                        // Detect a closed consumer even when the server is idle. poll
                        // only observes stdout; it never injects probe bytes.
                        if consumer_closed(&stdout) {
                            return Ok(());
                        }
                        continue;
                    }
                }
            }
        };
        if count == 0 {
            return if !acknowledged {
                Err("executor closed connection before watch acknowledgment".into())
            } else if !checkpoint_seen {
                Err("executor closed connection before replay checkpoint".into())
            } else {
                Ok(())
            };
        }
        if !line.ends_with('\n') {
            return Err("truncated watch frame".into());
        }
        let frame: Value = serde_json::from_str(&line)
            .map_err(|error| format!("malformed watch frame: {error}"))?;
        let kind = frame
            .get("type")
            .and_then(Value::as_str)
            .ok_or("malformed watch frame: missing string type")?;
        if !acknowledged && kind != "error" {
            if kind != "watched"
                || frame["scope"] != "run"
                || frame["id"] != id
                || frame["replay"] != true
                || frame["stage"] != json!(stage)
                || frame["stage_run_id"] != json!(stage_run_id)
            {
                return Err(format!("unexpected watch acknowledgment: {frame}"));
            }
            acknowledged = true;
        }
        let checkpoint = kind == "replay_checkpoint";
        if checkpoint {
            if frame["scope"] != "run"
                || frame["id"] != id
                || frame["stage"] != json!(stage)
                || frame["stage_run_id"] != json!(stage_run_id)
                || !frame["complete"].is_boolean()
                || !frame["through_sequence"].is_u64()
                || !frame["evicted_through_sequence"].is_u64()
            {
                return Err(format!("unexpected replay checkpoint: {frame}"));
            }
            checkpoint_seen = true;
        }
        // Preserve wire JSON, including unknown fields, gaps, and checkpoints.
        if let Err(error) = output
            .write_all(line.as_bytes())
            .and_then(|_| output.flush())
        {
            return if error.kind() == std::io::ErrorKind::BrokenPipe {
                Ok(())
            } else {
                Err(format!("write watch stdout: {error}"))
            };
        }
        if kind == "error" {
            return Err(frame
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("malformed watch error: missing message")
                .to_owned());
        }
        if snapshot && checkpoint {
            return Ok(());
        }
        line.clear();
    }
}
