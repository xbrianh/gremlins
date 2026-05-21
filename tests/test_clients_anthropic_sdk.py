"""Tests for AnthropicSdkClient (gremlins/clients/providers/anthropic_sdk.py)."""

from __future__ import annotations

import pytest

from gremlins.clients.client import Client
from gremlins.clients.providers.anthropic_sdk import (
    AnthropicSdkClient,
    make_anthropic_client,
)


def test_constructor_missing_key_raises(monkeypatch):
    monkeypatch.delenv("ANTHROPIC_API_KEY", raising=False)
    with pytest.raises(RuntimeError, match="ANTHROPIC_API_KEY"):
        AnthropicSdkClient("claude-sonnet-4-6")


def test_constructor_with_key_succeeds(monkeypatch):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "test-key")
    client = AnthropicSdkClient("claude-sonnet-4-6")
    assert client._model == "claude-sonnet-4-6"


def test_client_has_run_method(monkeypatch):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "test-key")
    client = AnthropicSdkClient("claude-sonnet-4-6")
    assert callable(getattr(client, "run", None))


def test_client_has_reap_all(monkeypatch):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "test-key")
    client = AnthropicSdkClient("claude-sonnet-4-6")
    client.reap_all()  # must not raise


def test_client_total_cost_usd(monkeypatch):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "test-key")
    client = AnthropicSdkClient("claude-sonnet-4-6")
    assert client.total_cost_usd is None


def test_factory_constructs_client(monkeypatch):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "test-key")
    client = make_anthropic_client("claude-sonnet-4-7")
    assert isinstance(client, AnthropicSdkClient)
    assert client._model == "claude-sonnet-4-7"


def test_factory_missing_key_raises(monkeypatch):
    monkeypatch.delenv("ANTHROPIC_API_KEY", raising=False)
    with pytest.raises(RuntimeError, match="ANTHROPIC_API_KEY"):
        make_anthropic_client("claude-sonnet-4-7")


def test_registered_factory_parses_url(monkeypatch):
    monkeypatch.setenv("ANTHROPIC_API_KEY", "test-key")
    c = Client.parse("anthropic:claude-sonnet-4-7")
    assert c.provider == "anthropic"
    assert c.model == "claude-sonnet-4-7"
    impl = c._get_impl()
    assert isinstance(impl, AnthropicSdkClient)
    assert impl._model == "claude-sonnet-4-7"


def test_registered_factory_missing_key_fails_fast(monkeypatch):
    monkeypatch.delenv("ANTHROPIC_API_KEY", raising=False)
    c = Client.parse("anthropic:claude-sonnet-4-7")
    with pytest.raises(RuntimeError, match="ANTHROPIC_API_KEY"):
        c._get_impl()
