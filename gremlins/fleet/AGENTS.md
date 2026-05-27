# `gremlins/fleet/`

Fleet manager package for background gremlins. Reads every gremlin state file under `gremlins.paths.state_root()`, applies the shared liveness classifier inline, and prints one scannable line per gremlin. Fleet ops (`ack`, `skip`, `stop`, `land`, `rm`, `close`, `log`) are exposed as top-level `gremlins` subcommands via `gremlins/cli/fleet.py`.

## Module map

| File | Contents |
|---|---|
| `constants.py` | `BG_STALL_SECS`, `STATE_ROOT`, `FMT`, `RESCUE_CAP`, `HEADLESS_DIAGNOSIS_TIMEOUT_SECS`, `EXCLUDED_BAIL_CLASSES` |
| `state.py` | `iso_to_epoch`, `humanize_age`, `display_id`, `render_sub_stage`, `liveness_of_state_file`, `iter_state_files`, `load_state`, `kind_short`, `git_toplevel`, `atomic_patch_state` |
| `duration.py` | `parse_duration` |
| `render.py` | `build_row`, `print_table` |
| `resolve.py` | `GREMLIN_STAGES`, `resolve_gremlin` |
| `ack.py` | `do_ack`, `do_skip` |
| `stop.py` | `do_stop` |
| `log.py` | `do_log` |
| `close.py` | `do_close` |
| `land.py` | All land helpers + `do_rm`, `do_land`, `expected_branch`, `_print_cost`, `_persist_land_cost`, `_resolve_landing_cwd`, `_fast_forward_main`, `_cleanup_gremlin` |
| `views.py` | `collect_rows`, `do_list`, `do_recent`, `do_drill_in`, `do_list_json`, `do_drill_in_json` |
| `session_summary.py` | SessionStart/UserPromptSubmit hook: filters gremlins by `project_root`, reports running + newly-finished, marks finished as `summarized`, prunes closed state dirs older than 14 days |
| `__init__.py` | Package docstring only |

## JSON output contract (`--json`)

`gremlins --json` emits a JSON object; `gremlins <id> --json` emits one JSON object. Both are stdout-only — no decorative text when `--json` is active.

### Fleet list (`gremlins --json`)
Top-level shape:
```json
{
  "gremlins": [ ... ],
  "queue": {
    "pending": 0,
    "running": 0,
    "failed": 0,
    "runner_active": false
  }
}
```

Each element of `gremlins`:
```json
{
  "id": "string",
  "kind": "string",
  "stage": "string",
  "sub_stage": "string | object | null",
  "liveness": { "state": "running | waiting | finished | dead | stalled", ... },
  "age_seconds": 123.4,
  "client": "string",
  "description": "string",
  "started_at": "ISO-8601 string",
  "project_root": "string",
  "closed": false
}
```

### `liveness` object shapes
| `state` | additional fields |
|---|---|
| `running` | — |
| `waiting` | `duration?: "3m12s"` |
| `finished` | — |
| `dead` | `reason: "exit" \| "bailed" \| "host-terminated" \| "crashed" \| ...`, plus `exit_code?: int` or `bail_reason?: string` |
| `stalled` | `detail: string` |

### Drill-in (`gremlins <id> --json`)
Top-level fields: `id`, `liveness`, `closed`, `age_seconds`, `started_at`, `bail_class`, `bail_reason`, `bail_detail`, `state_dir`, `log_path`, `artifact_paths`, `rescue_reports`, `state` (full state.json object).

## Monkeypatch design

Tests patch `BG_STALL_SECS` and `STATE_ROOT` directly on `gremlins.fleet.constants`. Submodules import `constants` as a module object (`import gremlins.fleet.constants as _constants`) and read `_constants.BG_STALL_SECS` / `_constants.STATE_ROOT` inside function bodies so that monkeypatches propagate.
