import os
import subprocess

def run_ok(cmd: list[str], *, cwd: str | os.PathLike[str] | None = None) -> bool: ...
def run(
    cmd: list[str],
    *,
    cwd: str | os.PathLike[str] | None = None,
    check: bool = False,
    text: bool = True,
    timeout: float | None = None,
) -> subprocess.CompletedProcess[str]: ...
