# `gremlins/fleet/`

Fleet manager package for background gremlins. Reads every `${XDG_STATE_HOME:-$HOME/.local/state}/claude-gremlins/<id>/state.json`, applies the shared liveness classifier inline, and prints one scannable line per gremlin. Subcommands (`stop`, `rescue`, `land`, `rm`, `close`, `log`) operate on a single gremlin by id-prefix. Exposed via `python -m gremlins.cli fleet`.

## Module map

| File | Contents |
|---|---|
| `constants.py` | `BG_STALL_SECS`, `STATE_ROOT`, `FMT`, `RESCUE_CAP`, `HEADLESS_DIAGNOSIS_TIMEOUT_SECS`, `EXCLUDED_BAIL_CLASSES` |
| `state.py` | `iso_to_epoch`, `humanize_age`, `display_id`, `render_sub_stage`, `liveness_of_state_file`, `iter_state_files`, `load_state`, `kind_short`, `git_toplevel` |
| `duration.py` | `parse_duration` |
| `render.py` | `build_row`, `print_table` |
| `resolve.py` | `GREMLIN_STAGES`, `resolve_gremlin` |
| `stop.py` | `do_stop` |
| `rescue.py` | `build_rescue_prompt`, `_atomic_patch_state`, `_write_bail`, `write_rescue_report`, `_read_rescue_marker`, `_run_headless_diagnosis`, `do_rescue` |
| `log.py` | `do_log` |
| `close.py` | `do_close` |
| `land.py` | All land helpers + `do_rm`, `do_land`, `expected_branch`, `_print_cost`, `_persist_land_cost`, `_resolve_landing_cwd`, `_fast_forward_main`, `_cleanup_gremlin` |
| `views.py` | `collect_rows`, `do_list`, `do_recent`, `do_drill_in` |
| `cli.py` | `parse_args`, `render_view`, `_dispatch_subcommand`, `_main_impl`, `main` |
| `__init__.py` | Re-exports public surface + installs `_FleetModule` for monkeypatch support |

## Monkeypatch design

Tests patch `BG_STALL_SECS` and `STATE_ROOT` on the `fleet` module object. `state.py` imports `constants` as a module object (`from gremlins.fleet import constants as _constants`) and reads `_constants.BG_STALL_SECS` / `_constants.STATE_ROOT` inside function bodies so that monkeypatches propagate. `__init__.py` installs `_FleetModule` (a `types.ModuleType` subclass) whose `__setattr__` forwards writes to those two names into `sys.modules["gremlins.fleet.constants"]`.
