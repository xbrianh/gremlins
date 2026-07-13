import os
from collections.abc import Sequence

def run_ok(cmd: Sequence[str], cwd: str | os.PathLike[str] | None = None) -> bool:
    ...

def __version__() -> str:
    """Return the version of the native extension."""
    ...
