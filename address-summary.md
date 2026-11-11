# Fix summary: `clients/agent/` submodule extraction

## `crates/gremlins/src/clients/agent/mod.rs`

**Removed dead re-export block (lines 8–26).**
- **Category**: Public API / Readability (review one §1–3, review two §2–3, structural review §1–3).
- **What changed**: Deleted the entire `#[allow(unused_imports)]`-guarded re-export block containing `run_agent_loop`, `LoopOpts`, `RunContext`, `done_user_message`, `LoopHooks`, `agent_system_prompt`, `build_chat_preamble`, `chat_system_prompt`, `fork_system_prompt`, `task_system_prompt`, `BATCHING_GUIDANCE`, `build_task_selector`, `TaskModel`, `TaskModelOverrides`, and `TaskModelSelector`. Every external consumer uses the legacy path through `clients/mod.rs` (`crate::clients::agent_loop::*`, etc.), so these re-exports were dead code. The blanket `#[allow(unused_imports)]` was masking that fact.
- **Why not `LoopState`**: `LoopState` is `struct LoopState` (private, no `pub(crate)`); it is not reachable outside `agent_loop.rs` and should not be re-exported. The plan was incorrect to list it.

## `crates/gremlins/src/clients/mod.rs`

**Replaced comment with TODO for dual-path migration (line 3).**
- **Category**: Readability (review two §4, structural review summary).
- **What changed**: Replaced the `// Re-export submodules so existing paths continue to resolve.` comment with a TODO committing to migrate the three known consumers (`backend.rs`, `native.rs`, `conversation/standard.rs`) to `crate::clients::agent::*` paths and then delete the six module-alias re-exports.

## Files verified with zero changes needed

- **`done_helpers.rs`** (review one §4, review two §1): Imports only `rig_core::completion::Message` — no `super::` references. The move was safe as-is.
- **`task_models.rs`** (review one §5, review two §1): Imports `crate::task_kind::TaskKind`, `std::*`, and `rig_core::*` — no `super::` references. Safe.
- **`system_prompts.rs`** (review two "unverifiable" note): Only imports `std::path::Path`. The `include_str!` depth bump (→ `../../../prompts/`) was correct. Safe.
- **`agent_loop.rs` imports**: The one remaining `super::plug::LoopHooks` is correct (sibling in `agent/`). All parent-`clients` references correctly use `crate::clients::*`.