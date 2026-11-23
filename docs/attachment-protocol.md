# Attachment and watch replay protocol

## Passive run scope

A dedicated JSON-lines connection sends:

```json
{"op":"attach","scope":"run","id":"RUN","session_id":"SESSION"}
```

The target must be live in `RunRegistry`. A stale/finished target receives an
error before acknowledgment. Subscription is established before the server
writes:

```json
{"type":"attached","scope":"run","id":"RUN","capabilities":["observe"]}
```

Run scope observes across stage changes. It acquires no pause lease and offers
no correction address. Client input or disconnect ends this passive connection;
it does not cancel the run.

Display records use the native serialized `EventKind` schema (`type`), including
agent chat/display events, `ready`, `stream_reset`, `skill_loaded`,
`exec_output`, and `log_line`. There is no legacy `stage_stream` operation,
client, or stage-stream envelope. Lifecycle records remain:

| Type | Fields |
|---|---|
| `stage_boundary` | `id`, `stage`, `stage_type`, `active` |
| `stage_transition` | `id`, `stage`, optional `sub_stage` |
| `run_completed` | `id`, `exit_code` |
| `run_failed` | `id`, `exit_code`, `error` |
| `run_stopped` | `id` |
| `lagged` | `skipped` |

Only run-terminal records close the run follow; a stage's `done` or `ended`
is not run termination. Producer-side `stage_boundary.rs` remains in place.

The server uses a lag-tolerant subscription (capacity 256). Bus run-prefix
matching includes descendants, so attachment additionally requires the exact
correlation run id and, for id-bearing payloads, the exact payload id. Foreign
or child events cannot appear in or terminate this stream. Slow observation
can produce `lagged`; it must not backpressure the producer.

## CLI watch history handoff

`DaemonClient::follow_run` waits for and validates the `attached` type, run
scope, and target id before returning the receiver. Native frames already
buffered after acknowledgment are forwarded unchanged, as are future records.

`follow_run` requests `replay:true` and requires the acknowledgment capability
`replay_checkpoint` (advertised alongside `observe`). Without it, attachment fails promptly
rather than waiting for a marker an older server may never send. A replay ack is:

```json
{"type":"attached","scope":"run","id":"RUN","capabilities":["observe","replay_checkpoint"]}
```

The bus atomically captures native run history and registers live delivery under the same mutex used for journal
insertion and passive delivery. The server sends `attached`, historical native
frames, `replay_checkpoint`, then live frames. Each emission belongs to exactly
one side of that cut. For complete native replay, the client collects a bounded
snapshot and consumes the marker; there is no settling window, content matching,
or suppression of identical future records. Slow viewers can still receive
explicit live `lagged` reports.

The journal retains at most **16,384 records per exact run** and **65,536
records globally**. Oldest records are evicted at either limit. Run bookkeeping
exists only for retained records; there is no unbounded map of retired runs.
These are record limits, not byte limits: individual payload sizes vary.
`PromptRequest` payloads are not retained in history (live delivery is unchanged).
Other retained display payloads can still contain sensitive text.

For incomplete history, the server sends this before retained native frames:

```json
{"type":"history_gap","reason":"history_incomplete","through_sequence":42,"evicted_through_sequence":39}
```

Every replay ends with:

```json
{"type":"replay_checkpoint","scope":"run","id":"RUN","complete":false,"through_sequence":42,"evicted_through_sequence":39}
```

`through_sequence` counts all run-scoped emissions at the atomic cut, including
omitted records. `evicted_through_sequence` is the highest evicted or omitted
sequence anywhere in the process (zero initially). Completeness is conservative:
after any eviction or omission anywhere, all later snapshots report `false`,
even if this run's records were unaffected. This avoids unbounded tombstones.
Native historical and live display/lifecycle frames carry `sequence` from the
bus emission. Sequences reset on process restart and are not persisted replay
cursors. Protocol control records such as `lagged` are not journal emissions.
A complete checkpoint has `complete:true`; no preceding gap is sent.

The CLI snapshot buffer is limited to 65,536 frames and its socket-reader queue
to 256 frames. Socket stalls can cause server-side live `lagged` reports, never
producer backpressure. In complete native replay, identical records on either
side of the atomic cut are valid and are not suppressed by content matching.

On a history gap, incomplete checkpoint, snapshot limit, premature EOF, or failed
attachment (including missing replay capability), the client reads up to 500
persisted tail lines once:
agent `artifacts/stream-<safe-stage>.jsonl`, exec
`command_logs/exec-<safe-stage>.log`, or the run log. It does not open a second
live log stream over this fallback and never opens a replacement attachment.
For incomplete active replay, disk history is displayed with a visible
**history discontinuity / possible overlap** warning, followed by every collected
original native frame and continued delivery from that same subscription. Disk
read failure is reported without discarding original frames or ending live follow.
There is **no deduplication**: no sequence suppression, content matching, frozen
anchor, or occurrence budget, before or after handoff. Repeated output is preserved.

Disk tails have no execution-scoped coverage contract:

- Exec replay reads the raw `.log`, including its preamble/footer metadata.
  Legacy `.events` sidecars are ignored wholesale and new sidecars are not written.
  Their process-local sequence IDs can collide after daemon restart; treating them
  as durable identities could silently suppress unrelated live output.
- Bounded blob tails assert no `through_sequence` or `covered_sequences` coverage
  (the response fields are null). A bus sequence sampled around disk I/O is not a
  persistence watermark. The compatibility replay-cut helper returns zero and
  runs the read outside the bus mutex; zero covers no positive live sequence.
- Operational watch fallback uses a bounded run-log tail, not ordinary full log
  follow. Ordinary full log follow still uses its persisted `through_sequence`
  cut for `log_line`; that contract is separate and is not applied to watch tails.
- Agent stage transcripts can include other executions of that stage. Exact
  execution-scoped agent history/live merging is **deferred**, not implemented.

Complete native replay has an exact atomic snapshot/live cut. Incomplete disk
fallback is deliberately not an exact merge and makes no lossless coverage claim.
Persistence callbacks and disk reads run outside the bus state mutex: journal
insertion, routing/nonblocking enqueue, and snapshot/subscription registration
are synchronized, but persistence is not atomic with publication.
Historical artifacts remain unchanged. After daemon restart, finished/stale runs
fail live attachment and still use persisted history; the process-local journal
is not a replacement for persisted replay.
A UI-local `view_log` wrapper carries target/generation and the serialized
native record for routing; it is not a daemon wire envelope.

## Conversation scope and debug CLI

Conversation attachment remains separate: it binds an execution and supports
observation/control or debugger pause-on-entry for interactive agents. Exec
stages remain read-only. Capability and execution/nonce validation determine
whether operator messages may be sent; run attachment grants none of this.

The standalone debug CLI already sends:

```json
{"op":"attach","scope":"conversation","id":"RUN","mode":"debugger"}
```

A standalone debug subprocess integration test covers this routing.

See the [retirement tracker](actor-removal.md) for targeted test scope and
verification limits; no rollout or manual verification is claimed.
