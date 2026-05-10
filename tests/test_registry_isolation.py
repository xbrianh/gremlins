"""Verify that gremlins/stages/registry.py is a true leaf module."""

from __future__ import annotations

import os
import pathlib
import subprocess
import sys

PROJECT_ROOT = pathlib.Path(__file__).resolve().parent.parent


def test_registry_is_a_leaf_module() -> None:
    # Verify that importing gremlins.stages.registry does NOT pull in concrete
    # stage implementations — only the leaf modules registry, stages, and base
    # should appear in sys.modules under gremlins.stages.*.
    script = """
import sys
import importlib

importlib.import_module("gremlins.stages.registry")

stage_mods = [
    k for k in sys.modules
    if k.startswith("gremlins.stages.")
    and k not in (
        "gremlins.stages.registry",
        "gremlins.stages",
        "gremlins.stages.base",
        "gremlins.stages.compound",
        "gremlins.stages.loop",
    )
]
assert not stage_mods, f"unexpected stage modules: {stage_mods}"
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
