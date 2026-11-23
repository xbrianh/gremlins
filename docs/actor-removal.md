# Actor-removal tracker

## Run-attachment watch replay — conservative implementation

This tracks the run-watch retirement slice, not completion of actor removal or
rollout. See [attachment protocol](attachment-protocol.md) for the wire contract.

## Implemented behavior

- `stage_stream` is removed; producer-side stage boundaries remain.
- Passive native run attachment filters exact correlation/payload run ids,
  forwards agent/exec display and lifecycle records, and reports live lag without
  producer backpressure. Conversation debugger attachment remains separate.
- Complete native replay has an exact atomic snapshot/live cut. Replay requests
  negotiate `replay_checkpoint` in the `attached` capabilities; the client rejects
  missing capability instead of waiting indefinitely. Native historical and live
  frames carry process-local emission sequences.
- The journal retains at most 16,384 records per exact run and 65,536 globally,
  with bounded retained-run bookkeeping. Prompt-request payloads are omitted.
  Eviction/omission yields explicit gaps and conservative checkpoint completeness.
- CLI snapshot collection is bounded to 65,536 frames; the socket-reader queue
  is bounded to 256 frames.
- Incomplete/unavailable replay reads a one-shot persisted tail of up to 500 lines,
  including after daemon restart. Active fallback displays disk history and every
  collected original native frame, then continues the original live subscription.
  A visible discontinuity/possible-overlap warning explains that duplicates are
  preserved. Disk read errors do not discard native frames or stop live following.
- No replacement attachment, second live log follow, sequence suppression,
  content matching, frozen anchor, or occurrence-budget deduplication is used.
- Exec fallback reads raw logs with preamble/footer metadata preserved. Legacy
  `.events` sidecars are ignored and new ones are not written: process-local
  sequence IDs can collide across daemon epochs.
- Bounded blob/log tails assert no sequence coverage. The compatibility replay-cut
  helper returns zero, not a persisted watermark. Ordinary full log follow still
  uses its persisted `through_sequence` cut for `log_line`; watch fallback does not.
- Bus journal insertion, snapshot/subscription registration, and nonblocking
  delivery are synchronized. Persistence callbacks and disk reads run outside
  the bus mutex; persistence and publication are not an atomic transaction.

## Targeted validation scope

Targeted tests in the tree cover the following behavior. This is a coverage
inventory, not a fresh execution report; no tests were run for this docs-only
update. Historical full-suite counts and blanket passing claims are not evidence
for the current implementation.

- `gremlins-magic-bus` history tests: eviction, bounded bookkeeping, omission,
  completeness watermarks, and snapshot/live separation, including identical
  payloads on either side. Bus tests also cover reentrant persistence callbacks.
- Supervisor attachment tests: replay capability acknowledgment, checkpoint
  framing, live sequences, exact run filtering, and lag-tolerant observation.
- CLI client/watch replay tests: missing-capability rejection, bounded collection,
  incomplete fallback retaining original frames and repeated output, visible
  overlap warnings, disk-read failure handling, and original socket lifetime.
- Executor blob/session tests: raw exec tails, ignored legacy/malformed sidecars,
  retained raw metadata, bounded tails, and null coverage fields.
- Standalone debug subprocess test: conversation-scope debugger routing.

Only targeted test results explicitly reported for a given change should be
recorded as executed results. No full-suite, rollout-complete, or manual
watch/debug/restart verification is claimed here.

## Deferred work and limits

- Exact execution-scoped agent history/live merging is **deferred**. A stage
  transcript can contain other executions; disk fallback is not an exact merge.
- Incomplete fallback intentionally permits overlap. It does not promise lossless
  coverage across arbitrary gaps, concurrent persistence, or daemon restart.
- Limits are records/lines, not a total payload-byte bound. Individual records can
  be large and retained display text can be sensitive.
- Completeness is conservative process-global after any eviction/omission. There
  are no unbounded per-run tombstones or durable cross-restart sequence cursors.
- Passive live observation remains lag-tolerant; an exact snapshot/live cut does
  not guarantee subsequent live delivery to a slow viewer.
- Broader actor-removal completion and operational/manual rollout validation
  remain outside this slice.
