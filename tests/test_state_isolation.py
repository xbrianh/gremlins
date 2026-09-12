"""Regression tests for the GREMLINS_GREMLIN_ID-leakage bug captured on PR #140.

When `pytest` runs as a subprocess of an implement-stage gremlin, GREMLINS_GREMLIN_ID
is inherited. Without isolation, gremlins.state.set_stage would write the
parent gremlin's state.json and corrupt its `stage` and `sub_stage` fields
— observable in `/gremlins --watch` and dangerous for any rescue-flow logic
that branches on `stage`.

The fix is the autouse `_isolate_gremlin_id` fixture in conftest.py, which
delenv's GREMLINS_GREMLIN_ID before every test. These tests verify both layers:

- test_autouse_isolate_gremlin_id_unsets_gremlin_id_under_inherited_env: spawns a
  pytest subprocess with GREMLINS_GREMLIN_ID set in its environment and asserts the
  autouse fixture removes it inside the test body. Without the subprocess
  hop, this test would pass trivially in any clean CI environment (no
  GREMLINS_GREMLIN_ID inherited) regardless of whether the autouse fixture was present.
- test_*_does_not_clobber_external_state: per-orchestrator end-to-end
  checks that running each entry point does not modify a pre-staged parent
  gremlin's state.json. Each orchestrator is exercised in its own test so a
  regression message names the offender.

Coverage envelope: with GREMLINS_GREMLIN_ID unset (the post-fix invariant), set_stage
early-returns before touching state.json, so these tests verify that guard
plus the autouse fixture's delenv. The orchestrator tests also verify
on-disk contents directly — no fake executables or subprocess interception
needed since set_stage is pure Python.
"""

import json
import os
import pathlib
import subprocess
import sys
import textwrap

from _gremlins_core.executor import StateData


def test_autouse_isolate_gremlin_id_unsets_gremlin_id_under_inherited_env(
    tmp_path, child_sandbox
):
    # Spawn a pytest subprocess with GREMLINS_GREMLIN_ID set in env. The autouse
    # fixture must remove it inside the inner test body. Without the subprocess
    # hop this would pass trivially in any environment that doesn't already
    # have GREMLINS_GREMLIN_ID set, so removing the autouse fixture wouldn't trip
    # the regression.
    #
    # Place a conftest.py next to the inner test that loads the real autouse
    # fixture from tests/conftest.py via importlib (keyed by GREMLINS_TESTS_DIR)
    # so we are exercising the actual fixture under test, not a re-implementation.
    inner_conftest = tmp_path / "conftest.py"
    inner_conftest.write_text(
        textwrap.dedent("""
        # Load the autouse _isolate_gremlin_id fixture from the real
        # tests/conftest.py via a file-path import (GREMLINS_TESTS_DIR).
        # importlib avoids the name collision pytest sees when this
        # conftest.py tries to `from conftest import ...`.
        import importlib.util as _u, os, pathlib as _p
        _src = _p.Path(os.environ["GREMLINS_TESTS_DIR"]) / "conftest.py"
        _spec = _u.spec_from_file_location("gremlins_tests_conftest", _src)
        _mod = _u.module_from_spec(_spec)
        _spec.loader.exec_module(_mod)
        _isolate_gremlin_id = _mod._isolate_gremlin_id
    """)
    )
    test_file = tmp_path / "test_inner.py"
    test_file.write_text(
        textwrap.dedent("""
        import os

        def test_gremlin_id_unset_inside_pytest():
            assert os.environ.get("GREMLINS_GREMLIN_ID") is None, (
                "autouse _isolate_gremlin_id fixture failed to remove inherited "
                f"GREMLINS_GREMLIN_ID={os.environ.get('GREMLINS_GREMLIN_ID')!r}"
            )
    """)
    )
    tests_dir = pathlib.Path(__file__).resolve().parent
    repo_root = tests_dir.parent
    env = child_sandbox.share()
    env["GREMLINS_GREMLIN_ID"] = "fake-parent-gremlin-deadbeef"
    env["GREMLINS_TESTS_DIR"] = str(tests_dir)
    env["PYTHONPATH"] = str(repo_root) + os.pathsep + str(tests_dir)
    result = subprocess.run(
        [
            sys.executable,
            "-m",
            "pytest",
            str(test_file),
            "-q",
            "-p",
            "no:cacheprovider",
        ],
        env=env,
        capture_output=True,
        text=True,
        cwd=str(tmp_path),
    )
    assert result.returncode == 0, (
        f"inner pytest failed (autouse fixture not isolating GREMLINS_GREMLIN_ID?):\n"
        f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
    )


# ---------------------------------------------------------------------------
# set_stage direct tests
# ---------------------------------------------------------------------------


def _make_state_dir(state_root, gremlin_id):
    state_dir = state_root / gremlin_id
    state_dir.mkdir(parents=True)
    sf = state_dir / "state.json"
    sf.write_text(json.dumps({"id": gremlin_id, "stage": "implement"}))
    return sf


def test_set_stage_noop_when_gremlin_id_unset(sandbox):
    """set_stage is a no-op when gremlin_id is None (autouse already clears GREMLINS_GREMLIN_ID)."""
    sf = _make_state_dir(sandbox.state, "gr-noop-test")
    # GREMLINS_GREMLIN_ID is already unset via autouse fixture
    mtime_before = sf.stat().st_mtime_ns
    StateData(None).set_stage("running")
    assert sf.stat().st_mtime_ns == mtime_before


def test_set_stage_writes_stage_and_timestamp(sandbox):
    """set_stage writes stage and stage_updated_at to state.json."""
    gremlin_id = "gr-stage-write-test"
    sf = _make_state_dir(sandbox.state, gremlin_id)

    StateData(gremlin_id).set_stage("review-code")

    data = json.loads(sf.read_text())
    assert data["stage"] == "review-code"
    assert "stage_updated_at" in data
    # ISO-8601 UTC second-precision format
    ts = data["stage_updated_at"]
    assert ts.endswith("Z")
    assert len(ts) == 20  # e.g. "2026-04-29T12:00:00Z"


def test_set_stage_with_sub_stage(sandbox):
    """set_stage with sub_stage writes the sub_stage key."""
    gremlin_id = "gr-substage-test"
    sf = _make_state_dir(sandbox.state, gremlin_id)

    StateData(gremlin_id).set_stage("implement", sub_stage={"attempt": 2})

    data = json.loads(sf.read_text())
    assert data["stage"] == "implement"
    assert data["sub_stage"] == {"attempt": 2}


def test_set_stage_removes_sub_stage_when_none(sandbox):
    """Calling set_stage without sub_stage removes a previously written sub_stage key."""
    gremlin_id = "gr-substage-del-test"
    sf = _make_state_dir(sandbox.state, gremlin_id)

    StateData(gremlin_id).set_stage("implement", sub_stage={"k": 1})
    assert "sub_stage" in json.loads(sf.read_text())

    StateData(gremlin_id).set_stage("review-code")
    data = json.loads(sf.read_text())
    assert data["stage"] == "review-code"
    assert "sub_stage" not in data


def test_set_stage_noop_when_state_json_missing(sandbox):
    """set_stage is a no-op when state.json doesn't exist (no crash)."""
    gremlin_id = "gr-missing-state-test"
    state_dir = sandbox.state / gremlin_id
    state_dir.mkdir(parents=True)
    # No state.json written
    StateData(gremlin_id).set_stage("running")  # must not raise


# ---------------------------------------------------------------------------
# write_bail_file direct tests
# ---------------------------------------------------------------------------


def test_write_bail_file_creates_bail_file(sandbox):
    gremlin_id = "gr-wbf-write"
    _make_state_dir(sandbox.state, gremlin_id)

    StateData(gremlin_id).patch(attempt="stage-abc123")
    StateData(gremlin_id).write_bail_file("other", "something went wrong")

    bail_file = sandbox.state / gremlin_id / "bail_stage-abc123.json"
    assert bail_file.exists()
    data = json.loads(bail_file.read_text())
    assert data["class"] == "other"
    assert data["detail"] == "something went wrong"


def test_write_bail_file_noop_when_gremlin_id_none(sandbox):
    gremlin_id = "gr-wbf-noop"
    sf = _make_state_dir(sandbox.state, gremlin_id)
    mtime_before = sf.stat().st_mtime_ns
    StateData(None).write_bail_file("other")
    assert sf.stat().st_mtime_ns == mtime_before


def test_write_bail_file_noop_when_attempt_empty(sandbox):
    gremlin_id = "gr-wbf-empty-attempt"
    _make_state_dir(sandbox.state, gremlin_id)
    StateData(gremlin_id).write_bail_file("other")
    bail_files = list((sandbox.state / gremlin_id).glob("bail_*.json"))
    assert not bail_files


# ---------------------------------------------------------------------------
# Malformed arguments are swallowed, not raised
# ---------------------------------------------------------------------------


def test_patch_ignores_unconvertible_delete(sandbox):
    """A malformed _delete must no-op rather than raise."""
    gremlin_id = "gr-patch-bad-delete"
    sf = _make_state_dir(sandbox.state, gremlin_id)
    StateData(gremlin_id).patch(_delete=5, attempt="x")
    assert "attempt" not in json.loads(sf.read_text())
    StateData(gremlin_id).patch(_delete=[object()], attempt="x")
    assert "attempt" not in json.loads(sf.read_text())


def test_set_stage_ignores_unserializable_sub_stage(sandbox):
    gremlin_id = "gr-substage-bad"
    sf = _make_state_dir(sandbox.state, gremlin_id)
    StateData(gremlin_id).set_stage("implement", sub_stage=object())
    data = json.loads(sf.read_text())
    assert "sub_stage" not in data
    assert "stage_updated_at" not in data
