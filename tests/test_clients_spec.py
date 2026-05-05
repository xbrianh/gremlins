"""Tests for stage client resolution."""

from __future__ import annotations

import pytest

from gremlins.clients.resolve import (
    PACKAGE_DEFAULT,
    ClientSpec,
    require_stage_spec,
    resolve_stage_client,
)


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


def test_require_stage_spec_missing_stage_exits(capsys):
    with pytest.raises(SystemExit):
        require_stage_spec({}, "implement")

    assert (
        "stage 'implement' missing from state.json stage_clients"
        in capsys.readouterr().err
    )
