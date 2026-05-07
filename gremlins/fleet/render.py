"""Row building and table printing."""

import os
from dataclasses import dataclass
from typing import Any

from gremlins.fleet.constants import FMT
from gremlins.fleet.state import (
    display_id,
    effective_pipeline_kind,
    humanize_age,
    render_sub_stage,
)


@dataclass
class FleetRow:
    started_at: str
    kind: str
    sid: str
    boss: str
    stage: str
    liveness: str
    live_full: str
    age: str
    client: str
    desc: str
    project_root: str
    gr_id: str
    wdir: str
    closed: bool
    state: dict[str, Any]


def build_row(
    gr_id: str, _sf: str, wdir: str, state: dict[str, Any], live: str
) -> FleetRow:
    """Return a FleetRow with all display fields resolved."""
    k = effective_pipeline_kind(state)
    pr = state.get("project_root", "")
    stage = state.get("stage") or "-"
    sub = state.get("sub_stage")
    desc = state.get("description") or state.get("instructions") or ""
    started_at = state.get("started_at") or ""

    sub_disp = render_sub_stage(sub)
    stage_disp = stage
    if sub_disp:
        stage_disp = f"{stage} ({sub_disp})"

    # Boss in waiting: surface the child's current pipeline stage.
    if k == "boss" and stage == "waiting":
        child_stage = str(state.get("chain_current_child_stage") or "")
        if child_stage:
            stage_disp = f"waiting:{child_stage}"

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

    client = state.get("client") or "—"

    return FleetRow(
        started_at=str(started_at),
        kind=k,
        sid=sid,
        boss=boss_disp,
        stage=stage_trim,
        liveness=live_trim,
        live_full=live,
        age=age,
        client=str(client),
        desc=str(desc_trim),
        project_root=str(pr),
        gr_id=gr_id,
        wdir=wdir,
        closed=os.path.isfile(os.path.join(wdir, "closed")),
        state=state,
    )


def print_table(rows: list[FleetRow]) -> None:
    """Print header + rows using the fixed format string."""
    print(
        FMT
        % ("KIND", "ID", "STAGE", "LIVENESS", "AGE", "BOSS", "CLIENT", "DESCRIPTION")
    )
    for r in rows:
        print(
            FMT
            % (
                r.kind,
                r.sid,
                r.stage,
                r.liveness,
                r.age,
                r.boss,
                r.client,
                r.desc,
            )
        )
