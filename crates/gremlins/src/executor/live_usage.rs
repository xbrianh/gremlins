//! Bus subscriber that tracks live token usage and dumps prompt debug files.
//!
//! [`spawn_live_usage_fold`] subscribes to the run-scoped event bus before
//! an agent stage begins, folds [`TokenUsage`](gremlins_magic_bus::EventKind::TokenUsage)
//! events into shared snapshot, and persists [`PromptRequest`](gremlins_magic_bus::EventKind::PromptRequest)
//! JSON blobs to the state store when the DEBUG log level is active.
//!
//! The caller drops the returned [`BusSubGuard`] after the agent loop
//! finishes, then awaits the [`JoinHandle`] with a timeout to drain any
//! already-buffered events — notably the final turn's `TokenUsage`.

use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;

use gremlins_magic_bus::connection::BusSubGuard;
use gremlins_magic_bus::stats::UsageStats;
use gremlins_magic_bus::{subscribe_run, EventKind, Recv};

use crate::executor::state;
use crate::executor::state::StateStore;

/// Live usage snapshot: (cumulative_usage, delta_usage, session_cost_usd).
pub(crate) type LiveUsageSnapshot = Option<(UsageStats, UsageStats, Option<f64>)>;

/// Handle returned by [`spawn_live_usage_fold`].
///
/// Drop `guard` to close the bus subscription so the fold task drains any
/// already-buffered events (notably the final turn's `TokenUsage`) and then
/// exits.
pub(crate) struct LiveUsageFold {
    pub guard: BusSubGuard,
    pub task: JoinHandle<()>,
}

/// Emit a `TokenUsage` event with `cost_usd` filled in from the session
/// tracker. The agent loop cannot know the session cost (it owns no client),
/// so it emits `cost_usd: None`. This helper fills in the missing cost so
/// the live UI sees cumulative totals alongside token counts.
fn emit_cost_filled(
    id: &str,
    usage: &UsageStats,
    last_prompt_tokens: Option<u64>,
    last_cached_input_tokens: Option<u64>,
    session_cost: Option<f64>,
) {
    if let Some(cost) = session_cost {
        gremlins_magic_bus::emit_event(EventKind::TokenUsage {
            id: id.to_string(),
            usage: usage.clone(),
            cost_usd: Some(cost),
            last_prompt_tokens,
            last_cached_input_tokens,
        });
    }
}

/// Spawn a bus-subscriber task that folds live token-usage events into the
/// shared snapshot and persists prompt-debug dumps to the state store.
///
/// `gremlin_id` scopes the subscription to a single agent run.  `store`
/// is the state-store handle used for prompt-dump blob writes.  `live_usage`
/// is the `Arc<Mutex<...>>` snapshot updated on each `TokenUsage` event.
/// `cost_fn` supplies the total session cost in USD (from the backend) at
/// the moment each `TokenUsage` arrives.  `baseline_usage` is the persisted
/// token usage from prior stages, added to each live-usage delta so the
/// live UI sees cumulative totals.
pub(crate) fn spawn_live_usage_fold(
    gremlin_id: &str,
    store: Arc<dyn StateStore + Send + Sync>,
    live_usage: Arc<Mutex<LiveUsageSnapshot>>,
    cost_fn: Box<dyn Fn() -> Option<f64> + Send>,
    baseline_usage: UsageStats,
) -> LiveUsageFold {
    let (usage_sub, mut usage_rx) = subscribe_run(gremlin_id);
    let guard = BusSubGuard::new(usage_sub);
    let live_gremlin_id = gremlin_id.to_string();
    let task = tokio::spawn(async move {
        loop {
            match usage_rx.recv().await {
                Recv::Event(dispatch) => match dispatch.kind() {
                    // Guarded by both the run id (descendant runs share the
                    // parent subscription via ancestor matching) and the
                    // DEBUG threshold (the producer emits unconditionally;
                    // the consumer owns the logging-policy check).
                    EventKind::PromptRequest { id, json }
                        if *id == live_gremlin_id
                            && crate::core::logging::log_level_threshold()
                                >= log::LevelFilter::Debug =>
                    {
                        let ts = state::now_stamp_millis()
                            .trim_matches(|c: char| c == '[' || c == ']')
                            .replace(':', "-");
                        let name = format!("prompt_{ts}.txt");
                        match store.open(&name, state::BlobMode::Write) {
                            Ok(mut blob) => {
                                let _ = std::io::Write::write_all(&mut blob, json.as_bytes());
                                let _ = std::io::Write::write_all(&mut blob, b"\n");
                            }
                            Err(e) => {
                                log::warn!("prompt dump: failed to open {name}: {e}");
                            }
                        }
                    }
                    EventKind::TokenUsage {
                        id,
                        usage,
                        cost_usd,
                        last_prompt_tokens,
                        last_cached_input_tokens,
                    } if *id == live_gremlin_id => {
                        let combined =
                            crate::executor::supervisor::add_usage_stats(&baseline_usage, usage);
                        let session_cost = cost_fn();
                        *live_usage.lock().unwrap() =
                            Some((combined.clone(), usage.clone(), session_cost));
                        // The agent loop emits `cost_usd: None`. Fill in the
                        // missing cost from our session tracker so the live UI
                        // sees cumulative totals.
                        //
                        // We re-emit via the event bus (rather than calling a
                        // live-UI callback directly) so that *every*
                        // subscriber — not just the live-UI thread — sees the
                        // cost-filled event. The `is_none()` guard makes this
                        // a fixed point: our own re-emission has `cost_usd:
                        // Some(…)` and will not match this branch.
                        if cost_usd.is_none() {
                            emit_cost_filled(
                                id,
                                &combined,
                                *last_prompt_tokens,
                                *last_cached_input_tokens,
                                session_cost,
                            );
                        }
                    }
                    _ => {}
                },
                Recv::Lagged(_) => continue,
                Recv::Closed => break,
            }
        }
    });
    LiveUsageFold { guard, task }
}
