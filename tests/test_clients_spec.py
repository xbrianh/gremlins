"""Tests for stage client resolution."""

from __future__ import annotations

import pytest

from gremlins.clients.resolve import PACKAGE_DEFAULT, ClientSpec
from gremlins.stage_clients import (
    collect_stage_specs,
    require_stage_spec,
    resolve_stage_client,
    validate_stage_specs,
)
from gremlins.pipeline.discovery import resolve_pipeline_path
from gremlins.pipeline.loader import load_pipeline


def test_parse_valid():
    spec = ClientSpec.parse("claude:sonnet")
    assert spec.provider == "claude"
    assert spec.model == "sonnet"


def test_parse_empty_model():
    with pytest.raises(ValueError, match="model must not be empty"):
        ClientSpec.parse("claude:")


def test_parse_no_colon_raises():
    with pytest.raises(ValueError, match="expected 'provider:model'"):
        ClientSpec.parse("claude")


def test_parse_unknown_provider_raises():
    with pytest.raises(ValueError, match="unknown provider"):
        ClientSpec.parse("unknown:model")


def test_str_round_trip():
    for s in ("claude:sonnet", "copilot:gpt-4o"):
        assert str(ClientSpec.parse(s)) == s


def test_package_default():
    assert PACKAGE_DEFAULT.provider == "claude"
    assert PACKAGE_DEFAULT.model == "sonnet"


def test_resolve_stage_wins():
    stage = ClientSpec("claude", "opus")
    cli = ClientSpec("copilot", "gpt-4o")
    pipeline_default = ClientSpec("claude", "haiku")
    assert resolve_stage_client(stage, cli, pipeline_default) is stage


def test_resolve_cli_wins_over_pipeline():
    cli = ClientSpec("copilot", "gpt-4o")
    pipeline_default = ClientSpec("claude", "haiku")
    assert resolve_stage_client(None, cli, pipeline_default) is cli


def test_resolve_pipeline_default_wins_over_package():
    pipeline_default = ClientSpec("claude", "haiku")
    assert resolve_stage_client(None, None, pipeline_default) is pipeline_default


def test_resolve_falls_back_to_package_default():
    assert resolve_stage_client(None, None, None) is PACKAGE_DEFAULT


def test_require_stage_spec_returns_present_stage():
    spec = ClientSpec("claude", "opus")
    assert require_stage_spec({"implement": spec}, "implement") is spec


def test_require_stage_spec_missing_stage_raises():
    with pytest.raises(ValueError, match=r"stage_clients missing stage: 'implement'"):
        require_stage_spec({}, "implement")


def test_validate_stage_specs_reports_all_missing_stages(tmp_path):
    pipeline = load_pipeline(resolve_pipeline_path("gh", tmp_path))

    with pytest.raises(ValueError, match=r"stage_clients missing stages:"):
        validate_stage_specs({"plan": ClientSpec("claude", "sonnet")}, pipeline)


def _write(tmp_path, name, body):
    path = tmp_path / f"{name}.yaml"
    path.write_text(body)
    return path


def test_collect_and_validate_descend_into_nested_compounds(tmp_path):
    pipeline_path = _write(
        tmp_path,
        "nested",
        """
name: nested
default_client: claude:sonnet
stages:
  - name: outer
    type: loop
    body:
      - { name: leaf-a, type: plan }
      - name: inner
        parallel:
          - { name: leaf-b, type: plan }
          - { name: leaf-c, type: plan }
""",
    )
    pipeline = load_pipeline(pipeline_path)

    specs = collect_stage_specs(pipeline, cli_spec=None)
    assert {"outer", "leaf-a", "inner", "leaf-b", "leaf-c"} <= set(specs)

    validate_stage_specs(specs, pipeline)

    incomplete = {k: v for k, v in specs.items() if k not in {"leaf-b", "leaf-c"}}
    with pytest.raises(ValueError, match=r"'leaf-b', 'leaf-c'"):
        validate_stage_specs(incomplete, pipeline)


def test_address_code_in_loop_finds_sibling_reviews(tmp_path, monkeypatch):
    """address-code inside a loop scopes review lookup to its loop body, not the boss top level."""
    from gremlins.stages.all import _review_stage_info

    pipeline_path = _write(
        tmp_path,
        "boss",
        """
name: boss
default_client: claude:sonnet
stages:
  - { name: review-chain, type: review-code }
  - name: chain
    type: loop
    body:
      - name: reviews
        parallel:
          - { name: review-code, type: review-code }
          - { name: review-code-fidelity, type: review-code }
      - { name: address-code, type: address-code }
""",
    )
    pipeline = load_pipeline(pipeline_path)
    chain_entry = next(s for s in pipeline.stages if s.name == "chain")
    address_entry = next(s for s in chain_entry.body if s.name == "address-code")

    class _Stub:
        pass

    runner = _Stub()
    runner.pipeline_data = pipeline
    runner.session_dir = tmp_path
    runner.current_scope = chain_entry.body
    names, dirs = _review_stage_info(runner)  # type: ignore[arg-type]
    assert sorted(names) == ["review-code", "review-code-fidelity"]

    runner.current_scope = []
    names_top, _ = _review_stage_info(runner)  # type: ignore[arg-type]
    assert names_top == ["review-chain"]
    assert address_entry.type == "address-code"  # sanity
