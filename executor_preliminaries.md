# Preliminaries before porting `gremlins/executor/` to Rust

> Status: this document tracks **remaining** blockers. The target design lives in
> [`rust_executor_design.md`](rust_executor_design.md). The `proc` port
> ([#1536](https://github.com/xbrianh/gremlins/pull/1536)), the `git` port
> ([#1540](https://github.com/xbrianh/gremlins/pull/1540)), and the `env_file` port are now complete.

## What the executor does

Four Python files (~1,480 lines total):

| File | Lines | Role |
|------|-------|------|
| `gremlin.py` | 759 | `Gremlin` class — pipeline construction, init, fork, resume, run orchestration |
| `run.py` | 378 | `run_pipeline` — arg parsing, signal handlers, env isolation, bootstrap trigger |
| `bootstrap.py` | 233 | Shell/bootstrap command execution, DSL commands (`gremlins:bind_artifact`) |
| `parallel_state.py` | 114 | `ParallelGroupState` — **orphaned**; the production path now uses the Rust `ParallelGroupState` in `crates/pyext/src/python/stages.rs`. Only `tests/test_parallel_group_state.py` still imports it. |

The state layer (`State`, `StateData`, `state.json` I/O, `locked_update`, bail files,
token tracking, parallel worktree bookkeeping) is already in Rust at
`crates/gremlins/src/executor/state.rs` (~1,100 lines) with a PyO3 wrapper at
`crates/pyext/src/python/executor.rs`. The orchestration above it is still Python.

Every stage type is also fully ported to Rust — `gremlins/stages/` now contains
only `__init__.py`. Stage implementations live in `crates/gremlins/src/stages/`
(`agent.rs`, `composite.rs`, `exec.rs`, `loop.rs`, `parallel.rs`,
`parallel_bail.rs`, `sequence.rs`, …) with `#[pyclass]` wrappers in
`crates/pyext/src/python/stages.rs` (~4,100 lines).

## Blocking dependencies

The remaining Python-only utilities that the executor files import from
`gremlins.utils.*`:

- `gremlin.py` → `gremlins.utils.yaml_io`
- `run.py` → `gremlins.utils.git` (thin wrapper only)
- `bootstrap.py` → `gremlins.utils.proc` (thin wrapper, already Rust-backed)

### 1. `gremlins.utils.git` — DONE ([#1540](https://github.com/xbrianh/gremlins/pull/1540))

Fully ported to Rust at `crates/gremlins/src/core/git.rs` (595 lines) with PyO3
bindings at `crates/pyext/src/python/utils/git.rs`. The Python file
`gremlins/utils/git.py` is now a 64-line thin wrapper containing just two
filesystem helpers (`setup_workdir`, `stage_gremlins_overlay`) and a re-export of
`GitError`. All git operations — predicates, best-effort readers, fallible
mutations, and worktree management — live in `_gremlins_core.utils.git`.

The evasive `py.import("gremlins.utils.git")` at `crates/pyext/src/python/stages.rs:3904`
is gone. Executor code imports from `_gremlins_core.utils.git` directly for
git operations and from `gremlins.utils.git` only for the two filesystem helpers.

Key implementation notes:

- The Rust module shells out to the system `git` CLI via `crates/gremlins/src/core/proc.rs`,
  inheriting timeout and process-group semantics for free.
- Three return conventions mirror the Python API: predicates return `bool` and
  never raise, best-effort readers return `String` (empty on failure), fallible
  operations return `Result<T, GitError>`.
- Async variants (`setup_detached_worktree_async`, `remove_worktrees_async`, etc.)
  are real coroutine functions wrapped via `on_runtime` to bridge asyncio ↔ Tokio.
- Worktree paths are minted from `/dev/urandom` (or a time/pid fallback) and
  placed under `work_root()` by default.

### 2. `gremlin.env_file` — DONE

Fully ported to Rust at `crates/gremlins/src/core/env_file.rs` (277 lines) with
PyO3 bindings at `crates/pyext/src/python/utils/env_file.rs` (64 lines).
The Python file `gremlins/env_file.py` has been removed. `run.py` imports
`source_env_string` and `load_env_file_isolated` from
`_gremlins_core.utils.env_file` directly.

Implementation notes:

- Both functions shell out to `bash -c 'source "$1" >/dev/null && env -0'` via
  `proc::run_shell_async` (already ported), parse null-delimited output, and
  strip bash internals (`_`, `BASH*`, `PPID`, `SHLVL`, `SHELLOPTS`).
- `load_env_file_isolated(path, base_env, cwd)` sources a file; `source_env_string`
  writes the script to a temp file first, sources it, then unlinks.
- Sourcing shells out, so each call releases the GIL via `py.detach` for as
  long as `bash` runs.
- Every failure surfaces as a `RuntimeError`, matching the old Python module.

### 3. `gremlins.utils.yaml_io` — 0% done, ~67 lines, trivial

Wrappers over `serde_yaml`:
- `load_yaml_file` → `serde_yaml` parse + `YamlLoadError` shaping
- `dump_yaml_text` → `serde_yaml` serialize (no sort, block style)
- `load_bundled_prompt` / `render_bundled_prompt` → already delegate to
  `_gremlins_core.assets` (Rust); `render_bundled_prompt` adds `.format(**kwargs)`.

`serde_yaml` is already in `Cargo.toml`. Used by `gremlin.py` for writing branch
pipeline YAML and loading pipelines.

## Non-utility blockers

### 4. OS integration (for `run.py`)

| Concern | Python | Rust equivalent | Status |
|---------|--------|-----------------|--------|
| Signal handling (INT/TERM/HUP/QUIT) | `signal.signal` | `tokio::signal` | needs `signal` feature (not enabled) |
| Cleanup on exit | `atexit.register` | `Drop` / RAII | — |
| Argument parsing | `argparse` | `clap` | not in `Cargo.toml` |
| Python logging | `logging` stdlib | already `log`/`tracing` | — |

`tokio::signal` requires the `signal` feature (Cargo.toml currently enables
`process`, `time`, `rt`, `sync`, `io-util`, `macros`). `clap` would be a new
dependency. Both straightforward.

### 5. `StageProtocol` interface boundary

`Gremlin` iterates over `StageProtocol` objects — a Python `typing.Protocol`.
All stage types are already `#[pyclass]` in Rust and registered under
`_gremlins_core.stages` (the `gremlins/stages/` Python directory is empty). The
Rust Gremlin would either:

- **Keep stages as `Py<PyAny>`** and do duck-typed `.getattr("name")?`,
  `.getattr("client")?`, `.getattr("body")?` through PyO3 — simplest,
  no new trait hierarchy.
- **Define a Rust trait** (`GremlinStage`) and implement it for each stage
  struct — cleaner but more work, ties the stage crate to the executor.

Given all stage types already live in Rust, the duck-typed approach is
lower-effort and matches how the Python code already works (it accesses `.name`,
`.type`, `.client`, `.path`, `.gremlin`, `.body` as plain attributes with no
static type checking at runtime). This is unchanged from the original plan.

## What's NOT a blocker

Already in Rust and not needing porting:

- `_gremlins_core.artifacts.ArtifactRegistry` — pyclass in `crates/pyext/src/python/artifacts.rs`
- `_gremlins_core.clients.Client` — pyclass + `Client::parse` in Rust
- `_gremlins_core.config.project_root / scratch_root / state_root` — `gremlins::config`
- `_gremlins_core.discovery.resolve_pipeline_path` — pyclass in `crates/pyext/src/python/discovery.rs`
- `_gremlins_core.executor.State / StateData / build_state` — `crates/gremlins/src/executor/state.rs`
- `_gremlins_core.schemas.Pipeline / Bootstrap` — pyclasses in `crates/pyext/src/schemas/`
- `_gremlins_core.utils.env_file` — `load_env_file_isolated` and `source_env_string`
  in `crates/gremlins/src/core/env_file.rs` (277 lines) with PyO3 bindings at
  `crates/pyext/src/python/utils/env_file.rs` (64 lines).
- `_gremlins_core.utils.proc.*` — the full `proc` suite, including
  `spawn_with_pumps`, `pump_prefixed`, `wait_child_proc`,
  `terminate_with_grace`, `terminate_with_grace_blocking`, in
  `crates/gremlins/src/core/proc.rs` (~2,350 lines). `gremlins/utils/proc.py`
  is now a thin decoding wrapper over those pyfunctions.

## Work estimate

| Item | Effort | Notes |
|------|--------|-------|
| `git` module | — | **Done** ([#1540](https://github.com/xbrianh/gremlins/pull/1540)). ~595 lines of Rust + PyO3 bindings; Python wrapper is 64 lines |
| `env_file` port | — | **Done**. ~277 lines of Rust + PyO3 bindings (64 lines); Python file removed |
| `yaml_io` port | ~1h | Thin serde_yaml wrappers |
| **Subtotal utilities** | **~1h** | |
| OS integration (signals, atexit, clap) | ~2–3h | tokio::signal, clap setup |
| `StageProtocol` bridge | ~1h | Duck-typed PyO3 accessors |
| **Total remaining preliminaries** | **~4–5h** | Under 1 day |

The `proc` port, `git` port, and `env_file` port are already done.

## Removing evasive imports along the way

`evasive_port_problems.md` no longer exists — the `proc`, `git`, and stdlib
(`argparse`, `asyncio`, `subprocess`, …) evasive imports were eliminated in
[#1531](https://github.com/xbrianh/gremlins/pull/1531) and
[#1540](https://github.com/xbrianh/gremlins/pull/1540). The
remaining gremlins-specific evasive import in `crates/pyext/src/` is:

| Current | Replacement |
|---------|-------------|
| `py.import("_gremlins_core.schemas")` ×1 (stages.rs:2683) | Port `build_branch_pipeline` to use native constructors |

The other `py.import(...)` calls that remain (`builtins`, `sys`, `copy`,
`asyncio`) are Python stdlib access, not gremlins-module evasions.