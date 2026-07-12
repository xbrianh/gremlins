# gremlins — top-level AGENTS

Background orchestration for Claude Code: a gremlin is a detached process that runs a YAML-defined pipeline (plan → implement → review → address → open-PR …) against a goal or GitHub issue, writing artifacts to a per-user state directory.

This file is the entry-point orientation for an agent working on this codebase. Per-subpackage detail lives in `gremlins/<pkg>/AGENTS.md`. The user-facing project doc is `README.md`. Design notes live in `DESIGN.md` and `plans/`.

## Session start

At the beginning of every session, run `gremlins prompt-for-assistant` and follow its instructions — it defines the four-role collaboration model (thought partner / scribe / flight controller / correction loop) and queue conventions for working through gremlins. Captured work on this project goes to **GitHub issues** (`gh issue ...` against `xbrianh/gremlins`).

## Repository layout

```
gremlins/                    Python package — see gremlins/AGENTS.md
  cli/                       Subcommand entry points — one file per subcommand group
  launcher.py                Sets up state dir + worktree, spawns pipeline
  run_pipeline.py            Subprocess entry that wraps cli.main with terminal-state bookkeeping
  state.py                   state.json read/write helpers, bail/stage bookkeeping
  schema.py                  StageEntry, PipelineDef dataclasses
  stage_clients.py           Pipeline → client wiring (collect_stage_specs etc.)
  clients/                   Client classes + provider impls — see gremlins/clients/AGENTS.md
  stages/                    Per-stage bodies — see gremlins/stages/AGENTS.md
  pipelines/                 Bundled YAML pipelines (gh, local, boss)
  pipeline/                  YAML loader + discovery + schema
  prompts/                   Bundled prompt templates
  executor/                  State class + pipeline.py (StageRunner), run.py
  fleet/                     Fleet manager (status, stop, land, close, log) — see gremlins/fleet/AGENTS.md
  utils/                     proc helpers etc.
  _core.py                   Shim: from _gremlins_core import *
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
make dev           # build + install the Rust native extension (maturin develop)
make -j8 test      # runs pytest per-file in parallel (Makefile splits the suite)
make check         # ruff lint + ruff format check + pyright + clippy + rustfmt
```

**Always run tests with `make -j8 test`** (or `make -j$(sysctl -n hw.ncpu) test` / `make -j$(nproc) test`). The Makefile depends on each `tests/test_*.py` as its own sub-target, so `-jN` parallelizes cleanly and the suite finishes several times faster. Serial `make test` is leaving time on the floor — don't do it. Never use bare `-j` (means *unlimited*, spawns one pytest per file simultaneously, bad).

**Never `uv run pytest`** — the project venv is the test target, not whatever `uv run` resolves. Bare `pytest` is fine for a single file; `make -j8 test` is the way to run the whole suite.

The `Makefile` sets `MAKEFLAGS += -j$(shell sysctl -n hw.ncpu 2>/dev/null || nproc)` automatically, so `make test` is already parallel without explicit `-j`. Passing `-j8` or `-j$(nproc)` still works as an override.

`make check` now includes Rust checks (clippy + rustfmt) alongside the Python checks.

`make dev` is an alias for `cd crates/gremlins-core && maturin develop`.

## Project-wide conventions

- **No re-export facades.** Package `__init__.py` files do not import from submodules and re-publish via `__all__`. Imports name the defining submodule directly: `from gremlins.cli.fleet import fleet_main`, not `from gremlins.cli import fleet_main`. The sole exceptions are `__init__.py` files that *define* something (e.g. `gremlins/clients/__init__.py` runs provider registrations on import; `gremlins/__init__.py` defines `PACKAGE_ROOT`).
- **No backwards-compatibility shims.** No legacy aliases, no deprecation paths, no compat decorators. Replace at every call site.
- **No inheritance.** Composition only. Single inheritance is almost always the wrong tool; multiple inheritance is never acceptable.
- **Functional first.** Pure functions and plain data over classes. Reach for a class only when state must be kept.
- **Short functions.** If it doesn't fit on a screen, split it.
- **Few comments.** Names carry meaning. Comment only when *why* is non-obvious.
- **Worktree invariant (in-progress, see #395):** gremlin worktrees should operate on detached HEAD throughout the run; commits accumulate on detached HEAD; the PR-opening primitive pushes a remote branch directly. Existing code is mid-retrofit.

## Byte-stable strings — DO NOT change

These values are persisted to `state.json` and read by other writers (the fleet manager, the launcher). Renaming any of them silently breaks cross-process consumers.

- **Bail classes** (`state.json.bail_class`): `reviewer_requested_changes`, `security`, `secrets`, `other`. Source of truth in `gremlins/state.py`.
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
| Investigate a state-dir layout | `gremlins/state.py` resolves dirs; per-gremlin layout under `platformdirs.user_state_dir("gremlins")/<gremlin_id>/` |
| Find the design backlog | `plans/` (rough notes, not authoritative) |
| Find open work | GitHub issues, `gh issue list --repo xbrianh/gremlins` |

## Testing seam: clients

Stages that invoke `claude` go through an injected `Client` (in `gremlins/clients/client.py`). Production passes `Client("claude", "sonnet")`; tests pass `FakeClaudeClient(fixtures={label: <jsonl-or-list>})` from `gremlins/clients/fake.py`, which records each `run(...)` call into `self.calls` for assertion. **Never have a stage spawn `claude -p` directly** — go through the injected client so tests can intercept.

`FakeClaudeClient` looks fixtures up by `label`. Stages that re-enter the same logical step within one process (e.g. resumed implement) must use distinct labels per phase.

## State and bail bookkeeping

`state.set_stage` and `state.emit_bail` write to `state.json` atomically in pure Python via `patch_state`. Both helpers no-op without `GREMLINS_GREMLIN_ID` and never raise — stage / bail bookkeeping must not crash a running gremlin.
