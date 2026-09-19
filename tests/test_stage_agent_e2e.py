"""Pipeline loader end-to-end coverage for the agent stage type."""

from __future__ import annotations

from _gremlins_core.schemas import parse_stages


def test_agent_parse_stages_registers_type():
    """Confirm the 'agent' type is recognised by the pipeline loader."""
    raw = [
        {
            "name": "my-stage",
            "type": "agent",
            "prompt": ["Do the thing"],
        }
    ]
    stages = parse_stages(raw)
    assert stages[0].type == "agent"
