"""Queue business logic for sequential gremlin dispatch."""

from __future__ import annotations

import json
import re
import shlex
import subprocess
import sys
import time
from datetime import datetime
from pathlib import Path

_TERMINAL_STATUSES = frozenset({"done", "dead", "bailed"})
_ID_RE = re.compile(r"^[a-z0-9][a-z0-9-]+$")
_SUBDIRS = ("pending", "running", "done", "failed")
_POLL_TIMEOUT = 4 * 3600  # 4 hours; a gremlin silent longer than this is assumed dead


def queue_root() -> Path:
    from gremlins.paths import state_root

    root = state_root() / "queues" / "default"
    for sub in _SUBDIRS:
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


def _strip_id(stem: str) -> str:
    parts = stem.split(".")
    if len(parts) >= 2 and _ID_RE.match(parts[-1]):
        return ".".join(parts[:-1])
    return stem


def _is_launch(cmd: str) -> bool:
    tokens = cmd.split()
    for i, t in enumerate(tokens):
        if t == "gremlins" and i + 1 < len(tokens) and tokens[i + 1] == "launch":
            return True
    return False


def _move_item(cmd_path: Path, dst_dir: Path) -> Path:
    dst = dst_dir / cmd_path.name
    cmd_path.rename(dst)
    log = cmd_path.with_suffix(".log")
    if log.exists():
        log.rename(dst_dir / log.name)
    return dst


def _poll_terminal(gremlin_id: str) -> dict[str, object]:
    from gremlins.paths import state_root

    state_file = state_root() / gremlin_id / "state.json"
    deadline = time.time() + _POLL_TIMEOUT
    while True:
        if time.time() > deadline:
            raise TimeoutError(
                f"gremlin {gremlin_id} did not reach terminal status within"
                f" {_POLL_TIMEOUT // 3600}h"
            )
        try:
            data = json.loads(state_file.read_text())
            if data.get("status") in _TERMINAL_STATUSES:
                return data
        except (FileNotFoundError, json.JSONDecodeError):
            pass
        time.sleep(2)


def _run_launch(cmd: str, log_path: Path) -> tuple[str | None, bool]:
    full_cmd = cmd + " --print-id-only"
    with open(log_path, "w") as log_f:
        proc = subprocess.run(
            full_cmd,
            shell=True,
            stdout=subprocess.PIPE,
            stderr=log_f,
        )
        stdout = proc.stdout.decode().strip()
        log_f.write(stdout + "\n")
    ok = proc.returncode == 0 and bool(stdout)
    return (stdout if stdout else None, ok)


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
    for sub in _SUBDIRS:
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


def run() -> int:
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

    while True:
        pending = sorted((root / "pending").glob("*.cmd"))
        if not pending:
            return 0

        src = pending[0]
        cmd = src.read_text().strip()
        item = _move_item(src, root / "running")
        log_path = item.with_suffix(".log")

        print(f"queue: running {item.stem}", flush=True)

        if _is_launch(cmd):
            gremlin_id, proc_ok = _run_launch(cmd, log_path)
            if not proc_ok or gremlin_id is None:
                _move_item(item, root / "failed")
                print(f"queue: failed {item.stem}", file=sys.stderr)
                return 1

            if not _ID_RE.match(gremlin_id):
                _move_item(item, root / "failed")
                print(
                    f"queue: failed {item.stem} (invalid gremlin id: {gremlin_id!r})",
                    file=sys.stderr,
                )
                return 1

            base_stem = _strip_id(item.stem)
            new_stem = base_stem + "." + gremlin_id
            new_item = item.parent / (new_stem + ".cmd")
            item.rename(new_item)
            if log_path.exists():
                new_log = item.parent / (new_stem + ".log")
                log_path.rename(new_log)
                log_path = new_log
            item = new_item

            print(f"queue: waiting for gremlin {gremlin_id}", flush=True)
            try:
                state = _poll_terminal(gremlin_id)
            except TimeoutError as e:
                _move_item(item, root / "failed")
                print(f"queue: failed {item.stem}: {e}", file=sys.stderr)
                return 1
            clean = state.get("exit_code") == 0 and "bail_class" not in state
        else:
            clean = _run_plain(cmd, log_path)

        if clean:
            _move_item(item, root / "done")
            print(f"queue: done {item.stem}", flush=True)
        else:
            _move_item(item, root / "failed")
            print(f"queue: failed {item.stem}", file=sys.stderr)
            return 1


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
    matches = [(sub, root / sub / (stem + ".cmd")) for sub in _SUBDIRS]
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
        for sub in _SUBDIRS:
            if sub == "running":
                for p in (root / sub).glob("*.cmd"):
                    gremlin_id = _parse_id(p)
                    if gremlin_id:
                        subprocess.run(["gremlins", "stop", gremlin_id])
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


def land() -> int:
    root = queue_root()
    landed = 0
    skipped = 0
    for p in sorted((root / "done").glob("*.cmd")):
        gremlin_id = _parse_id(p)
        if gremlin_id is None:
            skipped += 1
            continue
        result = subprocess.run(["gremlins", "land", gremlin_id])
        if result.returncode != 0:
            print(f"queue land: failed on {gremlin_id}", file=sys.stderr)
            return 1
        p.unlink()
        log = p.with_suffix(".log")
        if log.exists():
            log.unlink()
        landed += 1
    print(f"queue land: landed {landed}, skipped {skipped}", flush=True)
    return 0
