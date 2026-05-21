from __future__ import annotations

import pytest

from gremlins.clients.registry import CLIENT_FACTORIES
from gremlins.permissions.policy import Policy
from gremlins.permissions.validation import validate_policy_against_registry


def _policy(blocks: dict) -> Policy:
    return Policy(blocks=blocks)


def test_empty_block_is_ok() -> None:
    policy = _policy({"fake": {}})
    validate_policy_against_registry(policy, {"fake"})  # no raise


def test_unknown_provider_raises() -> None:
    policy = _policy({"ghost": {"allowed_tools": ["Read"]}})
    with pytest.raises(ValueError, match="ghost"):
        validate_policy_against_registry(policy, set())


def test_all_real_backends_registered() -> None:
    import gremlins.clients  # noqa: F401 — ensure factories are registered

    for provider in ("claude", "copilot", "openai", "xai", "anthropic"):
        block = {"allowed_tools": ["Read"]}
        policy = _policy({provider: block})
        validate_policy_against_registry(policy, set(CLIENT_FACTORIES))  # no raise


def test_error_message_for_unknown_provider_names_provider() -> None:
    policy = _policy({"unknown": {"allowed_tools": ["Read"]}})
    with pytest.raises(ValueError, match="unknown"):
        validate_policy_against_registry(policy, set())
