from __future__ import annotations

import pathlib

import gremlins.clients  # noqa: F401 — registers CLIENT_FACTORIES as a side effect
from gremlins.clients.registry import CLIENT_FACTORIES
from gremlins.permissions.loader import load_default_block
from gremlins.pipeline import Pipeline


def test_all_provider_defaults_cover_local_pipeline_tools() -> None:
    required = {"Read", "Edit", "Bash", "Write", "Grep", "Glob"}
    for provider in CLIENT_FACTORIES:
        block = load_default_block(provider)
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
