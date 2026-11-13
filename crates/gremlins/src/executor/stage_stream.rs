//! Thin bus proxy — server-side handler for the `stage_stream` session op.
//!
//! The bus is the single transport for stage content. This handler
//! subscribes losslessly to the run's events (`r:<id>`) and forwards every
//! event verbatim, as JSON, to the client. All classification, replay, and
//! deduplication live on the client side.

use serde_json::Value;
use tokio::io::BufReader;
use tokio::net::unix::OwnedReadHalf;

use crate::executor::socket;
use crate::executor::supervisor::{error_response, init_bus, subscribe_run, SharedWriter};
use crate::executor::EventKind;
use crate::magic_bus::Recv;

// ---------------------------------------------------------------------------
// handle_stage_stream
// ---------------------------------------------------------------------------

/// Streaming op: proxy a gremlin's run events to the client.
///
/// Lossless subscription means no event is dropped; the client receives every
/// `EventKind` in emission order until it disconnects or the bus closes.
///
/// When `follow` is explicitly `false`, the handler returns immediately after
/// subscribing — the caller only wants a connection check; all actual
/// streaming is client-driven.
pub(crate) async fn handle_stage_stream(
    request: &Value,
    mut reader: BufReader<OwnedReadHalf>,
    writer: &SharedWriter,
) {
    let id = request.get("id").and_then(|v| v.as_str()).unwrap_or("");
    if id.is_empty() {
        let resp = error_response("missing 'id' field");
        let _ = writer.write_json_line(&resp).await;
        return;
    }
    if crate::executor::gremlin::validate_gremlin_id(id).is_err() {
        let resp = error_response(&format!("invalid gremlin id {id:?}"));
        let _ = writer.write_json_line(&resp).await;
        return;
    }

    let follow = request
        .get("follow")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let (sub, mut bus_rx) = subscribe_run(id);

    if !follow {
        // Caller only wants a liveness check; unsubscribe and return.
        init_bus().unsubscribe(&sub);
        return;
    }

    loop {
        tokio::select! {
            result = bus_rx.recv() => {
                match result {
                    Recv::Event(dispatch) => {
                        let value = match serde_json::to_value(dispatch.into_kind()) {
                            Ok(value) => value,
                            Err(e) => {
                                log::warn!("stage_stream: serialize event failed: {e}");
                                continue;
                            }
                        };
                        if writer.write_json_line(&value).await.is_err() {
                            break;
                        }
                    }
                    Recv::Lagged(_) => {
                        // Lossless subscriptions never lag.
                    }
                    Recv::Closed => break,
                }
            }
            _ = socket::read_json_line(&mut reader) => break,
        }
    }

    init_bus().unsubscribe(&sub);
}

// ---------------------------------------------------------------------------
// stage_boundary helpers
// ---------------------------------------------------------------------------

/// Emit a StageBoundary event on the bus.
///
/// The bus is the single transport for stage boundaries and stage content:
/// subscribers receive this event in emission order, before the stage content
/// it opens.
pub(crate) fn send_stage_boundary(gremlin_id: &str, stage_name: &str, stage_type: &str) {
    let active = !stage_name.is_empty();
    crate::executor::supervisor::emit_event(EventKind::StageBoundary {
        id: gremlin_id.to_string(),
        stage: stage_name.to_string(),
        stage_type: stage_type.to_string(),
        active,
    });
}
