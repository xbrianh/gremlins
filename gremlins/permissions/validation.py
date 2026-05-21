from __future__ import annotations

from gremlins.permissions.policy import Policy


def validate_policy_against_registry(
    policy: Policy, capabilities: dict[str, bool]
) -> None:
    for provider, block in policy.blocks.items():
        if not block:
            continue
        if provider not in capabilities:
            raise ValueError(
                f"permission block references unknown provider {provider!r}; "
                f"remove the block or check your permissions config"
            )
        if not capabilities[provider]:
            raise ValueError(
                f"provider {provider!r} does not accept a permission block; "
                f"remove the block from your permissions config"
            )
