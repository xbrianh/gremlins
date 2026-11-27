# Watch, exact input, and chat protocol

This describes the inspected plan11 worktree. The [code audit](plan11-code-audit.md)
separates committed HEAD from concurrent removal changes and measures production
and test code independently. No runtime verification is claimed by this document.

## Retired attachment

`{"op":"attach"}` is rejected for both run and conversation scopes on the
global control socket and verified session auxiliary socket:
`attach is retired; use watch or exact execution input`.
There is no compatibility debugger controller, pause-on-entry lease, or successor
handoff. Watch Enter is the only TUI selection path: it observes a concrete
invocation without pausing. The TUI has no `/exit` command. Selection itself
neither pauses the invocation nor acquires execution input. Open Watch with
`/watch`, move with Up/Down, then press Enter. Ordinary text corrects the selected
exact agent execution; exec stages are read-only. Native input binds lazily on
submission, and delivery receipts are separate from bus output.
Producer-side `stage_boundary.rs` remains; `stage_stream` is retired.

## Exact execution input

Both sockets accept a dedicated input connection:

```json
{"op":"input","id":"RUN","stage_run_id":"INVOCATION","stage":"build","session_id":"SESSION","req_id":"bind-1"}
```

`id`, `stage_run_id`, and `req_id` are required nonempty strings. `stage` is an
optional nonempty consistency check, never a future-stage selector. A flat
`target` object may carry identity fields instead; mixing top-level identity
fields with `target` is rejected. Optional positive `nonce` is a consistency
check. Retired `stage_id` and `execution_id` fields are rejected at either level.
Unknown bind fields are rejected. A live input registration, not display metadata,
nonce alone, or a stage name, authorizes input. Missing, stale, unsupported, or
revoked registrations fail without waiting for a successor.

The response is `input_bound`, with `id`, `stage`, `stage_run_id`, and bind
`req_id`. Binding claims exclusive supervisor controller ownership for this
exact invocation; it does not pause, subscribe to output, or replay history.
Commands are dispatched only after acknowledgment is successfully written.
First acquisition wins across global input, session input, and targeted chat.
A competitor receives a correlated `controller_busy` error immediately: no queue,
automatic handoff, or automatic retry. Watchers never contend.

Chat-kind runs require their owning open session. A claimed session ID on a
global socket is insufficient; the verified listener supplies the trusted owner
handle and checks its chat IDs.

Subsequent commands carry distinct nonempty `req_id` values:

```json
{"cmd":"message","text":"Use the existing helper","req_id":"correction-1"}
{"cmd":"pause","req_id":"pause-1"}
{"cmd":"step","req_id":"step-1"}
{"cmd":"resume","req_id":"resume-1"}
{"cmd":"bail","req_id":"bail-1"}
```

These are the only execution commands. They cannot carry identity fields or
`target`: the socket stays bound to its original sender even after replacement.
`message` requires nonempty text and does not implicitly resume a paused agent.
Accepted commands receive `queued`, then `delivered` when incorporation/application
is confirmed, or `dropped` otherwise. Queue acceptance is not delivery. Rejection
receives `error` with `message` and `req_id`, never `queued`. The inbox is bounded;
full/revoked inboxes reject immediately. At most 32 commands await receipts per
connection; excess commands receive a correlated error without discarding
accepted receipts.

`{"cmd":"detach"}` is connection cleanup, not an execution command. It or EOF
releases only this controller acquisition, without cancellation or resume. A
paused execution stays paused; accepted commands and confirmed receipts remain
authoritative. Late cleanup cannot release a newer acquisition. Revocation settles
outstanding receipts and emits `ended`; input never follows another invocation.
Socket writing and receipt tracking are bounded and separate from execution.
Writes, including binding, time out after five seconds. Timeout/overflow closes
the connection and releases ownership without issuing an execution command;
wire notifications after disconnect are best effort. No display frames are sent.

## Read-only filtered bus watch

Both sockets accept:

```json
{"op":"watch","id":"RUN","stage":"build","stage_run_id":"INVOCATION","replay":true,"session_id":"SESSION"}
```

`id` is required. `stage` and `stage_run_id` are optional nonempty filters; when
combined both must match. Retired identity keys and `target` are rejected.
Unknown fields are rejected rather than broadening selection. Supported envelope
fields include `op`, `session_id`, and `req_id`; optional `scope` must be `run`.
Session verification and live run resolution precede acknowledgment. No input
registration or pause state is acquired or consulted.

A bounded lag-tolerant correlation subscription is registered before `watched`,
which includes `id`, `scope:"run"`, `stage`, `stage_run_id`, and negotiated `replay`.
Absent filters are null. A run watch survives stage changes; a stage-name watch
includes repeated invocations; an exact invocation watch never follows its
replacement. Exact follow ends at its closing boundary. Parent termination can
end a filtered watch even when the selected invocation is invisible; child
completion cannot terminate its parent watch.

Frames preserve native display/lifecycle payloads and `sequence`, adding actual
correlation `run_id`, `stage`, `stage_run_id`, and `correlation_turn` when present.
A differing payload stage (for example an enclosing `stage_transition`) is
preserved, with `correlation_stage` carrying the leaf route. Payload IDs such as
tool call IDs remain untouched. Descendants match a parent selector but retain
their actual source IDs; siblings outside a narrower selector do not consume its
mailbox. Internal `prompt_request` traffic is excluded.

Display, exec output, logs, stage boundaries/transitions, `run_started`, and run
terminal events share this bus path. `stage_type` comes from invocation envelope
metadata or validated boundaries when available. It is descriptive, **not a
writable capability**, proof of backend support, or input authorization. There is
no writable flag to infer from display. Use the exact input bind and report its
rejection; never guess a current stage or retarget to a replacement.

Replay atomically captures retained history and registers live delivery. Optional
`history_gap` precedes matching retained frames, then `replay_checkpoint` reports
completeness and watermarks before live delivery. Live-only watch has no checkpoint.
Live overflow reports `lagged` separately; slow viewers never backpressure the
producer. Commands receive a read-only error and close only that watch. Disconnect
releases the subscription, not the execution.

## Retention and TUI history handoff

The journal retains at most 16,384 records per exact run and 65,536 globally,
including separate descendant budgets. Oldest records are evicted; bookkeeping
exists only for retained runs. Prompt-request payloads are omitted from retained
history. These are record limits, not byte limits; display text may be sensitive.

`through_sequence` is the process-local emission watermark at the atomic cut;
`evicted_through_sequence` is the highest omitted/evicted sequence anywhere in the
process (initially zero). Completeness is conservatively false after any global
omission/eviction, even for unaffected selectors. There are no unbounded retired-run
tombstones or durable cross-restart cursors. A complete checkpoint has no preceding
gap; protocol control frames are not journal emissions.

The TUI `follow_run` uses `watch`/`watched` and replay, not `attach`/`attached` or
capability negotiation. Its snapshot buffer is bounded to 65,536 frames and socket
reader queue to 256. A complete replay has an exact snapshot/live cut; identical
records on either side are valid and preserved.

On incomplete history, snapshot limit, premature EOF, or unavailable watch, the
TUI reads up to 500 persisted tail lines once: agent
`artifacts/stream-<safe-stage>.jsonl`, exec `command_logs/exec-<safe-stage>.log`, or
the run log. Active fallback warns of **history discontinuity / possible overlap**,
displays disk history and every collected original frame, and continues that same
subscription. Disk read failure does not discard native frames. There is no
replacement watch, second live log follow, sequence/content suppression, frozen
anchor, or occurrence-budget deduplication.

Exec fallback reads raw logs including metadata; legacy `.events` sidecars are
ignored and new sidecars are not written. Bounded tails assert no
`through_sequence`/`covered_sequences` coverage (null); the compatibility replay-cut
helper returns zero and reads outside the bus mutex. Ordinary full log follow's
persisted log-line cut is separate and does not apply to watch tails. Stage
transcripts may contain other executions: exact persisted execution-scoped agent
history/live merging remains deferred. Disk fallback is deliberately not an exact
or lossless merge. Persistence callbacks and disk reads happen outside the bus
mutex; persistence and publication are not an atomic transaction.

CLI `watch --snapshot` instead emits JSONL and exits at `replay_checkpoint`.
Standalone `debug RUN INVOCATION` is input-only; observe output with a separate
watch. CLI `/continue` sends explicit resume, while `/exit` or EOF drains final
receipts and closes local input without resuming or bailing.

## Idle chat versus targeted correction

Untargeted session `chat` is only a new prompt while the session is idle. A busy
session rejects it with `session busy: a turn is in progress; use target for
corrections`; it must not become an implicit correction or replacement turn.
The advisory idle check is backed by the session task's atomic current-turn gate.

Targeted chat requires `target.id`, positive `target.nonce`, and nonempty
`target.stage_run_id`. It uses the same live registration, ownership checks,
exclusive claim, and execution inbox as dedicated input. The one-shot claim lasts
only through the enqueue attempt, not receipt delivery or socket flushing.
Acknowledgment reports queued status; final notification is `delivered` or
`correction_dropped`. It does not attach a debugger, resume implicitly, or start
a new turn. Cancellation of a session turn and stopping/resuming a persisted run
remain distinct lifecycle operations, not alternative execution inboxes.
