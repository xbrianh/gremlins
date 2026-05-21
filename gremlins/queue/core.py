"""Queue business logic for sequential gremlin dispatch."""

from __future__ import annotations

import json
import re
import shlex
import signal
import subprocess
import sys
import threading
import time
from datetime import datetime
from pathlib import Path

_ID_RE = re.compile(r"^[A-Za-z0-9_-]+$")
SUBDIRS = ("pending", "running", "done", "failed")


def queue_root() -> Path:
    from gremlins.paths import state_root

    root = state_root() / "queues" / "default"
    for sub in SUBDIRS:
        (root / sub).mkdir(parents=True, exist_ok=True)
    return root


def _slugify(text: str) -> str:
    slug = re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")
    return slug[:24]


def _slug_token(tokens: list[str]) -> str:
    for i, t in enumerate(tokens):
        if t == "gremlins" and i + 1 < len(tokens) and tokens[i + 1] == "launch":
            rest = tokens[i + 2 :]
            break
    else:
        rest = tokens
    for t in rest:
        if not t.startswith("-"):
            return t
    return "item"


def _cmd_description(cmd: str) -> str:
    try:
        tokens = shlex.split(cmd)
    except ValueError:
        return ""
    for i, t in enumerate(tokens):
        if t == "--description" and i + 1 < len(tokens):
            return tokens[i + 1]
        if t.startswith("--description="):
            return t[len("--description=") :]
    return ""


def _parse_id(path: Path) -> str | None:
    parts = path.stem.split(".")
    if len(parts) < 2:
        return None
    candidate = parts[-1]
    return candidate if _ID_RE.match(candidate) else None


def _move_item(cmd_path: Path, dst_dir: Path) -> Path:
    dst = dst_dir / cmd_path.name
    cmd_path.rename(dst)
    log = cmd_path.with_suffix(".log")
    if log.exists():
        log.rename(dst_dir / log.name)
    return dst


def _run_plain(cmd: str, log_path: Path) -> bool:
    with open(log_path, "w") as log_f:
        proc = subprocess.run(cmd, shell=True, stdout=log_f, stderr=subprocess.STDOUT)
    return proc.returncode == 0


def add(command: str) -> str:
    root = queue_root()
    tokens = command.split()
    slug = _slugify(_slug_token(tokens))
    while True:
        name = f"{datetime.now().strftime('%Y%m%dT%H%M%S_%f')}-{slug}.cmd"
        try:
            with (root / "pending" / name).open("x") as f:
                f.write(command)
            return name
        except FileExistsError:
            continue


def list_queue() -> int:
    root = queue_root()
    entries: list[tuple[Path, str]] = []
    for sub in SUBDIRS:
        for p in (root / sub).glob("*.cmd"):
            entries.append((p, sub))
    if not entries:
        print("(queue is empty)")
        return 0
    entries.sort(key=lambda e: e[0].name, reverse=True)
    for p, sub in entries:
        gremlin_id = _parse_id(p)
        id_str = f" [{gremlin_id}]" if gremlin_id else ""
        desc = _cmd_description(p.read_text().strip())
        desc_str = f"  {desc}" if desc else ""
        print(f"{sub:8s}  {p.stem}{id_str}{desc_str}")
    return 0


def list_queue_json() -> int:
    root = queue_root()
    items: list[dict[str, object]] = []
    for sub in SUBDIRS:
        for p in (root / sub).glob("*.cmd"):
            cmd = p.read_text().strip()
            items.append(
                {
                    "bucket": sub,
                    "stem": p.stem,
                    "gremlin_id": _parse_id(p),
                    "description": _cmd_description(cmd),
                    "cmd": cmd,
                }
            )
    items.sort(key=lambda d: str(d["stem"]), reverse=True)
    print(json.dumps(items, indent=2))
    return 0


def run(
    once: bool = False,
    poll_interval: float = 1.0,
    _stop_event: threading.Event | None = None,
) -> int:
    root = queue_root()
    running = sorted((root / "running").glob("*.cmd"))
    if running:
        names = ", ".join(p.name for p in running)
        print(
            f"queue: error: running/ has stale items: {names}\n"
            "hint: move or remove them manually before running the queue.",
            file=sys.stderr,
        )
        return 1

    _stopped = False

    def _handle_signal(sig: int, frame: object) -> None:
        nonlocal _stopped
        _stopped = True

    def _should_stop() -> bool:
        return _stopped or (_stop_event is not None and _stop_event.is_set())

    on_main = threading.current_thread() is threading.main_thread()
    if on_main:
        old_int = signal.signal(signal.SIGINT, _handle_signal)
        old_term = signal.signal(signal.SIGTERM, _handle_signal)

    try:
        while True:
            pending = sorted((root / "pending").glob("*.cmd"))
            if not pending:
                if once or _should_stop():
                    return 0
                if _stop_event is not None:
                    _stop_event.wait(timeout=poll_interval)
                else:
                    time.sleep(poll_interval)
                continue

            if _should_stop():
                return 0

            src = pending[0]
            cmd = src.read_text().strip()
            item = _move_item(src, root / "running")
            log_path = item.with_suffix(".log")

            print(f"queue: running {item.stem}", flush=True)

            clean = _run_plain(cmd, log_path)

            if _should_stop():
                _move_item(item, root / "done" if clean else root / "failed")
                return 0

            if clean:
                _move_item(item, root / "done")
                print(f"queue: done {item.stem}", flush=True)
            else:
                _move_item(item, root / "failed")
                print(f"queue: failed {item.stem}", file=sys.stderr)
                return 1
    finally:
        if on_main:
            signal.signal(signal.SIGINT, old_int)  # type: ignore[reportPossiblyUnbound]
            signal.signal(signal.SIGTERM, old_term)  # type: ignore[reportPossiblyUnbound]


def requeue(include_done: bool = False) -> int:
    root = queue_root()
    buckets = ["failed"]
    if include_done:
        buckets.append("done")
    for sub in buckets:
        for p in sorted((root / sub).glob("*.cmd")):
            _move_item(p, root / "pending")
    return 0


def _delete_dir_contents(root: Path, sub: str) -> None:
    for p in (root / sub).glob("*.cmd"):
        p.unlink()
        log = p.with_suffix(".log")
        if log.exists():
            log.unlink()


def _clear_item(root: Path, stem: str) -> int:
    matches = [(sub, root / sub / (stem + ".cmd")) for sub in SUBDIRS]
    matches = [(sub, p) for sub, p in matches if p.exists()]

    if not matches:
        print(f"no such item: {stem}", file=sys.stderr)
        return 1

    if len(matches) > 1:
        locs = ", ".join(sub for sub, _ in matches)
        print(f"item {stem!r} found in multiple directories: {locs}", file=sys.stderr)
        return 1

    sub, p = matches[0]
    if sub == "running":
        print(
            f"item {stem!r} is running; use 'gremlins queue clear --purge' to stop running gremlins",
            file=sys.stderr,
        )
        return 1

    p.unlink()
    log = p.with_suffix(".log")
    if log.exists():
        log.unlink()
    return 0


def clear(
    failed_only: bool = False,
    done_only: bool = False,
    pending_only: bool = False,
    purge: bool = False,
    item: str | None = None,
) -> int:
    root = queue_root()
    if item is not None:
        if any([failed_only, done_only, pending_only, purge]):
            print(
                "--item is mutually exclusive with --failed, --done, --pending, --purge",
                file=sys.stderr,
            )
            return 1
        return _clear_item(root, item)
    if purge:
        for sub in SUBDIRS:
            _delete_dir_contents(root, sub)
        return 0

    if failed_only:
        buckets = ["failed"]
    elif done_only:
        buckets = ["done"]
    elif pending_only:
        buckets = ["pending"]
    else:
        buckets = ["done", "failed"]

    for sub in buckets:
        _delete_dir_contents(root, sub)
    return 0


def set_state(item: str, state: str) -> int:
    if state not in SUBDIRS:
        print(
            f"invalid state: {state!r}; must be one of: {', '.join(SUBDIRS)}",
            file=sys.stderr,
        )
        return 1
    root = queue_root()
    matches = [(sub, root / sub / (item + ".cmd")) for sub in SUBDIRS]
    matches = [(sub, p) for sub, p in matches if p.exists()]
    if not matches:
        print(f"no such item: {item}", file=sys.stderr)
        return 1
    if len(matches) > 1:
        locs = ", ".join(sub for sub, _ in matches)
        print(f"item {item!r} found in multiple directories: {locs}", file=sys.stderr)
        return 1
    current, cmd_path = matches[0]
    if current == state:
        print(f"item {item!r} is already in {state!r}", file=sys.stderr)
        return 1
    _move_item(cmd_path, root / state)
    return 0
