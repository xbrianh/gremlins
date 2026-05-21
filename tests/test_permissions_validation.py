from __future__ import annotations

import pytest

from gremlins.clients.registry import PROVIDER_CAPABILITIES
from gremlins.permissions.policy import Policy
from gremlins.permissions.validation import validate_policy_against_registry


def _policy(blocks: dict) -> Policy:
    return Policy(blocks=blocks)


def test_non_block_provider_with_non_empty_block_raises() -> None:
    caps = {"fake": False}
    policy = _policy({"fake": {"allowed_tools": ["Read"]}})
    with pytest.raises(ValueError, match="fake"):
        validate_policy_against_registry(policy, caps)


def test_non_block_provider_with_empty_block_is_ok() -> None:
    caps = {"fake": False}
    policy = _policy({"fake": {}})
    validate_policy_against_registry(policy, caps)  # no raise


def test_unknown_provider_raises() -> None:
    caps: dict[str, bool] = {}
    policy = _policy({"ghost": {"allowed_tools": ["Read"]}})
    with pytest.raises(ValueError, match="ghost"):
        validate_policy_against_registry(policy, caps)


def test_all_real_backends_accept_block() -> None:
    import gremlins.clients  # noqa: F401 — ensure factories are registered

    for provider in ("claude", "copilot", "openai", "xai", "anthropic"):
        block = {"allowed_tools": ["Read"]}
        policy = _policy({provider: block})
        validate_policy_against_registry(policy, PROVIDER_CAPABILITIES)  # no raise


def test_error_message_names_offending_provider() -> None:
    caps = {"nope": False}
    policy = _policy({"nope": {"allowed_tools": ["Bash"]}})
    with pytest.raises(ValueError, match="nope"):
        validate_policy_against_registry(policy, caps)


def test_error_message_for_unknown_provider_names_provider() -> None:
    policy = _policy({"unknown": {"allowed_tools": ["Read"]}})
    with pytest.raises(ValueError, match="unknown"):
        validate_policy_against_registry(policy, {})
