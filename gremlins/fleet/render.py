"""Row building and table printing."""

import os
from typing import Any

from gremlins.fleet.constants import FMT
from gremlins.fleet.state import display_id, humanize_age, kind_short, render_sub_stage


def build_row(
    gr_id: str, sf: str, wdir: str, state: dict[str, Any], live: str
) -> dict[str, Any]:
    """Return a dict of display fields for a gremlin row."""
    raw_kind = state.get("kind", "")
    k = kind_short(raw_kind)
    pr = state.get("project_root", "")
    stage = state.get("stage") or "-"
    sub = state.get("sub_stage")
    desc = state.get("description") or state.get("instructions") or ""
    started_at = state.get("started_at") or ""

    sub_disp = render_sub_stage(sub)
    stage_disp = stage
    if sub_disp:
        stage_disp = f"{stage} ({sub_disp})"

    rescue_count = state.get("rescue_count") or 0
    try:
        rescue_count = int(rescue_count)
    except (ValueError, TypeError):
        rescue_count = 0

    stage_trim = stage_disp[:22]
    # Rescue marker is appended AFTER the 28-char trim so it stays visible even
    # when the raw liveness reason is long; the row may overflow the column in
    # those cases but the (rescue) indicator is more important than alignment.
    live_trim = live[:28]
    if rescue_count == 1:
        live_trim = f"{live_trim} (rescue)"
    elif rescue_count > 1:
        live_trim = f"{live_trim} (rescue x{rescue_count})"
    desc_trim = desc[:60]
    age = humanize_age(started_at)
    sid = display_id(gr_id)
    parent_id = state.get("parent_id") or ""
    boss_disp = display_id(parent_id)[:20] if parent_id else ""

    impl_model = state.get("impl_model") or state.get("model") or "—"
    model_trim = impl_model[:10]

    return {
        "started_at": started_at,
        "kind": k,
        "sid": sid,
        "boss": boss_disp,
        "stage": stage_trim,
        "live": live_trim,
        "live_full": live,
        "age": age,
        "model": model_trim,
        "desc": desc_trim,
        "project_root": pr,
        "gr_id": gr_id,
        "wdir": wdir,
        "closed": os.path.isfile(os.path.join(wdir, "closed")),
        "state": state,
    }


def print_table(rows: list[dict[str, Any]]) -> None:
    """Print header + rows using the fixed format string."""
    print(
        FMT % ("KIND", "ID", "STAGE", "LIVENESS", "AGE", "BOSS", "MODEL", "DESCRIPTION")
    )
    for r in rows:
        print(
            FMT
            % (
                r["kind"],
                r["sid"],
                r["stage"],
                r["live"],
                r["age"],
                r["boss"],
                r["model"],
                r["desc"],
            )
        )
