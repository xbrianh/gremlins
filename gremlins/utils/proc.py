from __future__ import annotations

import subprocess


def run(
    cmd: list[str],
    *,
    cwd: str | None = None,
    check: bool = False,
    text: bool = True,
    timeout: float | None = None,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        cmd, cwd=cwd, capture_output=True, text=text, check=check, timeout=timeout
    )


def run_ok(cmd: list[str], *, cwd: str | None = None) -> bool:
    r = subprocess.run(cmd, cwd=cwd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return r.returncode == 0


def run_quiet(cmd: list[str], *, cwd: str | None = None) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(cmd, cwd=cwd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def run_or_raise(cmd: list[str], *, cwd: str | None = None) -> str:
    r = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, check=True)
    return r.stdout.strip()
