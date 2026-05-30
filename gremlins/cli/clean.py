from __future__ import annotations

import argparse
import os
import pathlib
import shutil
import sys
from dataclasses import dataclass

from gremlins import paths
from gremlins.fleet.land import cleanup_gremlin
from gremlins.fleet.state import liveness_of_state_file, load_state
from gremlins.utils import proc


@dataclass
class CleanItem:
    path: pathlib.Path
    label: str
    size_bytes: int


def _is_pid_alive(pid: int) -> bool:
    if pid <= 0:
        return False
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


def _dir_size(p: pathlib.Path) -> int:
    total = 0
    try:
        for e in os.scandir(p):
            if e.is_dir(follow_symlinks=False):
                total += _dir_size(pathlib.Path(e.path))
            elif e.is_file(follow_symlinks=False):
                total += e.stat().st_size
    except Exception:
        return 0
    return total


def _scan_state(failed: bool, finished: bool) -> list[CleanItem]:
    items: list[CleanItem] = []
    root = paths.state_root()
    if not root.is_dir():
        return items
    entries = list(os.scandir(root))
    for entry in entries:
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
            if not (
                live.startswith("dead:")
                or (isinstance(exit_code, int) and exit_code != 0)
            ):
                continue
        elif finished:
            if not (isinstance(exit_code, int) and exit_code == 0):
                continue
        size = _dir_size(pathlib.Path(entry.path))
        items.append(CleanItem(pathlib.Path(entry.path), name, size))
        prefix = f"{name}--"
        for child in entries:
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
                if not (
                    clive.startswith("dead:") or (isinstance(cexit, int) and cexit != 0)
                ):
                    continue
            elif finished:
                if not (isinstance(cexit, int) and cexit == 0):
                    continue
            csize = _dir_size(pathlib.Path(child.path))
            items.append(CleanItem(pathlib.Path(child.path), child.name, csize))
    return items


def _scan_worktrees() -> list[CleanItem]:
    items: list[CleanItem] = []
    live_workdirs: set[str] = set()
    sroot = paths.state_root()
    if sroot.is_dir():
        for entry in os.scandir(sroot):
            if not entry.is_dir(follow_symlinks=False):
                continue
            sf = pathlib.Path(entry.path) / "state.json"
            if not sf.is_file():
                continue
            live = liveness_of_state_file(str(sf))
            if live == "running" or live.startswith("stalled:"):
                state = load_state(str(sf)) or {}
                w = state.get("workdir") or ""
                if w:
                    live_workdirs.add(str(w))
    wroot = paths.work_root()
    if not wroot.is_dir():
        return items
    for entry in os.scandir(wroot):
        if not entry.is_dir(follow_symlinks=False):
            continue
        wp = pathlib.Path(entry.path)
        if str(wp) not in live_workdirs:
            items.append(CleanItem(wp, entry.name, _dir_size(wp)))
    return items


def _scan_queue(failed: bool, finished: bool) -> list[CleanItem]:
    items: list[CleanItem] = []
    qroot = paths.state_root() / "queues" / "default"
    if not qroot.is_dir():
        return items
    if failed:
        buckets = ["failed"]
    elif finished:
        buckets = ["done"]
    else:
        buckets = ["done", "failed"]
    for sub in buckets:
        bdir = qroot / sub
        if not bdir.is_dir():
            continue
        for e in os.scandir(bdir):
            if e.is_file(follow_symlinks=False) and e.name.endswith(".cmd"):
                p = pathlib.Path(e.path)
                sz = p.stat().st_size if p.exists() else 0
                items.append(CleanItem(p, e.name, sz))
    return items


def _scan_locks() -> list[CleanItem]:
    items: list[CleanItem] = []
    root = paths.state_root()
    if not root.is_dir():
        return items
    for lf in root.rglob("state.json.lock"):
        sf = lf.with_name("state.json")
        if not sf.is_file():
            continue
        try:
            state = load_state(str(sf)) or {}
            pid = state.get("pid")
            if isinstance(pid, (int, str)):
                try:
                    if not _is_pid_alive(int(pid)):
                        sz = lf.stat().st_size
                        items.append(CleanItem(lf, str(lf.relative_to(root)), sz))
                except (ValueError, TypeError):
                    pass
        except Exception:
            pass
    for pf in root.rglob("*.pid"):
        try:
            pid = int(pf.read_text().strip())
            if not _is_pid_alive(pid):
                sz = pf.stat().st_size
                items.append(CleanItem(pf, str(pf.relative_to(root)), sz))
        except Exception:
            pass
    for pf in root.rglob("pid"):
        if pf.name != "pid":
            continue
        try:
            pid = int(pf.read_text().strip())
            if not _is_pid_alive(pid):
                sz = pf.stat().st_size
                items.append(CleanItem(pf, str(pf.relative_to(root)), sz))
        except Exception:
            pass
    return items


def _delete_state(items: list[CleanItem]) -> tuple[int, int]:
    count = 0
    rec = 0
    for it in items:
        gid = it.label
        wdir = str(it.path)
        sf = it.path / "state.json"
        state = load_state(str(sf)) or {}
        pr = str(state.get("project_root") or "")
        cwdg = pr if pr and os.path.isdir(pr) else None
        try:
            cleanup_gremlin(gid, wdir, state, cwdg, delete_branch=True)
            print(f"clean: removed state {gid}")
            rec += it.size_bytes
            count += 1
        except Exception:
            pass
    return count, rec


def _delete_worktrees(items: list[CleanItem]) -> tuple[int, int]:
    count = 0
    rec = 0
    for it in items:
        p = str(it.path)
        try:
            r = proc.run(["git", "worktree", "remove", "--force", p])
            if r.returncode != 0:
                shutil.rmtree(p, ignore_errors=True)
            print(f"clean: removed worktree {it.label}")
            rec += it.size_bytes
            count += 1
        except Exception:
            try:
                shutil.rmtree(p, ignore_errors=True)
                print(f"clean: removed worktree {it.label}")
                rec += it.size_bytes
                count += 1
            except Exception:
                pass
    return count, rec


def _delete_queue(items: list[CleanItem]) -> tuple[int, int]:
    count = 0
    rec = 0
    for it in items:
        try:
            it.path.unlink()
            print(f"clean: removed queue {it.label}")
            rec += it.size_bytes
            count += 1
        except Exception:
            pass
    return count, rec


def _delete_locks(items: list[CleanItem]) -> tuple[int, int]:
    count = 0
    rec = 0
    for it in items:
        try:
            it.path.unlink()
            print(f"clean: removed lock {it.label}")
            rec += it.size_bytes
            count += 1
        except Exception:
            pass
    return count, rec


def clean_main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="gremlins clean")
    parser.add_argument("--worktrees", action="store_true")
    parser.add_argument("--state", action="store_true")
    parser.add_argument("--queue", action="store_true")
    parser.add_argument("--locks", action="store_true")
    parser.add_argument("--all", action="store_true")
    parser.add_argument("--failed", action="store_true")
    parser.add_argument("--finished", action="store_true")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--yes", "-y", action="store_true")
    args = parser.parse_args(argv)
    if args.failed and args.finished:
        print("error: --failed and --finished are mutually exclusive", file=sys.stderr)
        return 1
    no_cats = not (args.worktrees or args.state or args.queue or args.locks or args.all)
    do_work = args.worktrees or args.all or no_cats
    do_state = args.state or args.all or no_cats
    do_queue = args.queue or args.all or no_cats
    do_locks = args.locks or args.all or no_cats
    state_items = _scan_state(args.failed, args.finished) if do_state else []
    work_items = _scan_worktrees() if do_work else []
    queue_items = _scan_queue(args.failed, args.finished) if do_queue else []
    lock_items = _scan_locks() if do_locks else []
    groups = {
        "worktrees": work_items,
        "state": state_items,
        "queue": queue_items,
        "locks": lock_items,
    }
    total_n = sum(len(v) for v in groups.values())
    if total_n == 0:
        print("nothing to clean")
        return 0
    for cat in ("worktrees", "state", "queue", "locks"):
        its = groups[cat]
        if its:
            n = len(its)
            b = sum(i.size_bytes for i in its)
            print(f"{cat}: {n} items, {b} bytes")
    if no_cats or args.dry_run:
        return 0
    if not args.yes:
        try:
            if input("Delete? [y/N] ").strip().lower() != "y":
                return 0
        except EOFError:
            return 0
    cw, rw = _delete_worktrees(work_items) if do_work else (0, 0)
    cs, rs = _delete_state(state_items) if do_state else (0, 0)
    cq, rq = _delete_queue(queue_items) if do_queue else (0, 0)
    cl, rl = _delete_locks(lock_items) if do_locks else (0, 0)
    print(f"reclaimed: {cw + cs + cq + cl} items, {rw + rs + rq + rl} bytes")
    return 0
