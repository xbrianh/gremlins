from __future__ import annotations

import pathlib
import subprocess


def _spawn_logged_process(
    cmd: list[str],
    cwd: str,
    env: dict[str, str],
    log_path: pathlib.Path,
    log_mode: str = "w",
) -> subprocess.Popen[bytes]:
    """Popen with stdout+stderr to log_path in a new session."""
    log_fh = open(log_path, log_mode)
    try:
        proc = subprocess.Popen(
            cmd,
            cwd=cwd,
            stdin=subprocess.DEVNULL,
            stdout=log_fh,
            stderr=log_fh,
            start_new_session=True,
            env=env,
        )
    finally:
        log_fh.close()
    return proc
