from __future__ import annotations

import os
import pathlib
import subprocess


def load_env_file(path: pathlib.Path) -> dict[str, str]:
    before = dict(os.environ)
    result = subprocess.run(
        ["bash", "-c", 'source "$1" && env -0', "--", str(path)],
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"failed to source {path}:\n{result.stderr.decode(errors='replace').strip()}"
        )
    after: dict[str, str] = {}
    for entry in result.stdout.split(b"\0"):
        decoded = entry.decode(errors="replace")
        if "=" in decoded:
            k, _, v = decoded.partition("=")
            after[k] = v
    return {k: v for k, v in after.items() if before.get(k) != v}
