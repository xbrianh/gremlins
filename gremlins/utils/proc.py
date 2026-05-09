from __future__ import annotations

import os
import subprocess


def run(
    cmd: list[str],
    *,
    cwd: str | os.PathLike[str] | None = None,
    check: bool = False,
    text: bool = True,
    timeout: float | None = None,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        cmd, cwd=cwd, capture_output=True, text=text, check=check, timeout=timeout
    )


def run_ok(cmd: list[str], *, cwd: str | os.PathLike[str] | None = None) -> bool:
    r = subprocess.run(
        cmd, cwd=cwd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
    )
    return r.returncode == 0


def run_quiet(cmd: list[str], *, cwd: str | os.PathLike[str] | None = None) -> int:
    r = subprocess.run(cmd, cwd=cwd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return r.returncode


def run_or_raise(cmd: list[str], *, cwd: str | os.PathLike[str] | None = None) -> str:
    r = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, check=True)
    return r.stdout.strip()
