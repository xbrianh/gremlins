"""Verify that gremlins/stages/registry.py is a true leaf module."""

from __future__ import annotations

import os
import pathlib
import subprocess
import sys

PROJECT_ROOT = pathlib.Path(__file__).resolve().parent.parent
REGISTRY_PATH = PROJECT_ROOT / "gremlins" / "stages" / "registry.py"


def test_registry_does_not_pull_in_stages_or_pipeline() -> None:
    # Load registry.py directly as a standalone module so Python does not
    # execute stages/__init__.py (which eagerly imports all stage modules).
    script = f"""
import sys
import importlib.util

spec = importlib.util.spec_from_file_location("_registry", r"{REGISTRY_PATH}")
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)

stage_mods = [
    k for k in sys.modules
    if k.startswith("gremlins.stages.") and k not in ("gremlins.stages.registry", "_registry")
]
assert not stage_mods, f"unexpected stage modules: {{stage_mods}}"
assert "gremlins.pipeline" not in sys.modules, "gremlins.pipeline was imported"
print("ok")
"""
    env = os.environ.copy()
    env["PYTHONPATH"] = str(PROJECT_ROOT)
    result = subprocess.run(
        [sys.executable, "-c", script],
        capture_output=True,
        text=True,
        env=env,
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "ok"
