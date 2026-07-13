from os import PathLike
from subprocess import CompletedProcess
from typing import Any

def sum(a: int, b: int) -> int: ...
def __version__() -> str: ...
def run(
    cmd: list[str],
    *,
    cwd: str | PathLike[str] | None = None,
    check: bool = False,
    text: bool = True,
    timeout: float | None = None,
) -> CompletedProcess[str]: ...
def run_ok(cmd: list[str], *, cwd: str | PathLike[str] | None = None) -> bool: ...
def run_quiet(
    cmd: list[str], *, cwd: str | PathLike[str] | None = None
) -> CompletedProcess[str]: ...
def run_or_raise(cmd: list[str], *, cwd: str | PathLike[str] | None = None) -> str: ...
def _run_async(
    cmd: list[str],
    *,
    cwd: str | PathLike[str] | None = None,
    check: bool = False,
    text: bool = True,
    timeout: float | None = None,
) -> Any: ...
def _run_shell_async(
    cmd: str,
    *,
    cwd: str | PathLike[str] | None = None,
    env: dict[str, str] | None = None,
    timeout: float | None = None,
) -> Any: ...
def _run_ok_async(cmd: list[str], *, cwd: str | PathLike[str] | None = None) -> Any: ...
def _run_quiet_async(
    cmd: list[str], *, cwd: str | PathLike[str] | None = None
) -> Any: ...
def _run_or_raise_async(
    cmd: list[str], *, cwd: str | PathLike[str] | None = None
) -> Any: ...
def _terminate_with_grace(pid: int, *, grace_s: float = 10.0) -> Any: ...
