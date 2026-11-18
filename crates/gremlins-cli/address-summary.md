# Fix Summary

## Compile fixes

1. **`crates/gremlins/src/executor/supervisor/attachment.rs`, line 44** — Missing import
   Added `use tokio::sync::watch;`. The `acquire_lease` and `dispatch_attach_command` functions reference `watch::Receiver<RunState>` but the import was absent, causing `cannot find module or crate 'watch'`.

2. **`crates/gremlins/src/executor/supervisor/attachment.rs`, line 283** — Immutable binding
   Changed `let Some((handle, store, run_state))` to `let Some((handle, store, mut run_state))`. The binding is passed as `&mut run_state` to `dispatch_attach_command`, which borrows it mutably to call `wait_for`.

3. **`crates/gremlins/src/executor/supervisor/attachment.rs`, line 529** — Immutable binding
   Changed `let delivery_task` to `let mut delivery_task`. The `JoinHandle` is polled via `&mut delivery_task` in the `tokio::select!` that awaits delivery confirmation.

4. **`crates/gremlins/src/clients/interactive.rs`, line 134** — Missing `Debug` impl
   Added a manual `impl std::fmt::Debug for PauseSlot`. `AtomicU64` (the `acquisition` field) does not implement `Debug`, so `#[derive(Debug)]` on `InteractiveSessionFactory` (which holds `Arc<PauseSlot>`) failed. The new impl composes only the debuggable fields (`state`, `nonce`, `execution_id`) and calls `finish_non_exhaustive()`.

## Correctness fixes

5. **`crates/gremlins/src/executor/supervisor/chat/mod.rs`, line 253** — Silent ready-failure path
   The `if (&mut attachment.ready).await.is_err()` branch returned without writing any frame, leaving the targeted correction request unanswered. Added a `correction_dropped` JSON frame (with execution_id when available) before the `return`.

6. **`crates/gremlins/src/executor/supervisor/chat/mod.rs`, line 211** — Missing execution ID validation
   The targeted correction path validated only `nonce`, not the client-supplied execution ID. Added parsing of `target.execution_id` from the request and a mismatch check against `debug_target.execution_id()` before attaching. When the client supplies an execution ID that doesn't match the current target's, the handler responds with `"execution_id mismatch: target has been replaced"`.

## Readability fix

7. **`crates/gremlins/src/executor/supervisor/attachment.rs`, line 24** — Misleading doc comment
   The `detach` row in the module header table said "Close this attachment, keep observing" but the impl emits `detached` and breaks the loop (closing the connection). Fixed to "Release the lease, close this attachment."