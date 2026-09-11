# gremlins — top-level AGENTS

Background orchestration for Claude Code: a gremlin is a detached process that runs a YAML-defined pipeline (plan → implement → review → address → open-PR …) against a goal or GitHub issue, writing artifacts to a per-user state directory.

Gremlins is an **unopinionated agentic workflow language**: the pipeline YAML is the program, and the harness is only its runtime. The harness injects no system prompt, preamble, or operational norms of its own — a stage's model sees exactly the prompts the pipeline declares plus the artifacts passed through. Behavioral opinions (what to re-check, how to communicate, when to bail) belong in the pipeline's own prompt files, never in harness code or a bundled default.

This file is the entry-point orientation for an agent working on this codebase. Per-subpackage detail lives in `gremlins/<pkg>/AGENTS.md`. The user-facing project doc is `README.md`. Design notes live in `DESIGN.md` and `plans/`.

## Repository layout

```
gremlins/                    Python package — see gremlins/AGENTS.md
  __init__.py                PACKAGE_ROOT only
  __main__.py                Entry: from gremlins.cli import main; raise SystemExit(main())
  launcher.py                Sets up state dir, spawns pipeline subprocess
  errors.py                  die(msg) helper
  paths.py                   Single source of truth for filesystem locations (state dir, worktree, etc.)
  logging_setup.py           configure_logging — UTC timestamp formatter, stdout, GREMLINS_LOG_LEVEL
  env_file.py                .env file loading (shell-like parsing)
  protocols.py               GremlinProtocol, StageProtocol — shared protocols to avoid circular imports
  cli/                       Subcommand entry points — one file per subcommand group
  clients/                   Client classes + provider impls — see gremlins/clients/AGENTS.md
  stages/                    Stage classes: agent, exec, loop, composite, parallel, sequence — see gremlins/stages/AGENTS.md
  pipeline/                  YAML loader, discovery, preprocessor, bootstrap
  pipelines/                 Bundled YAML pipelines (gh, gh-terse, local, boss, pr-extend)
  prompts/                   Bundled prompt templates
  executor/                  Run-time orchestrator — see gremlins/executor/AGENTS.md
    state.py                 State class: execution context + state.json I/O
    run.py                   run_pipeline: unified pipeline entry point
    gremlin.py               Gremlin: constructs, initializes, and runs a pipeline
    parallel_state.py        Per-shard state bookkeeping for parallel stages
  fleet/                     Fleet manager — see gremlins/fleet/AGENTS.md
  artifacts/                 Artifact registry + URI model — see gremlins/artifacts/AGENTS.md
  spawn/                     Internal spawn boundaries (pipeline + child subprocess entry points)
  queue/                     Sequential gremlin dispatch queue
  recipes/                   Reusable stage recipes (shell stages for YAML cmds:)
  utils/                     proc, git, text, yaml_io, state_file, parallel_bail helpers
Cargo.toml                   Rust workspace root
crates/                      Rust crates
  gremlins-core/             PyO3 native extension (maturin)
    src/lib.rs               #[pymodule] _gremlins_core
    src/core/                Pure Rust logic (future ports)
    src/python/              PyO3 glue (future ports)
    pyproject.toml           maturin build backend
.gremlins/                   Project-overlay pipeline YAMLs (project-scoped, win over bundled)
plans/                       Design notes, in-flight plan documents, per-feature sketches
tests/                       Pytest suite (testpaths = ["tests"])
DESIGN.md                    System design
README.md                    Dev install + CLI usage
```

## Dev workflow

```sh
uv venv && source .venv/bin/activate
uv pip install -e ".[dev]"
make install          # build + install the Rust native extension (maturin develop)
make -j8 test      # runs pytest per-file in parallel (Makefile splits the suite)
make check         # ruff lint + ruff format check + pyright + clippy + rustfmt
```

**Always run tests with `make -j8 test`** (or `make -j$(sysctl -n hw.ncpu) test` / `make -j$(nproc) test`). The Makefile depends on each `tests/test_*.py` as its own sub-target, so `-jN` parallelizes cleanly and the suite finishes several times faster. Serial `make test` is leaving time on the floor — don't do it. Never use bare `-j` (means *unlimited*, spawns one pytest per file simultaneously, bad).

**Never `uv run pytest`** — the project venv is the test target, not whatever `uv run` resolves. Bare `pytest` is fine for a single file; `make -j8 test` is the way to run the whole suite.

The `Makefile` sets `MAKEFLAGS += -j$(shell sysctl -n hw.ncpu 2>/dev/null || nproc)` automatically, so `make test` is already parallel without explicit `-j`. Passing `-j8` or `-j$(nproc)` still works as an override.

`make check` now includes Rust checks (clippy + rustfmt) alongside the Python checks.

## Project-wide conventions

- **Unopinionated workflow language.** The harness supplies mechanics (sequencing, worktrees, artifacts, bail bookkeeping, client plumbing) and injects no system prompt or operational norms into any model. A stage's model sees only the prompts its pipeline declares. Do not add hard-coded system prompting or behavioral instructions to the harness — put them in the pipeline's own prompt files, where the pipeline author owns them.
- **No re-export facades.** Package `__init__.py` files do not import from submodules and re-publish via `__all__`. Imports name the defining submodule directly: `from gremlins.cli.fleet import fleet_main`, not `from gremlins.cli import fleet_main`. The sole exceptions are `__init__.py` files that *define* something (e.g. `gremlins/clients/__init__.py` runs provider registrations on import; `gremlins/__init__.py` defines `PACKAGE_ROOT`).
- **No backwards-compatibility shims.** No legacy aliases, no deprecation paths, no compat decorators. Replace at every call site.
- **No inheritance.** Composition only. Single inheritance is almost always the wrong tool; multiple inheritance is never acceptable.
- **Functional first.** Pure functions and plain data over classes. Reach for a class only when state must be kept.
- **Short functions.** If it doesn't fit on a screen, split it.
- **Few comments.** Names carry meaning. Comment only when *why* is non-obvious.
- **Worktree invariant (in-progress, see #395):** gremlin worktrees should operate on detached HEAD throughout the run; commits accumulate on detached HEAD; the PR-opening primitive pushes a remote branch directly. Existing code is mid-retrofit.

## Byte-stable strings — DO NOT change

These values are persisted to `state.json` and read by other writers (the fleet manager, the launcher). Renaming any of them silently breaks cross-process consumers.

- **Bail classes** (`state.json.bail_class`): `reviewer_requested_changes`, `security`, `secrets`, `other`. These are convention tokens parsed by the agent stage from the `BAIL: <class>: <detail>` marker (`crates/gremlins/src/stages/agent.rs`); no Python constant enumerates them.
- **Stage names** (`state.json.stage`): defined per-pipeline in YAML. The authoritative list for a pipeline is its YAML file under `gremlins/pipelines/` or `.gremlins/`.

## Where to look for…

| You want to … | Look at |
|---|---|
| Understand the run-time architecture | `DESIGN.md` |
| Add a new stage | `gremlins/stages/AGENTS.md` and an existing stage as a model |
| Add a new client provider | `gremlins/clients/AGENTS.md` |
| Add a new pipeline | YAMLs in `gremlins/pipelines/` (bundled) or `.gremlins/` (project) |
| Trace a CLI subcommand | `gremlins/cli/` |
| Understand fleet operations | `gremlins/fleet/AGENTS.md` |
| Understand the executor | `gremlins/executor/AGENTS.md` |
| Understand artifact registry + URIs | `gremlins/artifacts/AGENTS.md` |
| Investigate a state-dir layout | `gremlins/paths.py` resolves dirs; `gremlins/executor/state.py` manages state.json. Per-gremlin layout under `platformdirs.user_state_dir("gremlins")/<gremlin_id>/` |
| Find the design backlog | `plans/` (rough notes, not authoritative) |
| Find open work | GitHub issues, `gh issue list --repo xbrianh/gremlins` |

## Testing seam: clients

Stages that invoke `claude` go through an injected `Client` (in `gremlins/clients/client.py`). Production passes `Client.parse("xai:grok-4")` (per the pipeline's `default_client`); tests pass `FakeClient(fixtures={label: <jsonl-or-list>})` from `tests/fake_client.py`, which records each `run(...)` call into `self.calls` for assertion. **Never have a stage spawn `claude -p` directly** — go through the injected client so tests can intercept.

`FakeClient` looks fixtures up by `label`. Stages that re-enter the same logical step within one process (e.g. resumed implement) must use distinct labels per phase.

## State and bail bookkeeping

`State.set_stage` (in `executor/state.py`) writes stage info to `state.json` atomically via `State.patch` (which uses `locked_update`).
`State.write_bail_file` writes `bail_{attempt}.json` to the state dir. When a stage
detects a recorded bail (via `state.json`), it raises a `Bail` exception.
Both helpers no-op without `GREMLINS_GREMLIN_ID` and never raise —
stage / bail bookkeeping must not crash a running gremlin.
