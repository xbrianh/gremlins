from __future__ import annotations

import pathlib

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
