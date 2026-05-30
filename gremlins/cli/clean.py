from __future__ import annotations

import argparse
import os
import pathlib
import sys
from dataclasses import dataclass

from gremlins import paths
from gremlins.fleet.land import cleanup_gremlin
from gremlins.fleet.state import liveness_of_state_file, load_state


@dataclass
class CleanItem:
    path: pathlib.Path
    label: str
    size_bytes: int


def _dir_size(p: pathlib.Path) -> int:
    total = 0
    try:
        for e in os.scandir(p):
            try:
                if e.is_dir(follow_symlinks=False):
                    total += _dir_size(pathlib.Path(e.path))
                else:
                    total += e.stat().st_size
            except OSError:
                pass
    except OSError:
        return 0
    return total


def _scan_state(failed: bool = False, finished: bool = False) -> list[CleanItem]:
    items: list[CleanItem] = []
    root = paths.state_root()
    if not root.is_dir():
        return items
    for entry in os.scandir(root):
        if not entry.is_dir(follow_symlinks=False):
            continue
        name = entry.name
        if "--" in name:
            continue
        sf = pathlib.Path(entry.path) / "state.json"
        if not sf.is_file():
            continue
        live = liveness_of_state_file(str(sf))
        if live == "running" or live.startswith("stalled:"):
            continue
        state = load_state(str(sf)) or {}
        exit_code = state.get("exit_code")
        if failed:
            is_failed = live.startswith("dead:") or (
                exit_code is not None and exit_code != 0 and exit_code != "null"
            )
            if not is_failed:
                continue
        size = _dir_size(pathlib.Path(entry.path))
        items.append(CleanItem(pathlib.Path(entry.path), name, size))
        prefix = f"{name}--"
        for child in os.scandir(root):
            if not child.name.startswith(prefix):
                continue
            csf = pathlib.Path(child.path) / "state.json"
            if not csf.is_file():
                continue
            clive = liveness_of_state_file(str(csf))
            if clive == "running" or clive.startswith("stalled:"):
                continue
            cstate = load_state(str(csf)) or {}
            cexit = cstate.get("exit_code")
            if failed:
                cif = clive.startswith("dead:") or (
                    cexit is not None and cexit != 0 and cexit != "null"
                )
                if not cif:
                    continue
            csize = _dir_size(pathlib.Path(child.path))
            items.append(CleanItem(pathlib.Path(child.path), child.name, csize))
    return items


def clean_main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="gremlins clean")
    parser.add_argument("--state", action="store_true")
    parser.add_argument("--all", action="store_true")
    parser.add_argument("--failed", action="store_true")
    parser.add_argument("--finished", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--yes", "-y", dest="assume_yes", action="store_true")
    args = parser.parse_args(argv)
    if args.failed and args.finished:
        print("error: --failed and --finished mutually exclusive", file=sys.stderr)
        return 1
    cats: dict[str, list[CleanItem]] = {}
    if args.state or args.all:
        cats["state"] = _scan_state(args.failed, args.finished)
    else:
        cats["state"] = _scan_state()
    if not any(cats.values()):
        print("nothing to clean")
        return 0
    for cat, its in sorted(cats.items()):
        n = len(its)
        b = sum(i.size_bytes for i in its)
        print(f"{cat}: {n} items, {b} bytes")
    if args.dry_run:
        return 0
    if not args.assume_yes:
        try:
            ans = input("Delete? [y/N] ").strip().lower()
            if ans not in ("y", "yes"):
                return 0
        except (EOFError, KeyboardInterrupt):
            return 0
    total_reclaimed = 0
    for cat, its in cats.items():
        if not its:
            continue
        rcount = 0
        crec = 0
        for item in its:
            gid = item.label
            wdir = str(item.path)
            sf = str(item.path / "state.json")
            st = load_state(sf) or {}
            pr = str(st.get("project_root") or "")
            cg = pr if pr and os.path.isdir(pr) else None
            try:
                cleanup_gremlin(gid, wdir, st, cg, delete_branch=True)
                print(f"clean: {gid} removed")
                rcount += 1
                crec += item.size_bytes
            except Exception:
                continue
        print(f"{cat}: {rcount} removed, {crec} bytes")
        total_reclaimed += crec
    print(f"total: {total_reclaimed} bytes reclaimed")
    return 0
