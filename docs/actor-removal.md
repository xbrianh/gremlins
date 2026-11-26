# Plan11 removal tracker

The inspected worktree retires legacy debugger attachment ownership. This is a
code/docs inventory, not a rollout claim. See the [protocol](attachment-protocol.md)
and [production/test audit](plan11-code-audit.md). The audit explicitly separates
committed HEAD from concurrent uncommitted removal changes.

## Current ownership

- Bus `watch` is passive display/lifecycle observation, including invocation
  identity and descriptive `stage_type`, never a writable capability.
- Exact execution input owns message/pause/step/resume/bail delivery through one
  bounded inbox with final receipts. The supervisor exclusively claims an exact
  invocation; releasing a claim does not resume or cancel it.
- Targeted chat uses that same registration, inbox, and claim. Untargeted chat is
  an idle-session prompt only. Session cancellation and persisted-run lifecycle
  operations remain distinct.
- Old wire `attach` is rejected on global and verified session auxiliary sockets.
  There is no pause-on-entry debugger lease or successor fallback. TUI `/attach`
  remains a selection command, not a daemon operation.
- Producer stage boundaries remain; legacy stage-stream and debugger helpers do
  not define the display/input path.

## Replay guarantees and limits

Complete retained replay has an atomic native snapshot/live cut. History is
bounded to 16,384 records per run and 65,536 globally, with explicit gaps and
conservative completeness after omission/eviction. Live observation is bounded
and lag-tolerant; an exact replay cut does not make slow live viewers lossless.

The TUI's one-shot 500-line persisted fallback preserves every native frame and
warns of possible overlap. It does not deduplicate or open a replacement live
subscription. Exec fallback ignores legacy sidecars and reads raw logs. Bounded
tails assert no sequence coverage; disk persistence is not atomic with bus
publication. Exact persisted invocation-scoped agent merging remains deferred.
Record/line bounds are not payload-byte bounds; retained text can be sensitive.
Sequences are process-local, not durable restart cursors.

## Validation inventory

This docs-only update ran no Rust tests, full suite, or manual watch/debug/restart
checks. Existing tests are coverage evidence only after their results are reported
for the relevant code revision. Relevant current surfaces include bus filtered
history/live routing, supervisor exact input/claims and socket watch, targeted
corrections and idle chat, CLI exact debug receipts, and TUI watch replay/fallback.
Obsolete conversation-attachment fixtures are not the current protocol contract.
The audit records removed files and remaining validation/display ownership overlap;
it does not infer equivalent replacement coverage from file deletion.
