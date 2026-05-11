from __future__ import annotations

import fcntl
import json
import os
import pathlib
import secrets
from collections.abc import Callable
from typing import Any


def locked_update(sf: pathlib.Path, fn: Callable[[dict[str, Any]], None]) -> None:
    """Acquire an exclusive lock on sf.lock, read sf, apply fn(data), write sf atomically."""
    lock_path = sf.with_name(sf.name + ".lock")
    with open(lock_path, "a") as lock_f:
        fcntl.flock(lock_f, fcntl.LOCK_EX)
        data = json.loads(sf.read_text(encoding="utf-8"))
        fn(data)
        tmp = sf.with_name(f"{sf.name}.{os.getpid()}.{secrets.token_hex(8)}.tmp")
        tmp.write_text(json.dumps(data), encoding="utf-8")
        os.replace(tmp, sf)
