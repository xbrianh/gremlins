# `gremlins/`

Orchestration package for background gremlins. Owns the plan / implement /
review / address pipelines (`local`, `gh`, `boss`), the fleet manager
(`fleet/`), the chain-step decision agent (`handoff.py`), and the launcher
(`launcher.py`).

## Module layout

- `cli.py` — `python -m gremlins.cli {local,review,address,gh,boss,fleet,handoff}` dispatch.
- `runner.py` — `run_stages` sequencer (with `resume_from`) + SIGINT/SIGTERM handlers that reap `claude -p` children.
- `state.py` — session-dir resolution, `set_stage` / `emit_bail` / `patch_state` / `check_bail`.
- `git.py` — `in_git_repo`, `git_head`, branch / worktree helpers.
- `gh_utils.py` — `gh` CLI wrappers and stream-json URL extractors used by the gh orchestrator.
- `fleet/` — fleet manager package: status listing + `stop` / `rescue` / `land` / `close` / `rm` / `log` subcommands. See [`fleet/CLAUDE.md`](fleet/CLAUDE.md) for the per-module breakdown.
- `handoff.py` — chain-step decision agent (next-plan / chain-done / bail).
- `clients/claude.py` — `ClaudeClient` Protocol + `SubprocessClaudeClient` (production).
- `clients/fake.py` — `FakeClaudeClient` recording test double; replays canned stream-json from fixtures keyed by `label`.
- `stages/` — per-stage bodies: `plan`, `implement`, `review_code`, `address_code`, `test`, `commit_pr`, `ghreview`, `ghaddress`, `wait_copilot`. (The `request-copilot` stage body is inlined as a closure in `orchestrators/gh.py`.)
- `orchestrators/local.py` — `local_main`, `review_main`, `address_main`.
- `orchestrators/gh.py` — `gh_main`. Drives the gh pipeline.
- `orchestrators/boss.py` — `boss_main`. Subprocesses out to `python -m gremlins.cli handoff` and `python -m gremlins.cli fleet {stop,land,rescue}` between child gremlins.
- `prompts/` — externalized prompt templates (plan, implement, review lenses, etc).

## Entry points

| Subcommand | Module |
|---|---|
| `local` | `orchestrators.local.local_main` |
| `review` | `orchestrators.local.review_main` |
| `address` | `orchestrators.local.address_main` |
| `gh` | `orchestrators.gh.gh_main` |
| `boss` | `orchestrators.boss.boss_main` |
| `fleet` | `fleet.main` |
| `handoff` | `handoff.main` |

## Testability seam: `ClaudeClient`

Every stage that invokes `claude` takes an injected `client: ClaudeClient`
(Protocol in `clients/claude.py`). Production code passes
`SubprocessClaudeClient()` to those stages; tests pass
`FakeClaudeClient(fixtures={label: <jsonl-or-list>})`. The fake records each
`run(...)` call into `self.calls` for assertion. **Never have a stage
spawn `claude -p` directly** — go through the injected client so tests can
intercept it.

`FakeClaudeClient` looks fixtures up by `label`. Stages that re-enter the
same logical step within one process (e.g. resumed implement) must use
distinct labels per phase.

## Byte-stable strings — DO NOT change

These values are persisted to `state.json` files and read by other
writers (`session-summary.sh` hook, `liveness.sh` sourced from
`session-summary.sh`, the fleet manager that inlines an equivalent
classifier in [`fleet/state.py`](fleet/state.py), the launcher, the rescue
protocol). Renaming any of them silently breaks cross-process
consumers. Source of truth: bail-class constants live in
[`state.py`](state.py); local / gh stage-name vocab is defined and
validated in the orchestrators.

- **Bail classes** (`state.json.bail_class`): `reviewer_requested_changes`, `security`, `secrets`, `other`.
- **Local stage names**: `plan`, `implement`, `review-code`, `address-code`, `test`.
- **Gh stage names**: `plan`, `implement`, `commit-pr`, `request-copilot`, `ghreview`, `wait-copilot`, `ghaddress`.
- **Marker-protocol bail reasons**: `diagnosis_no_marker`, `diagnosis_bad_marker`, `diagnosis_claude_error`, `diagnosis_timeout`, `excluded_class:<class>`, `attempts_exhausted`, `relaunch_launcher_missing`, `relaunch_failed`.

## Stage and bail bookkeeping

`state.set_stage` and `state.emit_bail` write to `state.json` atomically
in pure Python via `patch_state`. Both helpers no-op without `GR_ID` and
never raise — stage / bail bookkeeping must not crash a running gremlin.

## Tests

```
uv pip install -e ".[dev]"
python -m pytest
```

`make test` runs the same thing. Tests live at the top-level
`./tests/` (sibling to this package), discovered via
`[tool.pytest.ini_options] testpaths = ["tests"]` in the repo's
`pyproject.toml`.
