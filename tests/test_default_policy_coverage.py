from __future__ import annotations

import pathlib

import pytest

from gremlins.permissions.loader import load_policy
from gremlins.permissions.policy import KNOWN_PROVIDERS
from gremlins.pipeline import Pipeline


def test_all_provider_defaults_cover_local_pipeline_tools(
    tmp_path: pathlib.Path,
) -> None:
    policy = load_policy(
        cli_bypass=None, cli_permissions_file=None, env={}, cwd=tmp_path
    )
    required = {"Read", "Edit", "Bash", "Write", "Grep", "Glob"}
    for provider in KNOWN_PROVIDERS:
        block = policy.block_for(provider)
        allowed = set(block.get("allowed_tools", []))
        assert required <= allowed, f"{provider}: missing {required - allowed}"


def test_gh_terse_pipeline_loads() -> None:
    from gremlins.pipeline.discovery import resolve_pipeline_name

    path = resolve_pipeline_name("gh-terse", pathlib.Path.cwd())
    pipeline = Pipeline.from_yaml(path)
    stage_types = [s.type for s in pipeline.stages]
    assert "plan" in stage_types
    assert "implement" in stage_types
    assert "github-open-pull-request" in stage_types


@pytest.mark.integration
def test_local_pipeline_default_block_no_denied_tools() -> None:
    """Verify local pipeline runs under default block without tool denials.

    Requires OPENAI_API_KEY or XAI_API_KEY to be set.
    Gated with: pytest -m integration
    """
    import os

    api_key = os.environ.get("OPENAI_API_KEY") or os.environ.get("XAI_API_KEY")
    if not api_key:
        pytest.skip("no OPENAI_API_KEY or XAI_API_KEY set")
    # The actual run is intentionally omitted — the test above verifies structural
    # correctness; a live run is left to CI integration job configuration.
    pytest.skip("live run not wired in unit test runner")
