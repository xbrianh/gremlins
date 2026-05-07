"""Fleet manager package for background gremlins.

Reads every gremlin state file under the per-user state directory from
``gremlins.paths.state_root()``, applies the shared liveness classifier inline,
and prints one scannable line per gremlin. Subcommands (``ack``, ``close``,
``land``, ``log``, ``rescue``, ``rm``, ``skip``, ``stop``) operate on a single
gremlin by id-prefix.

Exposed via ``python -m gremlins.cli fleet``.

Exit 0 on the listing path even on unexpected errors: same "never break a
session" principle as the session-summary hook.
"""

from gremlins.fleet.ack import (
    do_ack as do_ack,
)
from gremlins.fleet.ack import (
    do_skip as do_skip,
)
from gremlins.fleet.cli import (
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
    do_land,
    do_rm,
    expected_branch,
)
from gremlins.fleet.log import do_log
from gremlins.fleet.render import build_row, print_table
from gremlins.fleet.rescue import (
    build_rescue_prompt,
    do_rescue,
    write_rescue_report,
)
from gremlins.fleet.resolve import GREMLIN_STAGES, resolve_gremlin
from gremlins.fleet.state import (
    display_id,
    effective_pipeline_kind,
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
    "effective_pipeline_kind",
    "git_toplevel",
    # duration
    "parse_duration",
    # render
    "build_row",
    "print_table",
    # resolve
    "GREMLIN_STAGES",
    "resolve_gremlin",
    # ack / skip
    "do_ack",
    "do_skip",
    # stop
    "do_stop",
    # rescue
    "build_rescue_prompt",
    "write_rescue_report",
    "do_rescue",
    # log
    "do_log",
    # close
    "do_close",
    # land
    "expected_branch",
    "do_rm",
    "do_land",
    # views
    "collect_rows",
    "do_list",
    "do_recent",
    "do_drill_in",
]
