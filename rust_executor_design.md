# Rust executor design

## Overview

The executor is the runtime that loads a pipeline YAML, wires up stages and
clients, and drives a gremlin through its lifecycle. Today it lives in Python
(`gremlins/executor/`). The goal is a single Rust `Gremlin` struct that is the
entire runtime — construct it, call `.run().await`, done.

Multiple Gremlins share one tokio runtime. Parallel stages become tokio tasks
instead of subprocesses. No more fork+exec ceremony.

Each `Gremlin` owns its resolved environment as a plain `HashMap<String, String>`.
There is no process-global env mutation — `std::env::var()` is never called
directly by the executor. Every env read (API keys, client config, subprocess
envs for `Exec` / bootstrap commands) goes through `gremlin.env`.

## Architecture

```
┌─────────────────────────────────────────────────────┐
│ CLI binary (clap)                                   │
│  gremlins run <pipeline> --resume-from <stage>      │
└───────────────┬────────────────────────────────────┘
                │ argv
                ▼
┌───────────────────────────────────────────────────────┐
│ Gremlin::open(id)  or  Gremlin::launch(spec)        │
│                                                      │
│  load_pipeline(path) → Pipeline { stages, bootstrap } │
│  resolve_clients(stages, override)                   │
│  setup_worktree(base_ref) → PathBuf                  │
│  State::build(...)                                   │
│  ArtifactRegistry::new(artifact_dir)                  │
│  register_artifacts(stage_inputs)                     │
└───────────┬────────────────────────────────────────┘
            │
            ▼
┌───────────────────────────────┐
│ gremlin.run().await  →  i32   │
│                                │
│  for each stage:               │
│    state.set_stage(name, ...)  │
│    runner = stage.make_runner()│
│    runner().await              │
│    check_bail()                │
│                                │
│  write_terminal_state(code)    │
└───────────────────────────────┘
```

The CLI binary handles arg parsing, signal wiring, and log setup. `Gremlin`
handles everything else.

## The `Gremlin` struct

```rust
pub struct Gremlin {
    id: String,
    state_dir: PathBuf,
    artifact_dir: PathBuf,
    pipeline: Pipeline,           // from _gremlins_core.schemas
    stages: Vec<Py<PyAny>>,       // duck-typed stage objects (pyclasses)
    registry: ArtifactRegistry,   // from _gremlins_core.artifacts
    worktree: Option<PathBuf>,
    project_root: PathBuf,
    base_ref_sha: String,
    base_ref: String,
    resume_from: Option<String>,
    state: State,                 // from _gremlins_core.executor
    env: HashMap<String, String>,  // resolved per-gremlin environment
}
```

### Construction

One constructor for each entry path:

| Constructor | Use |
|-------------|-----|
| `Gremlin::launch(spec)` | Fresh gremlin from pipeline YAML |
| `Gremlin::open(id)` | Reconstruct from persisted state directory |
| `Gremlin::fork(&self, target_id)` | Child gremlin for parallel stage fan-out |

`launch` does the full init sequence:
1. Resolve pipeline path, load YAML → `Pipeline`
2. Apply client overrides
3. Set up state dir, artifact dir
4. Resolve environment (see [Environment](#environment))
5. Create git worktree (if `worktree_dir` not pre-set)
6. Register artifact inputs (stage_inputs)
7. Write `base_sha` artifact (current HEAD)
8. Build `State`
9. Run bootstrap commands in worktree

`open` reverses this from `state.json`:
1. Read `state.json` → `StateData`
2. Resolve pipeline path (hermetic copy first, then discovery)
3. Load pipeline
4. Rebuild `ArtifactRegistry`
5. Return `Gremlin` ready for `.run()`

`fork` creates an independent child from a running parent:
1. Copy artifact directory
2. Optionally create new worktree at parent's HEAD
3. Inherit parent's `env` (child may override via its own `bootstrap.env`)
4. Build child `State` from parent fields (minus transient keys)
5. Persist child `state.json`
6. Return child `State` (caller wraps in `Gremlin`)

### Execution

```rust
impl Gremlin {
    pub async fn run(&mut self) -> Result<i32> {
        let mut exit_code = 0;

        for (i, stage) in self.stages.iter().enumerate() {
            if self.resume_from.as_ref().is_some_and(|r| i < skip_until) {
                continue;
            }

            let name = stage_name(&stage);
            self.state.set_stage(&name, None, "");

            let runner = make_runner(&stage, &self);
            match runner().await {
                Ok(()) => {}
                Err(Bail { reason }) => {
                    self.state.data.write_bail_file("other", &reason);
                    exit_code = 1;
                    break;
                }
                Err(e) => {
                    self.state.data.write_bail_file(
                        "other",
                        &format!("unexpected: {e}"),
                    );
                    return Err(e);
                }
            }

            if bailed(&self.state) {
                break;
            }
        }

        self.state.write_terminal_state(exit_code);
        Ok(exit_code)
    }
}
```

Resume is just a skip over already-completed stages. The resume_from index is
validated at construction time.

### Environment

Every `Gremlin` carries a resolved `env: HashMap<String, String>`. This is
the single source of truth for environment variables — `std::env::var()` is
never called by the executor or its stages. Client API-key resolution, `Exec`
stage commands, and bootstrap subprocesses all read from `gremlin.env`.

Env is resolved once at construction time:

1. Start with the parent process env (`std::env::vars()`) as the base.
2. Add system vars (`GREMLINS_GREMLIN_ID`, `GREMLINS_PROJECT_ROOT`,
   `GREMLINS_WORKTREE_PATH`, etc.) — these go in last so users cannot
   override them.
3. If the pipeline defines `bootstrap.env`, source it via bash in a
   subprocess (same as today's `source_env_string`), passing the base
   env from steps 1–2. The sourced output *replaces* the base; system
   vars are re-injected after sourcing so they are always present.
4. Store the result in `gremlin.env`.

`open` re-reads `state.json` but cannot recover the original env (it was
never persisted in the state file — only the hermetically-copied pipeline.yaml
has the `bootstrap.env` script). It re-runs the same resolution: process env
base + system vars + `bootstrap.env` sourcing.

`fork` inherits the parent's `env` outright. If the child has its own
`bootstrap.env` (e.g. a parallel child from a different pipeline), it
re-runs env resolution starting from the parent's `env` as the base, so
diverging children get independent env maps.

### Bootstrap

Runs after worktree creation, before stages:

1.  `bootstrap.cmds` — join with `&&`, run via `proc::run_shell_async` in worktree
2.  `bootstrap.launch_cmds` — process each entry:
    - Lines starting with `gremlins:` → parse as DSL, execute inline
    - Everything else → shell command (after `{var}` substitution)
3.  `bootstrap.cli_out` → run an inline `Exec` stage capturing stdout

DSL commands are registered in a dispatch table:

```rust
type DslHandler = fn(&Gremlin, &[String], &HashMap<String, String>) -> Pin<Box<dyn Future<Output = Result<()>>>>;

const DSL_DISPATCH: phf::Map<&'static str, DslHandler> = phf::phf_map! {
    "bind_artifact" => dsl_bind_artifact,
};
```

Only `bind_artifact` exists today. Adding a new DSL command means adding a
function and an entry in the map.

## Stage interface

Stages are Python objects (`#[pyclass]`) registered under
`_gremlins_core.stages`. The Rust `Gremlin` accesses them through duck-typed
PyO3 calls:

```rust
fn stage_name(s: &Py<PyAny>) -> String {
    Python::with_gil(|py| {
        s.bind(py).getattr("name")?.extract::<String>()
    }).unwrap_or_default()
}

fn stage_type(s: &Py<PyAny>) -> String { /* ... */ }
fn stage_body(s: &Py<PyAny>) -> Vec<Py<PyAny>> { /* ... */ }
fn stage_client(s: &Py<PyAny>) -> Option<Py<PyAny>> { /* ... */ }
fn stage_make_runner(
    s: &Py<PyAny>,
    gremlin: &Gremlin,
    scope: &[Py<PyAny>],
) -> Pin<Box<dyn Future<Output = PyResult<()>>>> { /* ... */ }
```

This matches what the Python `Gremlin` already does — it accesses `.name`,
`.type`, `.client`, `.body`, `.gremlin` as plain attributes with no static
checking. No new trait hierarchy needed. When stages are eventually ported to
pure Rust structs, swap these accessors for a trait.

## Parallel execution

Today: `ParallelStage` forks a subprocess per child, each subprocess runs
`gremlins.spawn.child` which constructs a `Gremlin.from_subprocess()` and calls
`.run()`. This gives process isolation but costs fork+exec, separate Python
interpreters, and inter-process coordination via `state.json` polling.

In Rust: `ParallelStage` spawns a tokio task per child:

```rust
async fn run_parallel(stage: &Py<PyAny>, gremlin: &Gremlin) -> Result<()> {
    let children = stage_body(stage);
    let mut join_set = tokio::task::JoinSet::new();

    for child in &children {
        let child_id = format!("{}/{}", gremlin.id, stage_name(child));
        let mut child_gremlin = gremlin.fork(&child_id, child).await?;

        join_set.spawn(async move {
            child_gremlin.run().await
        });
    }

    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(Ok(0)) => {}  // child succeeded
            Ok(Ok(n)) | Ok(Err(_)) => {
                // First child to fail writes bail; others drain
                cancel_remaining(&mut join_set).await;
                return Ok(());
            }
            Err(join_err) => { /* panic in task */ }
        }
    }

    Ok(())
}
```

Key points:
- Each child gets its own `Gremlin` via `fork()`, with its own `State`,
  artifact dir, and optionally worktree
- Children run concurrently on the tokio runtime
- First failure triggers cancellation of siblings (tokio's `JoinSet.abort_all()`)
- No `state.json` polling — results are collected directly from task handles
- No subprocess cost — memory overhead is just the child `Gremlin` structs

If process isolation is still desired for some pipelines, parallel can be
configured to spawn subprocesses via a `#[pyclass]` flag. But the default
should be in-process tokio tasks.

### Worktree strategy

Parallel children that need independent worktrees:
- `fork()` optionally creates a new detached worktree at the parent's HEAD
- The child `Gremlin` gets `worktree = Some(path)`
- Cleanup happens in `Drop` or when the parallel group drains

For children that share the parent worktree (e.g. read-only stages like
review), `fork()` skips worktree creation.

## State management

`State` and `StateData` are already in Rust (`crates/gremlins/src/executor/state.rs`).
The `Gremlin` holds a `State` and calls its methods directly:

| During | Call |
|--------|------|
| Stage start | `state.data.set_stage(name, sub, "")` |
| Stage done | (implicit — next `set_stage` call) |
| Bail | `state.data.write_bail_file(class, detail)` |
| Token usage | `state.data.accumulate_token_usage(usage)` |
| Parallel worktree | `state.data.patch_parallel_worktrees(...)` |
| Parallel attempts | `state.data.patch_parallel_attempt(...)` |
| Child done | `state.data.mark_done(path, name)` |
| Terminate | `state.data.write_terminal_state(code)` |
| Cost | `state.data.add_subprocess_cost(amount)` |

## Error handling

```rust
enum RunError {
    Bail { reason: String },
    BootstrapFailed { exit_code: i32, stderr: String },
    StageFailed { stage: String, source: PyErr },
    Git { source: git2::Error },
    Io { source: std::io::Error },
}
```

- **Bail**: stage requested a stop. Write bail file, return exit code 1.
- **BootstrapFailed**: shell commands in bootstrap.cmds failed. Write bail, return 1.
- **StageFailed**: unexpected error from a stage's async runner. Write bail, return.
- **Git/Io**: infrastructure failures. Propagate up.

## What goes away

From `run.py`:
- `_parse_args` → clap in CLI binary
- `_install_signal_handlers` → `tokio::signal` in CLI binary
- `_read_state_json` → `StateData` handles this
- `env isolation block` → per-`Gremlin` env map, no global `os.environ` mutation
- `_prepend_overlay_bin_to_path` → overlay bin/ handling stays, moved into bootstrap
- `_unique_clients` → small helper in `Gremlin`

From `gremlin.py`:
- `write_initial_state` / `write_terminal_state` → `StateData` already has this
- `run_stages` → inline in `Gremlin::run`
- `_get_stage_types` → `STAGE_TYPES` constant in `loader.rs` already
- `validate_gremlin_id` → move to a `GremlinId` newtype
- `_apply_client_override` → small helper
- `_expand_stage_entries` → small helper

From `parallel_state.py`:
- `ParallelGroupState` → already in Rust `StateData` methods;
  the Python class is a thin wrapper, remove it

## Files after port

```
crates/
  gremlins/src/
    executor/
      mod.rs
      state.rs          ← already exists (StateData + state.json I/O)
      gremlin.rs        ← NEW: Gremlin struct, launch, open, fork
      run.rs             ← NEW: Gremlin::run, stage iteration, bail handling
      bootstrap.rs       ← NEW: bootstrap command execution, DSL dispatch
      parallel.rs        ← NEW: parallel fan-out via tokio tasks
    stages/              ← stages call back into executor via Gremlin reference
  pyext/src/
    python/
      executor.rs        ← updated: PyO3 wrappers shrink (State mostly;
                             Gremlin methods exposed for Python stage access)
```

The Python `gremlins/executor/` becomes a thin compatibility layer for any
remaining Python callers, or is removed entirely once all call sites
(CLI, tests) use the Rust API directly.

## Migration path

1.  **Preliminaries** (this repo's `executor_preliminares.md`):
    finish `proc`, add `git2`, port `env_file`, port `yaml_io`

2.  **`Gremlin` struct**: implement `launch`, `open`, `fork` in Rust.
    Run existing Python stages through PyO3. `run_pipeline()` in `run.py`
    becomes a thin wrapper that constructs Rust `Gremlin` and calls `.run()`.

3.  **Tokio-native parallel**: swap `ParallelStage` subprocess spawn for
    tokio task spawn. Remove `gremlins/executor/parallel_state.py`.

4.  **Inline the rest**: move signal handling, arg parsing, and log setup
    to the CLI binary. Remove `run.py`.

5.  **Stage trait** (future): once all stages are pure Rust, swap duck-typed
    PyO3 accessors for a `Stage` trait.