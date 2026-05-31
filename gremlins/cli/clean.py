from __future__ import annotations

import argparse
import os
import pathlib
from dataclasses import dataclass

from gremlins import paths
from gremlins.fleet.land import cleanup_gremlin
from gremlins.fleet.state import liveness_of_state_file, load_state


@dataclass
class CleanItem:
    path: pathlib.Path
    label: str
    size_bytes: int


def _dir_size(path: pathlib.Path) -> int:
    total = 0
    try:
        for entry in os.scandir(path):
            if entry.is_dir(follow_symlinks=False):
                total += _dir_size(pathlib.Path(entry.path))
            elif entry.is_file(follow_symlinks=False):
                try:
                    total += entry.stat().st_size
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
    candidates: list[tuple[str, pathlib.Path, int]] = []
    for entry in os.scandir(root):
        if not entry.is_dir():
            continue
        name = entry.name
        wdir = pathlib.Path(entry.path)
        sf = wdir / "state.json"
        if not sf.is_file():
            continue
        live = liveness_of_state_file(str(sf))
        if (
            live == "running"
            or live.startswith("stalled:")
            or live.startswith("waiting")
        ):
            continue
        state = load_state(str(sf)) or {}
        exit_code = state.get("exit_code")
        include = True
        if failed:
            if not (
                live.startswith("dead:") or (exit_code is not None and exit_code != 0)
            ):
                include = False
        if finished:
            if not (live == "finished" or (exit_code is not None and exit_code == 0)):
                include = False
        if not include:
            continue
        size = _dir_size(wdir)
        candidates.append((name, wdir, size))
    for name, wdir, size in candidates:
        if "--" in name:
            continue
        items.append(CleanItem(wdir, name, size))
        prefix = name + "--"
        for cname, cwdir, csize in candidates:
            if cname.startswith(prefix):
                items.append(CleanItem(cwdir, cname, csize))
    return items


def _print_summary(cat: str, items: list[CleanItem]) -> None:
    if not items:
        return
    n = len(items)
    b = sum(i.size_bytes for i in items)
    print(f"{cat}: {n} ({b} bytes)")


def clean_main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="gremlins clean")
    parser.add_argument("--state", action="store_true")
    parser.add_argument("--all", action="store_true")
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--failed", action="store_true")
    group.add_argument("--finished", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--yes", "-y", action="store_true")
    args = parser.parse_args(argv)
    if args.failed or args.finished:
        items = _scan_state(args.failed, args.finished)
    else:
        items = _scan_state()
    _print_summary("state", items)
    if not items:
        print("nothing to clean")
        return 0
    if not (args.state or args.all or args.failed or args.finished):
        return 0
    if args.dry_run:
        return 0
    if not args.yes:
        try:
            if input("Delete? [y/N] ").strip().lower() not in {"y", "yes"}:
                return 0
        except (EOFError, KeyboardInterrupt):
            return 0
    reclaimed = 0
    deleted: list[CleanItem] = []
    for item in items:
        try:
            state_file = str(item.path / "state.json")
            state = load_state(state_file) or {}
            project_root = str(state.get("project_root") or "")
            cwd_for_git = (
                project_root if project_root and os.path.isdir(project_root) else None
            )
            cleanup_gremlin(
                item.label, str(item.path), state, cwd_for_git, delete_branch=True
            )
            if not item.path.exists():
                print(f"removed {item.label}")
                reclaimed += item.size_bytes
                deleted.append(item)
            else:
                print(f"failed to remove {item.label}")
        except Exception as exc:
            print(f"failed to remove {item.label}: {exc}")
            continue
    _print_summary("state", deleted)
    print(f"reclaimed {reclaimed} bytes")
    return 0
