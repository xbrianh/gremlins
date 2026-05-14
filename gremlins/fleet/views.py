"""List, recent, and drill-in views."""

import argparse
import datetime
import json
import os
import time
from collections.abc import Iterator

from gremlins.executor.state import StateData
from gremlins.fleet.duration import parse_duration
from gremlins.fleet.render import FleetRow, build_row, print_table
from gremlins.fleet.state import (
    humanize_age,
    iso_to_epoch,
    iter_state_files,
    liveness_of_state_file,
    load_state,
    parse_liveness,
)


def _iter_filtered_gremlins(
    here_root: str | None = None,
    pipeline_filter: str | None = None,
    since_secs: float | None = None,
    liveness_filter: set[str] | None = None,
    include_closed: bool = False,
) -> Iterator[tuple[str, str, str, dict[str, object], str]]:
    now = time.time()
    for gremlin_id, sf, wdir in iter_state_files():
        if not include_closed and os.path.isfile(os.path.join(wdir, "closed")):
            continue
        state = load_state(sf)
        if not state:
            continue
        gremlin_id = str(state.get("id") or gremlin_id)
        if not gremlin_id:
            continue
        live = liveness_of_state_file(sf, state)
        if here_root is not None and state.get("project_root", "") != here_root:
            continue
        if pipeline_filter is not None:
            pipeline_path = str(state.get("pipeline_path") or "")
            pipeline_name = (
                os.path.basename(pipeline_path).replace(".yaml", "")
                if pipeline_path
                else str(state.get("kind") or "")
            )
            if pipeline_filter not in pipeline_name:
                continue
        if since_secs is not None:
            started_at: str = str(state.get("started_at") or "")
            epoch = iso_to_epoch(started_at)
            if epoch is None or (now - epoch) > since_secs:
                continue
        if liveness_filter and not any(live.startswith(p) for p in liveness_filter):
            continue
        yield gremlin_id, sf, wdir, state, live


def collect_rows(
    here_root: str | None = None,
    pipeline_filter: str | None = None,
    since_secs: float | None = None,
    liveness_filter: set[str] | None = None,
    include_closed: bool = False,
) -> list[FleetRow]:
    rows = [
        build_row(gid, sf, wdir, state, live)
        for gid, sf, wdir, state, live in _iter_filtered_gremlins(
            here_root=here_root,
            pipeline_filter=pipeline_filter,
            since_secs=since_secs,
            liveness_filter=liveness_filter,
            include_closed=include_closed,
        )
    ]
    rows.sort(key=lambda r: r.started_at)
    return rows


def do_list(args: argparse.Namespace, here_root: str | None = None) -> None:
    """Default list view."""
    liveness_filter: set[str] | None = None
    if args.running or args.dead or args.stalled:
        liveness_filter = set()
        if args.running:
            liveness_filter.add("running")
            liveness_filter.add("waiting")
        if args.dead:
            liveness_filter.add("dead:")
            liveness_filter.add("finished")
        if args.stalled:
            liveness_filter.add("stalled:")

    since_secs = None
    if args.since:
        try:
            since_secs = parse_duration(args.since)
        except ValueError as e:
            print(f"error: {e}")
            return

    rows = collect_rows(
        here_root=here_root,
        pipeline_filter=args.pipeline,
        since_secs=since_secs,
        liveness_filter=liveness_filter,
        include_closed=False,
    )

    # Running gremlins float to the top; within each group, older gremlins
    # appear first by started_at.
    rows.sort(
        key=lambda r: (
            r.live_full != "running" and not r.live_full.startswith("waiting"),
            r.started_at,
        )
    )

    if not rows:
        if here_root is not None:
            print(f"No active gremlins for project: {here_root}")
        else:
            print("No active gremlins on this machine.")
        return

    print_table(rows)


def do_recent(args: argparse.Namespace, here_root: str | None = None) -> None:
    """--recent [N]: show dead gremlins started within N hours."""
    n_hours = args.recent
    since_secs = n_hours * 3600

    rows = collect_rows(
        here_root=here_root,
        pipeline_filter=args.pipeline,
        since_secs=since_secs,
        liveness_filter={"dead:", "finished"},
        include_closed=True,
    )

    if not rows:
        if here_root is not None:
            print(f"No recent gremlins for project: {here_root}")
        else:
            print("No recent gremlins on this machine.")
        return

    print_table(rows)


def do_drill_in(target: str) -> None:
    """Print every field of a uniquely-matched gremlin in a labeled block."""
    matches: list[tuple[str, str, str]] = []
    for gremlin_id, sf, wdir in iter_state_files():
        if target in gremlin_id:
            matches.append((gremlin_id, sf, wdir))

    if not matches:
        print(f"no gremlin matched: {target}")
        return
    if len(matches) > 1:
        print(
            f"ambiguous id '{target}' matched {len(matches)} gremlins — use a longer prefix:"
        )
        for gremlin_id, _, _ in matches:
            print(f"  {gremlin_id}")
        return

    gremlin_id, sf, wdir = matches[0]
    state = load_state(sf)
    if not state:
        print(f"error: could not read state for {gremlin_id}")
        return

    live = liveness_of_state_file(sf)
    started_at: str = str(state.get("started_at") or "")
    age = humanize_age(started_at)

    # Convert started_at to local time for display.
    local_start = ""
    epoch = iso_to_epoch(started_at)
    if epoch is not None:
        local_start = datetime.datetime.fromtimestamp(epoch).strftime(
            "%Y-%m-%d %H:%M:%S %Z"
        )

    print(f"gremlin: {gremlin_id}")
    print(f"  liveness : {live}")
    print(
        f"  closed   : {'yes' if os.path.isfile(os.path.join(wdir, 'closed')) else 'no'}"
    )
    print(f"  age      : {age}")
    if local_start:
        print(f"  started  : {local_start}")

    # Surface bail markers (if any) above the raw state dump so they're
    # immediately visible. bail_class is upstream-set by review/address
    # stages; bail_reason/bail_detail are headless-rescue-set when it
    # declined to proceed.
    _gremlin_id_for_bail = str(state.get("id") or "")
    _bail_file = (
        StateData.load(_gremlin_id_for_bail).read_bail_info()
        if _gremlin_id_for_bail
        else None
    )
    bail_class = (
        (_bail_file.get("class") or "")
        if _bail_file
        else (state.get("bail_class") or "")
    )
    bail_reason = state.get("bail_reason")
    bail_detail = (
        (_bail_file.get("detail") or "")
        if _bail_file
        else (state.get("bail_detail") or "")
    )
    if bail_class or bail_reason:
        print("  bail:")
        if bail_class:
            print(f"    class  : {bail_class}")
        if bail_reason:
            print(f"    reason : {bail_reason}")
        if bail_detail:
            print(f"    detail : {bail_detail}")

    print("  state.json fields:")
    for key, val in state.items():
        print(f"    {key}: {json.dumps(val)}")

    print()
    print(f"  state directory: {wdir}")
    log_path = os.path.join(wdir, "log")
    artifacts_dir = os.path.join(wdir, "artifacts")
    artifact_paths: list[str] = []
    if os.path.isdir(artifacts_dir):
        for fname in sorted(os.listdir(artifacts_dir)):
            fpath = os.path.join(artifacts_dir, fname)
            if os.path.isfile(fpath):
                artifact_paths.append(fpath)
    rescue_reports: list[str] = []
    try:
        for fname in sorted(os.listdir(wdir)):
            if fname.startswith("rescue-") and fname.endswith(".md"):
                fpath = os.path.join(wdir, fname)
                if os.path.isfile(fpath):
                    rescue_reports.append(fpath)
    except OSError:
        pass
    has_log = os.path.isfile(log_path)
    if has_log:
        print(f"    log: {log_path}")
    if artifact_paths:
        print("    artifacts:")
        for fpath in artifact_paths:
            print(f"      {fpath}")
    if rescue_reports:
        print("    rescue reports:")
        for fpath in rescue_reports:
            print(f"      {fpath}")
    if not has_log and not artifact_paths and not rescue_reports:
        print("    (no log, artifacts, or rescue reports)")


def _gremlin_to_json(
    gremlin_id: str, wdir: str, state: dict[str, object], live: str
) -> dict[str, object]:
    started_at = str(state.get("started_at") or "")
    epoch = iso_to_epoch(started_at)
    age_seconds: float | None = (time.time() - epoch) if epoch is not None else None
    pipeline_path = str(state.get("pipeline_path") or "")
    kind = (
        os.path.basename(pipeline_path).replace(".yaml", "")
        if pipeline_path
        else str(state.get("kind") or "unknown")
    )
    return {
        "id": gremlin_id,
        "kind": kind,
        "stage": str(state.get("stage") or ""),
        "sub_stage": state.get("sub_stage"),
        "liveness": parse_liveness(live),
        "age_seconds": age_seconds,
        "client": str(state.get("client") or ""),
        "description": str(state.get("description") or state.get("instructions") or ""),
        "started_at": started_at,
        "project_root": str(state.get("project_root") or ""),
        "closed": os.path.isfile(os.path.join(wdir, "closed")),
    }


def do_list_json(args: argparse.Namespace, here_root: str | None = None) -> None:
    if args.recent is not None:
        since_secs: float | None = args.recent * 3600
        liveness_filter: set[str] | None = {"dead:", "finished"}
        include_closed = True
    else:
        liveness_filter = None
        if args.running or args.dead or args.stalled:
            liveness_filter = set()
            if args.running:
                liveness_filter |= {"running", "waiting"}
            if args.dead:
                liveness_filter |= {"dead:", "finished"}
            if args.stalled:
                liveness_filter.add("stalled:")
        since_secs = None
        if args.since:
            try:
                since_secs = parse_duration(args.since)
            except ValueError as e:
                print(f"error: {e}", file=__import__("sys").stderr)
                return
        include_closed = False

    items = sorted(
        (
            _gremlin_to_json(gid, wdir, state, live)
            for gid, _sf, wdir, state, live in _iter_filtered_gremlins(
                here_root=here_root,
                pipeline_filter=args.pipeline,
                since_secs=since_secs,
                liveness_filter=liveness_filter,
                include_closed=include_closed,
            )
        ),
        key=lambda d: str(d.get("started_at") or ""),
    )
    print(json.dumps(items, indent=2))


def do_drill_in_json(target: str) -> None:
    matches: list[tuple[str, str, str]] = []
    for gremlin_id, sf, wdir in iter_state_files():
        if target in gremlin_id:
            matches.append((gremlin_id, sf, wdir))

    if not matches:
        print(json.dumps({"error": f"no gremlin matched: {target}"}))
        return
    if len(matches) > 1:
        print(
            json.dumps(
                {
                    "error": f"ambiguous id '{target}' matched {len(matches)} gremlins",
                    "matches": [m[0] for m in matches],
                }
            )
        )
        return

    gremlin_id, sf, wdir = matches[0]
    state = load_state(sf)
    if not state:
        print(json.dumps({"error": f"could not read state for {gremlin_id}"}))
        return

    live = liveness_of_state_file(sf)
    started_at = str(state.get("started_at") or "")
    epoch = iso_to_epoch(started_at)
    age_seconds: float | None = (time.time() - epoch) if epoch is not None else None

    gremlin_id_for_bail = str(state.get("id") or "")
    bail_file = (
        StateData.load(gremlin_id_for_bail).read_bail_info()
        if gremlin_id_for_bail
        else None
    )
    bail_class = (
        (bail_file.get("class") or "") if bail_file else (state.get("bail_class") or "")
    )
    bail_reason = state.get("bail_reason")
    bail_detail = (
        (bail_file.get("detail") or "")
        if bail_file
        else (state.get("bail_detail") or "")
    )

    log_path = os.path.join(wdir, "log")
    artifacts_dir = os.path.join(wdir, "artifacts")
    artifact_paths: list[str] = []
    if os.path.isdir(artifacts_dir):
        for fname in sorted(os.listdir(artifacts_dir)):
            fpath = os.path.join(artifacts_dir, fname)
            if os.path.isfile(fpath):
                artifact_paths.append(fpath)
    rescue_reports: list[str] = []
    try:
        for fname in sorted(os.listdir(wdir)):
            if fname.startswith("rescue-") and fname.endswith(".md"):
                fpath = os.path.join(wdir, fname)
                if os.path.isfile(fpath):
                    rescue_reports.append(fpath)
    except OSError:
        pass

    obj: dict[str, object] = {
        "id": gremlin_id,
        "liveness": parse_liveness(live),
        "closed": os.path.isfile(os.path.join(wdir, "closed")),
        "age_seconds": age_seconds,
        "started_at": started_at,
        "bail_class": bail_class or None,
        "bail_reason": bail_reason or None,
        "bail_detail": bail_detail or None,
        "state_dir": wdir,
        "log_path": log_path if os.path.isfile(log_path) else None,
        "artifact_paths": artifact_paths,
        "rescue_reports": rescue_reports,
        "state": state,
    }
    print(json.dumps(obj, indent=2))
