"""Tests for the YAML-based implement stage-definition expansion."""

from __future__ import annotations

from _gremlins_core.schemas import Pipeline


def test_gh_pipeline_implement_expands_to_three_stages() -> None:
    """type: implement in gh.yaml expands to implement (agent) + git-commit (exec) + require-impl-progress (exec)."""
    from conftest import PIPELINE_FIXTURES_DIR

    pipeline = Pipeline.from_yaml(PIPELINE_FIXTURES_DIR / "gh.yaml")
    names = [s.name for s in pipeline.stages]
    impl_idx = names.index("implement")
    assert names[impl_idx] == "implement"
    assert names[impl_idx + 1] == "git-commit"
    assert names[impl_idx + 2] == "require-impl-progress"
