# Plan11 documentation reconciliation

Legacy attachment compile-fix notes and line-number references are obsolete.
The inspected worktree uses:

- Bus `watch` for display/lifecycle and descriptive stage type, with no writable
  capability or input ownership.
- Exact invocation input for message/pause/step/resume/bail, backed by the shared
  execution inbox and final receipts.
- Supervisor-exclusive controller claims; release never resumes or cancels.
- Targeted chat through the same input bind/inbox; untargeted chat only for an
  idle session's next prompt.
- Rejection of old wire `attach` on global and verified session auxiliary sockets.
  TUI selection uses Watch → Up/Down → Enter only, without pausing.

See [the protocol](../../docs/attachment-protocol.md) for current behavior and
[the audit](../../docs/plan11-code-audit.md) for separate production/test deltas
against `45a8bd545`, including inline test modules and remaining duplicate owners.
The audit distinguishes committed HEAD from uncommitted removal work. No Rust
edits, Rust test execution, or operational rollout verification are claimed by
this documentation update.
