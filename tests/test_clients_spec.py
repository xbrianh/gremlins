"""Tests for Client parsing."""

from __future__ import annotations

import pytest

from gremlins.clients.client import PACKAGE_DEFAULT, Client
from gremlins.pipeline import Pipeline


def test_parse_valid():
    spec = Client.parse("claude:sonnet")
    assert spec.provider == "claude"
    assert spec.model == "sonnet"


def test_parse_empty_model():
    with pytest.raises(ValueError, match="model must not be empty"):
        Client.parse("claude:")


def test_parse_no_colon_raises():
    with pytest.raises(ValueError, match="expected 'provider:model'"):
        Client.parse("claude")


def test_parse_unknown_provider_raises():
    with pytest.raises(ValueError, match="unknown provider"):
        Client.parse("unknown:model")


def test_str_round_trip():
    for s in ("claude:sonnet", "copilot:gpt-4o"):
        assert str(Client.parse(s)) == s


def test_package_default():
    assert PACKAGE_DEFAULT.provider == "claude"
    assert PACKAGE_DEFAULT.model == "sonnet"


def _write(tmp_path, name, body):
    path = tmp_path / f"{name}.yaml"
    path.write_text(body)
    return path


def test_address_code_in_loop_finds_sibling_reviews(tmp_path, monkeypatch):
    """address-code inside a loop scopes review lookup to its loop body, not the boss top level."""
    from gremlins.stages.address_code import _review_stage_info

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
    pipeline = Pipeline.from_yaml(pipeline_path)
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
