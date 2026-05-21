from __future__ import annotations

from gremlins.permissions.policy import Policy


def validate_policy_against_registry(policy: Policy, registered: set[str]) -> None:
    for provider, block in policy.blocks.items():
        if not block:
            continue
        if provider not in registered:
            raise ValueError(
                f"permission block references unknown provider {provider!r}; "
                f"remove the block or check your permissions config"
            )
