# executor/ — the gremlin runtime layer

The executor is the harness that turns a loaded definition tree into a
running gremlin. It owns the gremlin handle (`Gremlin`), the persisted
`state.json` + artifact registry (`StateStore`/`StateData`), the stage
dispatch loop (`run`), bootstrap/agent/exec execution, parallel fan-out,
and the single-instance daemon (`supervisor`).

The module is deliberately layered: `run` sequences stages but does not
know how a shell command runs; `exec_runner`/`agent_runner` know how a
leaf runs but not how it was selected; `state` knows storage but not what
the fields mean; `supervisor` knows process lifecycle but not stage
semantics. Keep those seams intact.

## File map

| File | Role |
|---|---|
| `mod.rs` | Module declarations and `RunError` — the coarse failure vocabulary the run loop returns. |
| `gremlin.rs` | `Gremlin` handle. Identity validation (`GremlinId`), environment assembly (`resolve_env`/`system_env`), and the `init`/`from`/`fork` constructors. Also `GremlinConfig` (ephemeral vs persistent backing) and `init_runtime` — the single place deferred runtime setup happens. |
| `state.rs` | `state.json` I/O, flock-guarded atomic updates, `StateData`, and the artifact registry. `StateStore` is the unified storage seam; `FileSystemStateStore` is the real implementation. |
| `run.rs` | The sequential stage loop. `Gremlin::run` walks `next_stage()` in declaration order, drives each through `run_stage_scoped`, and owns attempt/`loop_iter`/bail bookkeeping. `run_stage` dispatches by `ExecutorStage` variant. |
| `parallel.rs` | Parallel fan-out/fan-in. Forks one child gremlin per child stage, launches via supervisor, awaits `RunState` channels, applies `ErrorPolicy`, merges artifacts, and cleans up children. |
| `supervisor/` | Single-instance daemon, split into sub-modules since the `break-executor` refactor. `mod.rs` owns the accept loop, `RunHandle`/`RunState`, and op dispatch. See `supervisor/AGENTS.md` for the full sub-module map. |
| `agent_runner.rs` | Agent leaf execution: `prepare_agent` (resolve everything without touching the client) and `commit_agent` (verify outputs were produced). |
| `run_agent.rs` | Agent stage runner: `resolve_task_models`, `run_agent`, chat conversation persistence (`messages_from_conversation`, `persist_chat_conversation_turn`, `read_chat_conversation`, `append_chat_conversation_turn`, `persist_compacted_conversation`), and tool-call accounting. |
| `run_exec.rs` | Exec stage runner: thin wrapper around `prepare_exec` + `run_shell` + `commit_exec` from `exec_runner`. |
| `stage_stream.rs` | Stage stream protocol: `StageStreamKind` classification, live + replay serving (`serve_stage_stream_loop`), `agent_stream_lines`/`exec_stream_line` formatting, and `ReplayDedupe` for resume tail replay. |
| `socket.rs` | Unix-domain socket + daemon lock helpers (`GremlinsDaemonLock`, `bind_socket`, `connect_socket`, JSON-line read/write). The socket lives at `$state_root/executor.sock`. |
| `exec_runner.rs` | Exec leaf execution: `prepare_exec`, `run_shell`, and `commit_exec`. Owns `ExecPrepared` and `ExecError`. |
| `bootstrap.rs` | Worktree bootstrap: `bootstrap.cmds`, `bootstrap.launch_cmds`, and `bootstrap.cli_out`, plus the small `gremlins:bind_artifact(...)` DSL. |
| `vars.rs` | `{var}` substitution. `substitute_vars` is the shared resolver used by bootstrap, exec, and agent paths; `substitute_vars_to_env` exports to environment. |

## Architecture

### Gremlin lifecycle

- `Gremlin::init` starts a fresh run (generates id, reserves state dir,
  creates workdir, writes `state.json` and the hermetic
  `definition.json`).
- `Gremlin::from` reconstructs a handle from a persisted state dir for
  status/cleanup/recovery.
- `Gremlin::fork` spins off a parallel child that inherits the parent's
  artifacts, environment, and optionally a fresh workspace.

Construction is cheap on purpose: `init`/`from` only populate identity
and paths. Definition loading, registry construction, client creation,
and environment resolution are deferred to `Gremlin::init_runtime`,
which `Gremlin::run` calls first. A caller that only wants to read
metadata or `clean` never pays for the runtime.

### Stage dispatch

`run.rs::run_stage_scoped` is the dispatcher: `Agent` → `run_agent`,
`Exec` → `run_exec`, `Sequence` → `run_sequence`, `Parallel` →
`run_parallel`, `Done` → no-op. Scope (`a/b`, `loop~2`) and
`skip_if_exists` guards are sequence-level concerns, not leaf concerns.

Client resolution order lives in `run.rs::resolve_model_spec`:
1. stage's own `model:`; 2. enclosing composite's explicit model;
3. selected default profile's `model-by-stage` (exact → longest prefix);
4. `default-model-by-stage` runtime config (exact → longest prefix);
5. definition `default_model`.

### State and artifacts

Every `state.json` mutation goes through `write_state` or `locked_update`
under an advisory flock; reads are lock-free snapshot reads because every
mutation is rename-atomic. `StateStore` merges state I/O and the artifact
registry into one backend seam, so tests can substitute an in-memory or
scoped store.

The registry is URI-based (`artifact://scope/key`). `is_registered_uri`
accepts both a bare key and its `artifact://` form because pipelines
spell guards/stop conditions either way.

The walker (`gremlins-landscape`) builds the outline; the `Landscape` tool
handler (`clients/tools/handlers/landscape.rs`) serves it on demand. Agent stages
no longer inject a landscape block into the system prompt, so run-loop code in `run.rs`
only uses the stage's `landscape` flag to decide whether the tool is offered.

### Supervisor

The supervisor owns the accept loop and the global `RUN_MAP`. Each live
run is a `RunHandle` (cancel token, task, abort flag, `RunState` watch,
log broadcast, interactive handle, state store). `launch_child` registers
a run and spawns its task; `RunHandle::finish` removes it, publishes the
terminal `RunState`, and emits the terminal `DaemonEvent`.

The lock protocol serializes daemon startup: parent CLI acquires
`executor.lock`, dups the fd, and spawns `gremlins serve <fd>`; the child
reconstructs the lock from the inherited fd (`socket.rs`).

## Key invariants

- **`state.json` is rename-atomic and flock-guarded.** Never write it
  directly from run-loop code; go through `StateData`/`StateStore`.
- **Bail classes are byte-stable.** `reviewer_requested_changes`,
  `security`, `secrets`, `other` are persisted and read cross-process.
  See top-level `AGENTS.md`.
- **Stage names in `state.json` are pipeline-owned.** The harness must not
  invent or rename them.
- **Framework keys are runtime-injected.** `cwd` and `base_ref` (and other
  `FRAMEWORK_KEYS`) are stripped from user options before substitution.
  See `stage_spec/constants.rs`.
- **Attempts namespace artifacts.** `run.rs` appends a per-stage attempt
  to `loop_iter` so retries get distinct artifact namespaces; it is
  restored immediately after the stage so the next stage sees the base
  value.
- **Done markers are resume-stable.** `parallel.rs` strips the per-stage
  attempt suffix from `loop_iter` before computing the group scope, so
  completed children are recognized after resume.
- **Environment rules.** `resolve_env` seeds the five `GREMLIN*` system
  variables before sourcing `bootstrap.env`, then re-asserts them on top —
  bootstrap can shape the environment but cannot redirect harness paths.
- **Bootstrap/exec failures must not crash the daemon.** They surface as
  `RunError::BootstrapFailed` / `RunError::StageFailed`, recorded via
  `record_stage_error`, and the run finishes with a terminal state.
- **`Gremlin::run` owns `finish`.** Every exit path — success, bail, or
  error — must call `finish` so client reaping, cost folding, and
  `write_terminal_state` always run.

## Where to look for…

| You want to … | Look at |
|---|---|
| Change the run loop / stage sequencing | `run.rs::Gremlin::run`, `run_stage_scoped`, `run_sequence` |
| Change model selection | `run.rs::resolve_model_spec` |
| Add or change storage/registry behavior | `state.rs::StateStore` trait + `FileSystemStateStore` |
| Understand state fields/defaults | `state.rs::default_for`, `field_names`, `StateData` |
| Change environment assembly | `gremlin.rs::resolve_env`, `system_env`, `framework_subs` |
| Understand forking | `gremlin.rs::Gremlin::fork`, `parallel.rs::run_parallel` |
| Change parallel error policy / merging | `parallel.rs` — `run_parallel`, `merge_child_artifacts`, `aggregate_child_costs` |
| Add a supervisor op | `supervisor/ops/` — `control.rs` (launch/stop/resume/rm), `model.rs` (model/chat_model/compact), `query.rs` (ls/status/info/artifact/snapshot). Wire new ops through `dispatch_op` in `supervisor/mod.rs`. |
| Add or change stage streaming / replay | `stage_stream.rs` — `serve_stage_stream_loop`, `ReplayDedupe`, `agent_stream_lines` |
| Add or change agent stage execution / conversation I/O | `run_agent.rs` + `agent_runner.rs` |
| Add or change exec stage execution | `run_exec.rs` + `exec_runner.rs` |
| Change the daemon socket/lock protocol | `socket.rs::GremlinsDaemonLock`, `bind_socket`, `connect_socket` |
| Change bootstrap behavior / DSL | `bootstrap.rs::run_definition_bootstrap`, `parse_gremlins_command`, `bind_artifact` |
| Change exec/agent leaf execution | `exec_runner.rs` / `agent_runner.rs` (`prepare_*` + `commit_*`) |
| Change agent system prompt | `agent_runner.rs` (`prepare_agent`'s `system_prompt`) and `clients/system_prompts.rs` |
| Change the `Landscape` tool | `gremlins-landscape/src/lib.rs` (`format_landscape_block_tool`) and `clients/tools/handlers/landscape.rs` |
| Change `{var}` substitution | `vars.rs::substitute_vars`, `substitute_vars_to_env` |

## Error taxonomy

`RunError` (in `mod.rs`) is deliberately coarse and mirrors the outcomes
the Python executor distinguished:

- `Bail` — the definition asked to bail; operator-facing reason.
- `BootstrapFailed` — a bootstrap command exited non-zero.
- `StageFailed` — a stage failed, named for the operator.
- `Git`, `Message`, `Io`, `Client`, `State` — lower-layer errors passed
  through unchanged.

Leaf runners additionally have their own error types (`AgentError`,
`ExecError`) that wrap resolution and artifact-verification failures;
the run loop records those and maps them to `RunError::StageFailed`
rather than leaking runner-specific types to the daemon.
