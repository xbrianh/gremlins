# Address Summary

Every fix categorized by file, line, category, and change.

---

## `crates/gremlins/src/clients/path_context.rs` — full rewrite

**Category**: Correctness, Performance, Testing

1. **Split constructor into two phases** (plan: "resolution I/O must run inside the existing landscape outline build, not in the `PathContext` constructor") — `new(prompt)` is purely lexical (regex + dedup, no I/O); `resolve(&mut self, project_root)` does all canonicalize/metadata/outline_file work inside `spawn_blocking`.

2. **Broadened tokenizer** (plan: "extensionless file names, directory paths, `~`-prefixed paths") — new regex `CANDIDATE_PATTERN` matches extension-based paths, `/`-containing directory paths, `.`/`~`-prefixed paths, and extensionless paths.

3. **`~` expansion** via `crate::core::paths::expand_user` applied before path joining.

4. **Absolute-path resolution** (plan: "tab-completed absolute paths resolve correctly") — `resolve_candidate` tries absolute paths as-is, then re-anchors on the project root as a fallback.

5. **`..` containment** — rejected outright in `resolve_candidate`.

6. **Symlink containment** — canonicalization + `starts_with(&canonical_root)` check rejects symlinks escaping the project root.

7. **Candidate cap** — `MAX_CANDIDATES = 5` (matches old TUI matcher); size cap kept at 1 MiB.

8. **Removed dead `suppresses_prefetch`** method (plan: "Delete, don't shim").

9. **Updated doc comments** — now describe the module as a tool-dispatch cache rather than a `MutableModelContext`.

10. **11 unit tests** covering tokenization forms, dedup, cap, real-file resolution, absolute re-anchoring, nonexistent → empty, `..` rejection, and Unix symlink escape rejection. All pass.

---

## `crates/gremlins/src/executor/agent_runner.rs` — preamble conditional on chat vs. pipeline

**Category**: Correctness

1. **`build_workspace_preamble` now takes `is_chat: bool`** (line 223) — chat turns get only `"Your working directory is: {cwd}"`; pipeline stages get `"…\nRelevant environment variable: $GREMLIN_WORKDIR"`. This is the *opposite* of the prior hardcoded behavior and matches the plan: "a chat turn (no worktree) emits only the working-directory line and drops the empty `$GREMLIN_WORKDIR` hint."

2. **`user_prompt(&self, is_chat: bool)`** — new parameter threaded through (line 244).

3. **Tests updated** — `test_build_workspace_preamble_pipeline` asserts both lines; `test_build_workspace_preamble_chat_omits_env_var` asserts `$GREMLIN_WORKDIR` is absent for chat. The `test_prepare_workspace_preamble_absent` and `test_prepare_workspace_preamble_present` tests updated with the new semantics. All 27 tests pass.

---

## `crates/gremlins/src/executor/run_agent.rs` — PathContext I/O in spawn_blocking + is_chat threading

**Category**: Correctness, Performance

1. **`is_chat` derived from gremlin kind** (line 133 area) and passed to `prepared.user_prompt(is_chat)` (line 275).

2. **PathContext resolution wrapped in `spawn_blocking`** (lines 308–318) — `PathContext::new(&prepared.prompt)` is lightweight (no I/O); `resolve()` runs inside `tokio::task::spawn_blocking` so blocking file I/O never stalls the async runtime.

---

## `crates/gremlins/src/clients/tools/dispatch.rs` — short-circuit after validation + stale annotation

**Category**: Correctness

1. **PathContext short-circuit moved after validation** (lines 92–102) — now runs after chat-only gating, tool-filter gating, argument parsing, repair, schema validation, and sandbox checks. Only a call that would otherwise be dispatched normally is served from the cache.

2. **Stale-cache annotation** (`path_context_serve`, line 173) — output is prefixed with `[pre-resolved]\n` so the agent knows the outline may be stale after edits.

---

## `crates/gremlins/src/clients/agent/task.rs` — strip path_context from child workers

**Category**: Correctness

1. **`child_ctx.path_context = PathContext::default()`** (line 131) — after cloning the parent context, the child's path cache is reset so the parent's prompt-scoped resolution never leaks into workers (plan: "propagates_to_task is false").

---

## `crates/gremlins-cli/src/tui/chat_prefetch.rs` — no changes needed

**Category**: Readability (confirmed)

- `PathBuf` is still used in the remaining gremlin-ID and definition-name matchers. No unused-import warning.