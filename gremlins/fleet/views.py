"""List, recent, and drill-in views."""

import argparse
import datetime
import json
import os
import time

from gremlins.fleet.duration import parse_duration
from gremlins.fleet.render import build_row, print_table
from gremlins.fleet.state import (
    humanize_age,
    iso_to_epoch,
    iter_state_files,
    kind_short,
    liveness_of_state_file,
    load_state,
)


def collect_rows(
    here_root=None,
    kind_filter=None,
    since_secs=None,
    liveness_filter=None,
    include_closed=False,
):
    """
    Collect and return a list of row dicts, sorted by started_at ascending.

    here_root         — if set, restrict to gremlins with this project_root.
    kind_filter       — if set ('local', 'gh', or 'boss'), restrict to that kind.
    since_secs        — if set, restrict to gremlins started within this many seconds.
    liveness_filter   — if set, a set of prefixes ('running', 'dead', 'stalled').
    include_closed    — if True, include closed gremlins (for drill-in / --recent).
    """
    now = time.time()
    rows = []
    for gr_id, sf, wdir in iter_state_files():
        if not include_closed and os.path.isfile(os.path.join(wdir, "closed")):
            continue

        state = load_state(sf)
        if not state:
            continue
        gr_id_from_state = state.get("id") or gr_id
        if not gr_id_from_state:
            continue

        live = liveness_of_state_file(sf, state)

        # --here filter
        if here_root is not None:
            if state.get("project_root", "") != here_root:
                continue

        # --kind filter
        if kind_filter is not None:
            if kind_short(state.get("kind", "")) != kind_filter:
                continue

        # --since filter
        if since_secs is not None:
            started_at = state.get("started_at") or ""
            epoch = iso_to_epoch(started_at)
            if epoch is None or (now - epoch) > since_secs:
                continue

        # liveness filter
        if liveness_filter:
            matched_live = any(live.startswith(prefix) for prefix in liveness_filter)
            if not matched_live:
                continue

        row = build_row(gr_id_from_state, sf, wdir, state, live)
        rows.append(row)

    rows.sort(key=lambda r: r["started_at"])
    return rows


def do_list(args: argparse.Namespace, here_root: str | None = None) -> None:
    """Default list view."""
    liveness_filter = None
    if args.running or args.dead or args.stalled:
        liveness_filter = set()
        if args.running:
            liveness_filter.add("running")
        if args.dead:
            liveness_filter.add("dead:")
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
        kind_filter=args.kind,
        since_secs=since_secs,
        liveness_filter=liveness_filter,
        include_closed=False,
    )

    # Running gremlins float to the top; within each group, older gremlins
    # appear first by started_at.
    rows.sort(key=lambda r: (r["live_full"] != "running", r["started_at"]))

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
        kind_filter=args.kind,
        since_secs=since_secs,
        liveness_filter={"dead:"},
        include_closed=True,
    )

    for row in rows:
        if row["closed"]:
            row["desc"] = row["desc"][:51] + " [closed]"

    if not rows:
        if here_root is not None:
            print(f"No recent gremlins for project: {here_root}")
        else:
            print("No recent gremlins on this machine.")
        return

    print_table(rows)


def do_drill_in(target: str) -> None:
    """Print every field of a uniquely-matched gremlin in a labeled block."""
    matches = []
    for gr_id, sf, wdir in iter_state_files():
        if target in gr_id:
            matches.append((gr_id, sf, wdir))

    if not matches:
        print(f"no gremlin matched: {target}")
        return
    if len(matches) > 1:
        print(
            f"ambiguous id '{target}' matched {len(matches)} gremlins — use a longer prefix:"
        )
        for gr_id, _, _ in matches:
            print(f"  {gr_id}")
        return

    gr_id, sf, wdir = matches[0]
    state = load_state(sf)
    if not state:
        print(f"error: could not read state for {gr_id}")
        return

    live = liveness_of_state_file(sf)
    started_at = state.get("started_at") or ""
    age = humanize_age(started_at)

    # Convert started_at to local time for display.
    local_start = ""
    epoch = iso_to_epoch(started_at)
    if epoch is not None:
        local_start = datetime.datetime.fromtimestamp(epoch).strftime(
            "%Y-%m-%d %H:%M:%S %Z"
        )

    print(f"gremlin: {gr_id}")
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
    bail_class = state.get("bail_class")
    bail_reason = state.get("bail_reason")
    bail_detail = state.get("bail_detail")
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
    artifact_paths = []
    if os.path.isdir(artifacts_dir):
        for fname in sorted(os.listdir(artifacts_dir)):
            fpath = os.path.join(artifacts_dir, fname)
            if os.path.isfile(fpath):
                artifact_paths.append(fpath)
    rescue_reports = []
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
