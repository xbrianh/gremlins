"""Tests for parallel bail shards, state patch concurrency, and stage pinning."""

from __future__ import annotations

import json
import pathlib
import threading

import _gremlins_core.executor as state_mod
import pytest
from _gremlins_core.executor import StateData, build_state
from _gremlins_core.executor import locked_update as _state_locked_update

from tests.fake_client import FakeClient


def _make_state(state_root: pathlib.Path, gremlin_id: str) -> pathlib.Path:
    state_dir = state_root / gremlin_id
    state_dir.mkdir(parents=True)
    sf = state_dir / "state.json"
    sf.write_text(json.dumps({"id": gremlin_id, "stage": ""}), encoding="utf-8")
    return sf


def _read_state(sf: pathlib.Path) -> dict:
    return json.loads(sf.read_text(encoding="utf-8"))


def test_write_bail_file_no_child_key_writes_bail_file(sandbox):
    gremlin_id = "gr-bail-file-a"
    sf = _make_state(sandbox.state, gremlin_id)
    state_dir = sf.parent
    StateData(gremlin_id).patch(attempt="stage-abc")

    StateData(gremlin_id).write_bail_file("other", "child A bailed")

    bail_path = state_dir / "bail_stage-abc.json"
    assert bail_path.exists()
    data = json.loads(bail_path.read_text())
    assert data["class"] == "other"
    assert data["detail"] == "child A bailed"


def test_patch_state_concurrent_no_lost_updates(sandbox):
    gremlin_id = "gr-flock-race"
    _make_state(sandbox.state, gremlin_id)

    errors: list[Exception] = []
    n_threads = 20

    def _increment():
        try:
            sf = state_mod.resolve_state_file(gremlin_id)
            assert sf is not None
            for _ in range(5):
                _state_locked_update(
                    sf,
                    lambda data: data.update({"counter": data.get("counter", 0) + 1}),
                )
        except Exception as e:
            errors.append(e)

    threads = [threading.Thread(target=_increment) for _ in range(n_threads)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    assert not errors, f"threads raised: {errors}"
    sf = state_mod.resolve_state_file(gremlin_id)
    assert sf is not None
    data = _read_state(sf)
    assert data["counter"] == n_threads * 5, (
        f"expected {n_threads * 5} increments, got {data['counter']} (lost updates)"
    )


def test_pipeline_cancel_on_error_and_error_policy_parsed(tmp_path):
    from _gremlins_core.schemas import Pipeline

    yaml_content = """\
name: p
default_client: openai:gpt-4o
prompts:
  fix: |
    Fix it.
stages:
  - name: reviews
    cancel_on_error: true
    error_policy: all
    parallel:
      - {name: r1, type: verify, options: {cmds: ['true']}, prompt: fix}
      - {name: r2, type: verify, options: {cmds: ['true']}, prompt: fix}
"""
    p = tmp_path / "pipeline.yaml"
    p.write_text(yaml_content)
    pipeline = Pipeline.from_yaml(p)
    entry = pipeline.stages[0]
    assert entry.cancel_on_error is True
    assert entry.error_policy == "all"


def test_pipeline_error_policy_invalid_raises(tmp_path):
    from _gremlins_core.schemas import Pipeline

    yaml_content = """\
name: p
default_client: openai:gpt-4o
prompts:
  fix: |
    Fix it.
stages:
  - name: reviews
    error_policy: bogus
    parallel:
      - {name: r1, type: verify, options: {cmds: ['true']}, prompt: fix}
"""
    p = tmp_path / "pipeline.yaml"
    p.write_text(yaml_content)
    with pytest.raises(ValueError, match="error_policy"):
        Pipeline.from_yaml(p)


def test_parallel_child_set_stage_writes_parent_as_stage(tmp_path, sandbox):
    gremlin_id = "gr-parent-stage-pin"
    sf = _make_state(sandbox.state, gremlin_id)

    state = build_state(
        data=StateData(gremlin_id=gremlin_id),
        client=FakeClient(),
        artifact_dir=tmp_path,
        parent_stage="reviews",
    )

    # Simulate what make_runner does at the start of a child stage transition.
    state.data.set_stage("github-review-pull-request", parent_stage=state.parent_stage)

    data = _read_state(sf)
    assert data["stage"] == "reviews"
    assert data["sub_stage"] == "github-review-pull-request"


def test_parallel_child_set_stage_with_sub_stage_payload_writes_parent_as_stage(
    tmp_path, sandbox
):
    gremlin_id = "gr-parent-stage-pin-sub"
    sf = _make_state(sandbox.state, gremlin_id)

    state = build_state(
        data=StateData(gremlin_id=gremlin_id),
        client=FakeClient(),
        artifact_dir=tmp_path,
        parent_stage="reviews",
    )

    # Simulate a stage that calls set_stage with a dict sub_stage (e.g. review_code.py).
    state.data.set_stage(
        "github-review-pull-request",
        {"model": "claude-opus"},
        parent_stage=state.parent_stage,
    )

    data = _read_state(sf)
    assert data["stage"] == "reviews"
    assert data["sub_stage"] == "github-review-pull-request"
