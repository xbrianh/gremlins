"""Row building and table printing."""

import os
from dataclasses import dataclass
from typing import Any

from gremlins.fleet.constants import FMT
from gremlins.fleet.state import (
    display_id,
    humanize_age,
    render_sub_stage,
)


@dataclass
class FleetRow:
    started_at: str
    kind: str
    sid: str
    stage: str
    liveness: str
    live_full: str
    age: str
    client: str
    desc: str
    project_root: str
    gremlin_id: str
    wdir: str
    closed: bool


def build_row(
    gremlin_id: str, _sf: str, wdir: str, state: dict[str, Any], live: str
) -> FleetRow:
    """Return a FleetRow with all display fields resolved."""
    pipeline_path = str(state.get("pipeline_path") or "")
    pipeline_name = (
        os.path.basename(pipeline_path).replace(".yaml", "")
        if pipeline_path
        else str(state.get("kind") or "unknown")
    )[:15]
    pr = state.get("project_root", "")
    stage = state.get("stage") or "-"
    sub = state.get("sub_stage")
    desc = state.get("description") or state.get("instructions") or ""
    started_at = state.get("started_at") or ""

    sub_disp = render_sub_stage(sub)
    stage_disp = stage
    if stage == "waiting" and sub_disp:
        stage_disp = f"waiting:{sub_disp}"

    active_children: list[str] = list(state.get("active_children") or [])
    if active_children and stage != "waiting":
        children_part = (
            f"[{','.join(active_children)}]" if len(active_children) > 1
            else active_children[0]
        )
        stage_disp = f"{stage}/{children_part}"

    rescue_count = state.get("rescue_count") or 0
    try:
        rescue_count = int(rescue_count)
    except (ValueError, TypeError):
        rescue_count = 0

    stage_trim = stage_disp[:22]
    live_trim = live[:28]
    if rescue_count == 1:
        live_trim = f"{live_trim} (rescue)"
    elif rescue_count > 1:
        live_trim = f"{live_trim} (rescue x{rescue_count})"
    closed = os.path.isfile(os.path.join(wdir, "closed"))
    if closed:
        desc_trim = desc[:51] + " [closed]"
    else:
        desc_trim = desc[:60]
    age = humanize_age(started_at)
    sid = display_id(gremlin_id)
    client = state.get("client") or "—"

    return FleetRow(
        started_at=str(started_at),
        kind=pipeline_name,
        sid=sid,
        stage=stage_trim,
        liveness=live_trim,
        live_full=live,
        age=age,
        client=str(client),
        desc=str(desc_trim),
        project_root=str(pr),
        gremlin_id=gremlin_id,
        wdir=wdir,
        closed=closed,
    )


def print_table(rows: list[FleetRow]) -> None:
    """Print header + rows using the fixed format string."""
    print(FMT % ("KIND", "ID", "STAGE", "LIVENESS", "AGE", "CLIENT", "DESCRIPTION"))
    for r in rows:
        print(FMT % (r.kind, r.sid, r.stage, r.liveness, r.age, r.client, r.desc))
