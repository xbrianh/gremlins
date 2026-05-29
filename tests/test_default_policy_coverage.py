from __future__ import annotations

import pathlib

import gremlins.clients  # noqa: F401 — registers CLIENT_FACTORIES as a side effect
from gremlins.clients.registry import BYPASS_REQUIRED, CLIENT_FACTORIES
from gremlins.permissions.loader import has_default_block, load_default_block
from gremlins.pipeline import Pipeline


def test_all_provider_defaults_cover_local_pipeline_tools() -> None:
    required = {"Read", "Edit", "Bash", "Write", "Grep", "Glob"}
    for provider in CLIENT_FACTORIES:
        if provider in BYPASS_REQUIRED:
            continue  # bypass-only backends have no allowlist defaults
        if not has_default_block(provider):
            continue  # no bundled defaults for this provider
        block = load_default_block(provider)
        allowed = set(block.get("allowed_tools", []))
        assert required <= allowed, f"{provider}: missing {required - allowed}"


def test_anthropic_default_block_has_disallowed_tools() -> None:
    block = load_default_block("anthropic")
    denied = block.get("disallowed_tools", [])
    assert denied, (
        "anthropic default block must have at least one disallowed_tools entry"
    )


def test_gh_terse_pipeline_loads() -> None:
    from gremlins.pipeline.discovery import resolve_pipeline_name

    path = resolve_pipeline_name("gh-terse", pathlib.Path.cwd())
    pipeline = Pipeline.from_yaml(path)
    stage_names = [s.name for s in pipeline.stages]
    assert "plan" in stage_names
    assert "implement" in stage_names
    assert "push-and-open" in stage_names
