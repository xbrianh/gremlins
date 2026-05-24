"""Verify child_sandbox fixture: share and fresh subprocess environments."""

from __future__ import annotations

import subprocess
import sys

import pytest


def test_share_env_has_parent_sandbox_root(child_sandbox, sandbox):
    env = child_sandbox.share()
    assert env["GREMLINS_SANDBOX_ROOT"] == str(sandbox.root)
    assert env["HOME"] == str(sandbox.home)


def test_fresh_env_has_separate_sandbox_root(child_sandbox, sandbox):
    cs = child_sandbox.fresh()
    assert cs.env["GREMLINS_SANDBOX_ROOT"] != str(sandbox.root)
    assert cs.env["HOME"] != str(sandbox.home)
    assert cs.state.exists()
    assert cs.work.exists()
    assert cs.config.exists()
    assert cs.home.exists()
    assert cs.project.exists()


def test_fresh_child_writes_stay_in_child_sandbox(child_sandbox, sandbox):
    """Child in fresh mode must not write to the parent sandbox."""
    cs = child_sandbox.fresh()
    subprocess.run(
        [
            sys.executable,
            "-c",
            "import gremlins.paths; (gremlins.paths.state_root() / 'sentinel.txt').write_text('x')",
        ],
        env=cs.env,
        check=True,
    )
    assert (cs.state / "sentinel.txt").exists(), (
        "child should write to its own state dir"
    )
    assert not (sandbox.state / "sentinel.txt").exists(), (
        "child must not pollute parent sandbox"
    )


def test_share_child_writes_go_to_parent_sandbox(child_sandbox, sandbox):
    """Child in share mode writes to the parent sandbox."""
    env = child_sandbox.share()
    subprocess.run(
        [
            sys.executable,
            "-c",
            "import gremlins.paths; (gremlins.paths.state_root() / 'shared_sentinel.txt').write_text('x')",
        ],
        env=env,
        check=True,
    )
    assert (sandbox.state / "shared_sentinel.txt").exists(), (
        "child should write to parent state dir"
    )
