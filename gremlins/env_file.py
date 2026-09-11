from __future__ import annotations

import os
import pathlib
import subprocess
import tempfile

# Variables set by bash itself that are not meaningful to propagate.
_BASH_INTERNALS = frozenset(
    {
        "_",
        "BASH",
        "BASH_VERSION",
        "BASH_VERSINFO",
        "BASHOPTS",
        "BASHPID",
        "PPID",
        "SHLVL",
        "SHELLOPTS",
    }
)


def load_env_file_isolated(
    path: pathlib.Path,
    *,
    base_env: dict[str, str],
    cwd: pathlib.Path | None = None,
) -> dict[str, str]:
    """Source `path` in a controlled bash environment and return every env var.

    `base_env` is the environment the bash subprocess receives.
    The caller constructs it — typically the full parent env plus system vars,
    so the script has complete control over what to keep or override.
    The caller re-injects system vars afterward so users cannot tamper.
    """
    # Strip BASH_ENV so bash doesn't auto-source an unrelated file.
    _env = {k: v for k, v in base_env.items() if k != "BASH_ENV"}
    try:
        result = subprocess.run(
            ["bash", "-c", 'source "$1" >/dev/null && env -0', "--", str(path)],
            capture_output=True,
            check=False,
            env=_env,
            cwd=cwd,
        )
    except FileNotFoundError:
        raise RuntimeError(f"failed to source {path}: bash not found")
    if result.returncode != 0:
        raise RuntimeError(
            f"failed to source {path}:\n{result.stderr.decode(errors='replace').strip()}"
        )
    env: dict[str, str] = {}
    for entry in result.stdout.split(b"\0"):
        decoded = entry.decode(errors="replace")
        if "=" in decoded:
            k, _, v = decoded.partition("=")
            env[k] = v
    # Strip bash internal vars.
    for key in _BASH_INTERNALS:
        env.pop(key, None)
    return env


def source_env_string(
    script: str, base_env: dict[str, str], *, cwd: pathlib.Path | None = None
) -> dict[str, str]:
    """Source a bash script string, return the resulting environment dict."""
    with tempfile.NamedTemporaryFile(
        mode="w", suffix=".env.sh", delete=False, encoding="utf-8"
    ) as tf:
        tf.write(script)
        tf.flush()
        temp_path = tf.name
    try:
        return load_env_file_isolated(
            pathlib.Path(temp_path), base_env=base_env, cwd=cwd
        )
    finally:
        try:
            os.unlink(temp_path)
        except OSError:
            pass
