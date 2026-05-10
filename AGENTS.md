# gremlins — top-level AGENTS

Background orchestration for Claude Code: a gremlin is a detached process that runs a YAML-defined pipeline (plan → implement → review → address → open-PR …) against a goal or GitHub issue, writing artifacts to a per-user state directory.

This file is the entry-point orientation for an agent working on this codebase. Per-subpackage detail lives in `gremlins/<pkg>/AGENTS.md`. The user-facing project doc is `README.md`. Design notes live in `DESIGN.md` and `plans/`.

## Repository layout

```
gremlins/                    Python package — see gremlins/AGENTS.md
  cli.py                     Subcommand dispatch
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
  orchestrators/             pipeline.py (StageRunner), run.py, review_address.py
  fleet/                     Fleet manager (status, stop, rescue, land, close, log) — see gremlins/fleet/AGENTS.md
  utils/                     proc helpers etc.
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
make test          # runs pytest per-file (Makefile splits the suite)
make check         # ruff lint + ruff format check + pyright
```

**Run tests with `make test` or bare `pytest`. Never `uv run pytest`** — the project venv is the test target, not whatever `uv run` resolves.

`make test` depends on each `tests/test_*.py` as its own sub-target, so it parallelizes cleanly with `-jN` — e.g. `make -j8 test`. `make` does not auto-detect CPU count (bare `-j` means *unlimited*, which spawns one pytest per file simultaneously and is bad). Portable auto-cap on this machine: `make -j$(sysctl -n hw.ncpu) test` on macOS, `make -j$(nproc) test` on Linux.

## Project-wide conventions

- **No re-export facades.** Package `__init__.py` files do not import from submodules and re-publish via `__all__`. Imports name the defining submodule directly: `from gremlins.fleet.cli import main`, not `from gremlins.fleet import main`. The sole exceptions are `__init__.py` files that *define* something (e.g. `gremlins/clients/__init__.py` runs provider registrations on import; `gremlins/__init__.py` defines `PACKAGE_ROOT`).
- **No backwards-compatibility shims.** No legacy aliases, no deprecation paths, no compat decorators. Replace at every call site.
- **No inheritance.** Composition only. Single inheritance is almost always the wrong tool; multiple inheritance is never acceptable.
- **Functional first.** Pure functions and plain data over classes. Reach for a class only when state must be kept.
- **Short functions.** If it doesn't fit on a screen, split it.
- **Few comments.** Names carry meaning. Comment only when *why* is non-obvious.
- **Worktree invariant (in-progress, see #395):** gremlin worktrees should operate on detached HEAD throughout the run; commits accumulate on detached HEAD; the PR-opening primitive pushes a remote branch directly. Existing code is mid-retrofit.

## Byte-stable strings — DO NOT change

These values are persisted to `state.json` and read by other writers (the fleet manager, the launcher, the rescue protocol). Renaming any of them silently breaks cross-process consumers.

- **Bail classes** (`state.json.bail_class`): `reviewer_requested_changes`, `security`, `secrets`, `other`. Source of truth in `gremlins/state.py`.
- **Stage names** (`state.json.stage`): defined per-pipeline in YAML. The authoritative list for a pipeline is its YAML file under `gremlins/pipelines/` or `.gremlins/`.
- **Marker-protocol bail reasons** (used by handoff/rescue): `diagnosis_no_marker`, `diagnosis_bad_marker`, `diagnosis_claude_error`, `diagnosis_timeout`, `excluded_class:<class>`, `attempts_exhausted`, `relaunch_launcher_missing`, `relaunch_failed`.

## Where to look for…

| You want to … | Look at |
|---|---|
| Understand the run-time architecture | `DESIGN.md` |
| Add a new stage | `gremlins/stages/AGENTS.md` and an existing stage as a model |
| Add a new client provider | `gremlins/clients/AGENTS.md` |
| Add a new pipeline | YAMLs in `gremlins/pipelines/` (bundled) or `.gremlins/` (project) |
| Trace a CLI subcommand | `gremlins/cli.py` dispatch table |
| Understand fleet operations | `gremlins/fleet/AGENTS.md` |
| Investigate a state-dir layout | `gremlins/state.py` resolves dirs; per-gremlin layout under `platformdirs.user_state_dir("claude-gremlins")/<gr_id>/` |
| Find the design backlog | `plans/` (rough notes, not authoritative) |
| Find open work | GitHub issues, `gh issue list --repo xbrianh/gremlins` |

## Testing seam: clients

Stages that invoke `claude` go through an injected `Client` (in `gremlins/clients/client.py`). Production passes `Client("claude", "sonnet")`; tests pass `FakeClaudeClient(fixtures={label: <jsonl-or-list>})` from `gremlins/clients/fake.py`, which records each `run(...)` call into `self.calls` for assertion. **Never have a stage spawn `claude -p` directly** — go through the injected client so tests can intercept.

`FakeClaudeClient` looks fixtures up by `label`. Stages that re-enter the same logical step within one process (e.g. resumed implement) must use distinct labels per phase.

## State and bail bookkeeping

`state.set_stage` and `state.emit_bail` write to `state.json` atomically in pure Python via `patch_state`. Both helpers no-op without `GR_ID` and never raise — stage / bail bookkeeping must not crash a running gremlin.
