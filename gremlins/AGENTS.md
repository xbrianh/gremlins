# `gremlins/`

Orchestration package for background gremlins. Owns the plan / implement /
review / address pipelines, the fleet manager
(`fleet/`), the chain-step decision agent (`handoff.py`), and the launcher
(`launcher.py`).

## Module layout

- `cli.py` — top-level dispatch: `gremlins {launch,review,address,resume,stop,rescue,land,rm,close,log}`. `launch` accepts a pipeline file or skill name. Bare invocation prints fleet status.
- `bail.py` — `python -m gremlins.bail <class> [detail]`. Writes bail marker to `state.json`.
- `run_pipeline.py` — `python -m gremlins.run_pipeline <gr_id> <kind>`. Spawned by the launcher; wraps `cli.main` and writes terminal state on exit.
- `runner.py` — `run_stages` sequencer (with `resume_from`) + SIGINT/SIGTERM handlers that reap `claude -p` children.
- `state.py` — session-dir resolution, `set_stage` / `emit_bail` / `patch_state` / `check_bail`.
- `utils/git.py` — `in_git_repo`, `head_sha`, branch / worktree helpers.
- `utils/github.py` — `gh` CLI wrappers and stream-json URL extractors used by the gh orchestrator.
- `fleet/` — fleet manager package: status listing + `stop` / `rescue` / `land` / `close` / `rm` / `log` ops. See [`fleet/AGENTS.md`](fleet/AGENTS.md) for the per-module breakdown.
- `clients/protocol.py` — `ClaudeClient` Protocol + `CompletedRun` dataclass.
- `clients/stream.py` — `stream_events` + `_emit_event` (stream-json parser and stderr renderer).
- `clients/claude.py` — `SubprocessClaudeClient` (production subprocess runner).
- `clients/fake.py` — `FakeClaudeClient` recording test double; replays canned stream-json from fixtures keyed by `label`.
- `pipeline/` — `Pipeline` dataclass + `Pipeline.from_yaml(path)` classmethod; `resolve_pipeline_path`; supports parallel stage groups; resolves stage types against `STAGE_REGISTRY` (auto-populated via `stages/all.py`).
- `pipelines/` — bundled YAML pipeline files (`local.yaml`, `gh.yaml`); lookup target for `resolve_pipeline_path`.
- `stages/registry.py` — `STAGE_REGISTRY` and `CLIENT_FACTORIES` dicts + `register_stage` / `register_client_factory` functions.
- `stages/all.py` — side-effect import; importing it causes every stage module to self-register into `STAGE_REGISTRY`. Called automatically by `Pipeline.from_yaml` via `_ensure_registered()`; no manual import needed.
- `stages/base.py` — `Stage` Protocol + `StageContext` dataclass: shared `client`, `session_dir`, `gr_id` threaded into every stage.
- `stages/` — per-stage bodies: `plan`, `implement`, `review_code`, `address_code`, `verify`, `test`, `commit_pr`, `ghreview`, `ghaddress`, `request_copilot`, `wait_copilot`, `wait_ci`, `handoff`.
- `orchestrators/local.py` — `local_main`, `review_main`, `address_main`.
- `orchestrators/gh.py` — `gh_main`. Drives the gh pipeline.
- `orchestrators/boss.py` — `boss_main`. Runs the `Handoff` stage (in-process) and `gremlins {stop,land,rescue}` between child gremlins.
- `prompts/` — externalized prompt templates (plan, implement, review lenses, etc).

## Entry points

| Subcommand | Module |
|---|---|
| `launch local` | `orchestrators.local.local_main` |
| `launch gh` | `orchestrators.gh.gh_main` |
| `launch boss` | `orchestrators.boss.boss_main` |
| `review` | `orchestrators.local.review_main` |
| `address` | `orchestrators.local.address_main` |
| `resume` | `cli._resume_main` |
| `stop` / `rescue` / `land` / `rm` / `close` / `log` | `fleet.cli.*_main` |
| (bare / id-prefix) | `fleet.main` |

## Testability seam: `ClaudeClient`

Every stage that invokes `claude` takes an injected `client: ClaudeClient`
(Protocol in `clients/protocol.py`). Production code passes
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
[`state.py`](state.py); stage-name vocab is defined in the pipeline YAML.

- **Bail classes** (`state.json.bail_class`): `reviewer_requested_changes`, `security`, `secrets`, `other`.
- **Stage names** (`state.json.stage`): stable within a pipeline definition. The authoritative list for any pipeline is its YAML file. `resolve_pipeline_path` checks `.gremlins/pipelines/<name>.yaml` (project-scoped) first, then bundled `gremlins/pipelines/<name>.yaml`; `--pipeline` accepts either a bare name (resolved this way) or a direct path.
- **Marker-protocol bail reasons**: `diagnosis_no_marker`, `diagnosis_bad_marker`, `diagnosis_claude_error`, `diagnosis_timeout`, `excluded_class:<class>`, `attempts_exhausted`, `relaunch_launcher_missing`, `relaunch_failed`.

## Recovering from a child bail

When a child bails in a boss chain, the boss halts. The operator must put the
child into a well-defined state, then rescue the boss. The boss reads only the
child's `state.json` to decide what to do — no `gh` calls, no git inspection.

### Child states the boss recognizes

| Child state | Boss action on rescue |
|---|---|
| `status=running` | Adopt as current child, wait for it to finish |
| `status=bailed`, `external_outcome=landed` | Mark `landed-externally`, next handoff |
| `status=bailed`, `external_outcome=abandoned` | Mark `abandoned`, next handoff |
| `status=bailed`, no `external_outcome` | Refuse to advance — print operator instructions |
| `status=done`, `exit_code=0` | Mark `landed`, next handoff (normal flow) |

### The three operator commands

- `gremlins resume <child-id>` — re-spawn the bailed child at its bailed stage. Use after pushing a fix to the PR or editing the worktree.
- `gremlins ack <child-id>` — assert the child's work is already in main. Writes `external_outcome=landed`. Use after manually merging the child's PR.
- `gremlins skip <child-id>` — give up on the child's work. Writes `external_outcome=abandoned`. Use when the child's plan was wrong and you want the handoff agent to plan something different.

### Operator recovery flows

```sh
# Keep this child going (fix the issue, re-run from bail point):
/ghaddress <pr>
gremlins resume <child-id>
gremlins rescue <boss-id>

# Manually merged the PR:
gh pr merge <pr> --squash
gremlins ack <child-id>
gremlins rescue <boss-id>

# Give up on this child's work, re-plan:
gremlins skip <child-id>
gremlins rescue <boss-id>
```

Two commands per recovery. If the boss was rescued with no operator decision
recorded, it prints the three options above and exits non-zero — it never
silently re-handoffs and spawns a near-duplicate child.

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
