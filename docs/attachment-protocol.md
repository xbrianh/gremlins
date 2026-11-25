# Attachment, input, and watch replay protocol

## Exact execution input

The global control socket and verified session auxiliary socket accept a dedicated
input connection:

```json
{"op":"input","id":"RUN","stage_run_id":"INVOCATION","stage":"build","session_id":"SESSION","req_id":"bind-1"}
```

`id`, `stage_run_id`, and `req_id` are required nonempty strings. `stage` is an optional
nonempty consistency check, not a selector for a future execution. A nested
`target` object may carry these identity fields instead. Retired `stage_id` and
`execution_id` fields are rejected at either level. The live input registration,
not a display event or stage name, is authoritative. Missing, stale, or revoked
registrations fail before acknowledgment. There is no waiting for a successor.

The response is `input_bound`, with `id`, `stage`, `stage_run_id`, and the bind
`req_id`. Binding claims exclusive controller ownership for this exact invocation.
It does not acquire a debugger pause lease, subscribe to display, replay history,
or pause the agent. Commands are dispatched only after the bind acknowledgment
has been written successfully. First acquisition wins across global, session, targeted chat,
and still-writable legacy conversation attachments. A competing bind receives
`{"type":"error","message":"controller_busy","req_id":"bind-1"}` immediately;
there is no queue or automatic handoff. Watchers never contend. After release,
a rejected caller must explicitly retry. Release sends no execution command:
a paused execution stays paused, and accepted corrections retain their receipts.
Late cleanup releases only its own acquisition, never a newer holder.
One-shot targeted chat holds the same claim only through its enqueue attempt,
not through receipt delivery or socket flushing. Chat-kind runs require
the owning, open session: a claimed session id on a global socket is insufficient.
The session listener supplies the trusted owner handle and checks its chat ids.

Subsequent commands have a nonempty string `req_id`:

```json
{"cmd":"message","text":"Use the existing helper","req_id":"correction-1"}
{"cmd":"pause","req_id":"pause-1"}
{"cmd":"step","req_id":"step-1"}
{"cmd":"resume","req_id":"resume-1"}
{"cmd":"bail","req_id":"bail-1"}
```

Commands cannot carry identity fields or `target`: the socket remains bound to its
original input sender even after an execution is replaced. `message` requires
nonempty text. Accepted commands receive `queued`, followed by `delivered` when
the consumer confirms incorporation/application, or `dropped` when delivery is
not confirmed. These frames echo `req_id`; queue acceptance alone is not delivery.
Rejected commands receive `error` with `message` and `req_id`, never `queued`.
The execution inbox is bounded; a full or revoked inbox rejects immediately.
Clients should use distinct request ids for unambiguous receipt correlation.
At most 32 commands may await receipts on one connection. Excess requests receive
a correlated error without closing the connection or discarding accepted receipts.

`{"cmd":"detach"}` or EOF releases the controller claim without cancelling the
run. Execution revocation settles outstanding receipts and emits `ended`; this
connection never follows another invocation. Disconnect does not retract commands
already accepted by the execution. Socket writing runs separately with bounded
output and receipt tracking. Each write, including the bind acknowledgment, has
a five-second timeout; expiry releases control without issuing a command.
A slow peer cannot indefinitely retain ownership or
block input lifecycle cleanup; overflow closes that connection, and outstanding
wire notifications are best effort after disconnect. No display frames are sent.


## Read-only filtered bus watch

Both the control socket and verified session auxiliary socket accept:

```json
{"op":"watch","id":"RUN","stage":"build","stage_run_id":"INVOCATION","replay":true,"session_id":"SESSION"}
```

`id` is required. `stage` and `stage_run_id` are optional nonempty strings;
when both are supplied, both must match. Retired `stage_id` and `execution_id`
keys are rejected, including inside `target`; watch does not accept `target`.
Unknown request fields are rejected rather than silently broadening a selector.
Supported envelope fields are `op`, `session_id`, and `req_id`; `scope`, if
supplied, must be `"run"`. Session verification and run resolution precede
subscription and acknowledgment.
No input registration or pause lease is consulted or acquired.

The server registers a bounded, lag-tolerant correlation subscription before
sending `watched`, with `id`, `scope:"run"`, `stage`, `stage_run_id`, and the
negotiated `replay` boolean. Absent filters are reported as null. A run watch
survives stage changes; a stage-name watch includes later invocations of that
name; an invocation watch never follows a replacement.

Frames retain native payloads and sequences, adding correlation `run_id` and,
when present, `stage` and `stage_run_id`. If a native payload already declares a
different `stage` (such as an enclosing display stage on `stage_transition`),
that payload is preserved and `correlation_stage` identifies the invocation's
stage instead. Payload `id` remains untouched (for
example, a tool call ID). Descendant runs match the parent selector but retain
their actual run identity; unrelated runs and internal `prompt_request` traffic
are excluded. Child completion does not terminate the parent watch.

Replay uses the bus's atomic retained snapshot and live receiver: an optional
`history_gap` with native watermarks precedes retained matching frames, then a
`replay_checkpoint` reports completeness and watermarks before live delivery.
Live mailbox overflow is a separate `lagged` report. Live-only watches send no
checkpoint. Commands receive a read-only error and close only that watch;
disconnection releases the subscription, never the execution.

The older `attach` request remains available unchanged.

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
| `stage_boundary` | `id`, `stage_run_id`, `stage`, `stage_type`, `active` |
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

Conversation attachment remains separate: it binds a stage run and supports
observation/control or debugger pause-on-entry for interactive agents. Exec
stages remain read-only. Capability and stage-run/nonce validation determine
whether operator messages may be sent; run attachment grants none of this.

`stage_run_id` is the opaque invocation identity, distinct from the run `id`
and the reusable stage name `stage`. Conversation acknowledgments and targeted
requests use `stage_run_id`; pin both it and `nonce` when targeting a particular
live invocation. A repeated stage or a new chat turn has a new stage run ID.
Historical run artifacts are not rewritten by this naming change.

The standalone debug CLI already sends:

```json
{"op":"attach","scope":"conversation","id":"RUN","mode":"debugger"}
```

A standalone debug subprocess integration test covers this routing.

See the [retirement tracker](actor-removal.md) for targeted test scope and
verification limits; no rollout or manual verification is claimed.
