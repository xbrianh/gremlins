# gremlins

Background orchestration pipelines for Claude Code. Given a goal or GitHub issue,
a gremlin runs the full plan → implement → review-code → address-code cycle
unattended, writing artifacts to `~/.local/state/claude-gremlins/` and optionally
opening a pull request. A fleet manager tracks running, stalled, and finished
gremlins and provides stop / rescue / land / close operations.

**Status:** pre-1.0, not published to PyPI. The copy at `~/.claude/gremlins/`
is still what Claude Code skills (`/localgremlin`, `/ghgremlin`, `/bossgremlin`)
consume today — this repo is the upstream source.

---

## Dev install

```sh
uv venv
source .venv/bin/activate  # or `.venv\Scripts\activate` on Windows
uv pip install -e ".[dev]"
```

## Make targets

| Target | What it runs |
|---|---|
| `make test` | `pytest` |
| `make lint` | `ruff check .` |
| `make format` | `ruff format --check .` (check only — does not rewrite files) |
| `make typecheck` | `pyright` |
| `make check` | lint + format + typecheck |

## CLI subcommands

Invoked as `python -m gremlins.cli <subcommand>` or `gremlins <subcommand>`
after install. The authoritative list and per-subcommand description lives in
the module docstring at the top of [`gremlins/cli.py`](gremlins/cli.py).

| Subcommand | Purpose |
|---|---|
| `launch local` | Full local pipeline: plan → implement → review-code → address-code |
| `launch gh` | GitHub issue-driven pipeline (plan → implement → PR → Copilot review → address) |
| `launch boss` | Chained serial workflow driven by a top-level spec |
| `review` | review-code stage only |
| `address` | address-code stage only |
| `resume` | Re-spawn an existing gremlin from its recorded stage |
| `stop` | Send SIGTERM to a running gremlin |
| `rescue` | Diagnose and resume a dead or stalled gremlin |
| `land` | Land a finished gremlin onto the current branch |
| `rm` | Delete a dead gremlin's state dir, worktree, and branch |
| `close` | Mark a dead gremlin as closed |
| `log` | Tail the gremlin's log file |

`_run-pipeline` is an internal spawn boundary; not for direct use.

## Pipeline loader

`gremlins/pipeline.py` loads YAML pipeline definitions. Key API:

- `load_pipeline(path)` → `Pipeline` — parses a YAML file, resolves `clients`
  via `CLIENT_FACTORIES`, and validates every stage `type` against
  `STAGE_REGISTRY` (populated by importing `gremlins.stages.all`).
- `resolve_pipeline_path(name_or_path, base_dir)` — resolves a bare name (e.g.
  `"local"`) by checking `<base_dir>/.gremlins/pipelines/<name>.yaml` first,
  then the bundled `gremlins/pipelines/` directory.

Dataclasses: `Pipeline`, `StageEntry` (supports `type="parallel"` groups),
`ClientDef`.

Bundled YAML pipeline files live in `gremlins/pipelines/` (`local.yaml`,
`gh.yaml`).

## Internals docs

- [`gremlins/CLAUDE.md`](gremlins/CLAUDE.md) — module layout, entry points,
  testability seam, byte-stable strings
- [`gremlins/fleet/CLAUDE.md`](gremlins/fleet/CLAUDE.md) — fleet manager internals
- [`gremlins/orchestrators/CLAUDE.md`](gremlins/orchestrators/CLAUDE.md) — orchestrator internals
- [`gremlins/stages/CLAUDE.md`](gremlins/stages/CLAUDE.md) — stage internals

## Planned: `gremlins install`

A future `gremlins install` subcommand will bootstrap the Claude Code skill
layer — syncing this package into `~/.claude/gremlins/` and wiring up the
hook and skill definitions. **This command does not exist yet.**
