"""Tests for the local pipeline stage definitions."""

from __future__ import annotations

from _gremlins_core.schemas import Pipeline


def test_local_yaml_loads_and_validates():
    from conftest import PIPELINE_FIXTURES_DIR

    pipeline = Pipeline.from_yaml(PIPELINE_FIXTURES_DIR / "local.yaml")
    assert len(pipeline.stages) == 10
    names = [s.name for s in pipeline.stages]
    assert names == [
        "plan",
        "set-description",
        "implement",
        "git-commit",
        "require-impl-progress",
        "review-code",
        "address-code",
        "normalize",
        "verify-check",
        "verify-test",
    ]
