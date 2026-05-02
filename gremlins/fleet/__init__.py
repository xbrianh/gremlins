"""Fleet manager package for background gremlins.

Reads every ``${XDG_STATE_HOME:-$HOME/.local/state}/claude-gremlins/<id>/state.json``,
applies the shared liveness classifier inline, and prints one scannable line per
gremlin. Subcommands (``stop``, ``rescue``, ``land``, ``rm``, ``close``, ``log``)
operate on a single gremlin by id-prefix.

Exposed via ``python -m gremlins.cli fleet``.

Exit 0 on the listing path even on unexpected errors: same "never break a
session" principle as the session-summary hook.
"""

from gremlins.fleet.cli import (
    _dispatch_subcommand,
    _main_impl,
    main,
    parse_args,
    render_view,
)
from gremlins.fleet.close import do_close
from gremlins.fleet.constants import (
    BG_STALL_SECS,
    EXCLUDED_BAIL_CLASSES,
    FMT,
    HEADLESS_DIAGNOSIS_TIMEOUT_SECS,
    RESCUE_CAP,
    STATE_ROOT,
)
from gremlins.fleet.duration import parse_duration
from gremlins.fleet.land import (
    _cleanup_gremlin,
    _fast_forward_main,
    _land_boss,
    _land_gh,
    _land_local,
    _persist_land_cost,
    _print_cost,
    _resolve_landing_cwd,
    _synthesize_commit_message_ai,
    do_land,
    do_rm,
    expected_branch,
)
from gremlins.fleet.log import do_log
from gremlins.fleet.render import build_row, print_table
from gremlins.fleet.rescue import (
    _atomic_patch_state,
    _read_rescue_marker,
    _recreate_worktree,
    _run_headless_diagnosis,
    _write_bail,
    build_rescue_prompt,
    do_rescue,
    write_rescue_report,
)
from gremlins.fleet.resolve import GREMLIN_STAGES, resolve_gremlin
from gremlins.fleet.state import (
    display_id,
    git_toplevel,
    humanize_age,
    iso_to_epoch,
    iter_state_files,
    kind_short,
    liveness_of_state_file,
    load_state,
    render_sub_stage,
)
from gremlins.fleet.stop import do_stop
from gremlins.fleet.views import collect_rows, do_drill_in, do_list, do_recent

__all__ = [
    # cli
    "main",
    "parse_args",
    "render_view",
    "_dispatch_subcommand",
    "_main_impl",
    # constants
    "BG_STALL_SECS",
    "STATE_ROOT",
    "FMT",
    "RESCUE_CAP",
    "HEADLESS_DIAGNOSIS_TIMEOUT_SECS",
    "EXCLUDED_BAIL_CLASSES",
    # state
    "iso_to_epoch",
    "humanize_age",
    "display_id",
    "render_sub_stage",
    "liveness_of_state_file",
    "iter_state_files",
    "load_state",
    "kind_short",
    "git_toplevel",
    # duration
    "parse_duration",
    # render
    "build_row",
    "print_table",
    # resolve
    "GREMLIN_STAGES",
    "resolve_gremlin",
    # stop
    "do_stop",
    # rescue
    "build_rescue_prompt",
    "_atomic_patch_state",
    "_write_bail",
    "write_rescue_report",
    "_read_rescue_marker",
    "_run_headless_diagnosis",
    "_recreate_worktree",
    "do_rescue",
    # log
    "do_log",
    # close
    "do_close",
    # land
    "expected_branch",
    "_print_cost",
    "_persist_land_cost",
    "_resolve_landing_cwd",
    "_fast_forward_main",
    "_cleanup_gremlin",
    "do_rm",
    "_land_local",
    "_land_boss",
    "_land_gh",
    "do_land",
    "_synthesize_commit_message_ai",
    # stdlib re-export for tests that patch fleet.subprocess.Popen
    "subprocess",
    # views
    "collect_rows",
    "do_list",
    "do_recent",
    "do_drill_in",
]

# ---------------------------------------------------------------------------
# Monkeypatch support for tests
#
# Tests do:
#   from gremlins import fleet as gremlins_fleet
#   monkeypatch.setattr(gremlins_fleet, "BG_STALL_SECS", 100)
#   monkeypatch.setattr(gremlins_fleet, "STATE_ROOT", str(state_root))
#
# state.py imports `constants` as a module object and reads
# `_constants.BG_STALL_SECS` / `_constants.STATE_ROOT` inside function bodies,
# so writes must propagate to the constants module's __dict__.
#
# _FleetModule.__setattr__ handles that propagation transparently.
# ---------------------------------------------------------------------------

import subprocess
import sys
import types


class _FleetModule(types.ModuleType):
    # Map each patchable name to the submodule that owns it. Tests set these
    # on the fleet module object; __setattr__ forwards them so the functions
    # that actually call them see the patched value.
    _SUBMODULE_ATTRS = {
        "BG_STALL_SECS": "gremlins.fleet.constants",
        "STATE_ROOT": "gremlins.fleet.constants",
        "_run_headless_diagnosis": "gremlins.fleet.rescue",
        "_recreate_worktree": "gremlins.fleet.rescue",
        "_synthesize_commit_message_ai": "gremlins.fleet.land",
    }

    def __setattr__(self, name, value):
        object.__setattr__(self, name, value)
        submod_name = self._SUBMODULE_ATTRS.get(name)
        if submod_name is not None:
            mod = sys.modules.get(submod_name)
            if mod is not None:
                object.__setattr__(mod, name, value)


sys.modules[__name__].__class__ = _FleetModule
