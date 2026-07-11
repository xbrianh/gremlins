"""Tests for Client.parse contract."""

import pytest

from gremlins.clients.client import PACKAGE_DEFAULT, Client


def test_parse_xai():
    c = Client.parse("xai:grok-4")
    assert c.provider == "xai"
    assert c.model == "grok-4"
    assert str(c) == "xai:grok-4"


def test_parse_openai():
    c = Client.parse("openai:gpt-4o-mini")
    assert c.provider == "openai"
    assert c.model == "gpt-4o-mini"
    assert str(c) == "openai:gpt-4o-mini"


def test_parse_claude_equals_default():
    c = Client.parse("claude:sonnet")
    assert c.provider == "claude"
    assert c.model == "sonnet"
    assert c == PACKAGE_DEFAULT


def test_parse_roundtrips():
    for spec in (
        "claude:sonnet",
        "copilot:gpt-4o",
        "openai:gpt-4o-mini",
        "xai:grok-4",
        "openrouter:openai/gpt-4o",
    ):
        assert str(Client.parse(spec)) == spec


def test_parse_no_colon_raises():
    with pytest.raises(ValueError, match="'provider:model'"):
        Client.parse("no-colon")


def test_parse_empty_provider_raises():
    with pytest.raises(ValueError, match="'provider:model'"):
        Client.parse(":model")


def test_parse_empty_model_raises():
    with pytest.raises(ValueError, match="model"):
        Client.parse("provider:")


def test_parse_unknown_provider_raises():
    with pytest.raises(ValueError, match="unknown provider"):
        Client.parse("does-not-exist:foo")


def test_client_equality_and_hash():
    a = Client("openai", "gpt-4")
    b = Client("openai", "gpt-4")
    c = Client("openai", "gpt-4o")
    assert a == b
    assert hash(a) == hash(b)
    assert a != c
    assert hash(a) != hash(c)
