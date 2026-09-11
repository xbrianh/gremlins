"""Helpers for shell integration tests.

Each test gets its own isolated HOME, PATH, and a real git
repo.
"""

from __future__ import annotations

import pathlib
import sys


def install_fake_bin(bin_dir: pathlib.Path, name: str, target: pathlib.Path) -> None:
    if " " in sys.executable:
        raise RuntimeError(
            f"sys.executable contains spaces; POSIX shebang lines have no quoting mechanism "
            f"and will fail to exec. Move Python to a path without spaces: {sys.executable!r}"
        )
    bin_dir.mkdir(parents=True, exist_ok=True)
    wrapper = bin_dir / name
    wrapper.write_text(
        f"#!{sys.executable}\nimport runpy, sys\nrunpy.run_path({str(target)!r}, run_name='__main__')\n",
        encoding="utf-8",
    )
    wrapper.chmod(0o755)
