from __future__ import annotations

import json
import pathlib
import re

import pytest
from conftest import MINIMAL_EVENTS, common_local_patches

from gremlins.clients.fake import FakeClaudeClient
from gremlins.orchestrators.local import local_main


def _write_state(state_dir: pathlib.Path, **fields: object) -> None:
    state_dir.mkdir(parents=True, exist_ok=True)
    (state_dir / "state.json").write_text(
        json.dumps(
            {
                "id": "boss-gr",
                "stage": "",
                "stage_clients": {
                    "chain": "claude:sonnet",
                    "review-chain": "claude:sonnet",
                    "address-chain": "claude:sonnet",
                },
                **fields,
            }
        ),
        encoding="utf-8",
    )


class BossPipelineClient(FakeClaudeClient):
    def __init__(self, *, handoffs: list[dict[str, str]]) -> None:
        super().__init__(
            fixtures={
                "handoff": MINIMAL_EVENTS,
                "implement": MINIMAL_EVENTS,
                "review-code:sonnet": MINIMAL_EVENTS,
                "review-chain:sonnet": MINIMAL_EVENTS,
                "address-code": MINIMAL_EVENTS,
            }
        )
        self._handoffs = list(handoffs)

    def run(self, prompt, *, label, **kwargs):
        if label == "handoff":
            assert self._handoffs, "unexpected extra handoff"
            action = self._handoffs.pop(0)
            rolling_path = pathlib.Path(
                re.search(
                    r"updated plan document.*?to: `([^`]+)`", prompt, re.DOTALL
                ).group(1)
            )
            signal_path = pathlib.Path(
                re.search(
                    r"Write the \*\*signal marker\*\* to: `([^`]+)`", prompt
                ).group(1)
            )
            child_match = re.search(
                r"write a \*\*child plan\*\* to: `([^`]+)`", prompt, re.DOTALL
            )
            child_path = pathlib.Path(child_match.group(1)) if child_match else None

            rolling_path.write_text(action["rolling_plan"], encoding="utf-8")
            payload: dict[str, object] = {
                "exit_state": action["exit_state"],
                "child_plan": None,
                "reason": action.get("reason") or None,
                "operator_followups": [],
            }
            if action["exit_state"] == "next-plan":
                assert child_path is not None
                child_path.write_text(action["child_plan"], encoding="utf-8")
                payload["child_plan"] = str(child_path)
            signal_path.write_text(json.dumps(payload), encoding="utf-8")
        elif label.startswith("review-"):
            out = pathlib.Path(
                re.search(r"`([^`]+\.md)`\s+is the canonical", prompt).group(1)
            )
            out.parent.mkdir(parents=True, exist_ok=True)
            out.write_text("# Review\n\n## Findings\nNone.\n", encoding="utf-8")
        return super().run(prompt, label=label, **kwargs)


def _boss_test_setup(tmp_path, monkeypatch):
    session_dir = tmp_path / "session"
    session_dir.mkdir()
    state_dir = tmp_path / "state" / "boss-gr"
    _write_state(
        state_dir,
        original_plan="# Boss plan\n\n## Tasks\n- [ ] Ship the full change\n",
        chain_base_ref="HEAD",
    )

    monkeypatch.chdir(tmp_path)
    common_local_patches(monkeypatch)
    monkeypatch.setattr("gremlins.paths.state_root", lambda: tmp_path / "state")
    monkeypatch.setattr(
        "gremlins.orchestrators.local.resolve_session_dir",
        lambda gr_id=None: session_dir,
    )
    monkeypatch.setattr("gremlins.orchestrators.local.in_git_repo", lambda: False)
    monkeypatch.setattr(
        "gremlins.orchestrators.local.head_sha", lambda cwd=None: "HEAD"
    )
    monkeypatch.setattr(
        "gremlins.handoff.collect_git_context", lambda base: ("main", "", "")
    )
    monkeypatch.setattr("gremlins.handoff.sanitize_rolling_plan", lambda *a, **kw: None)
    monkeypatch.setattr(
        "gremlins.stages.implement.changes_outside_git", lambda sentinel, session: True
    )

    plan_file = tmp_path / "boss-plan.md"
    plan_file.write_text("# Boss plan\n\n## Tasks\n- [ ] Ship the full change\n")
    return plan_file, session_dir, state_dir


def test_boss_chain_done_without_children(tmp_path, monkeypatch):
    plan_file, session_dir, _state_dir = _boss_test_setup(tmp_path, monkeypatch)
    client = BossPipelineClient(
        handoffs=[
            {
                "exit_state": "chain-done",
                "rolling_plan": "# Complete\n",
            }
        ]
    )

    result = local_main(
        ["--pipeline", "boss", "--plan", str(plan_file)],
        client=client,
        gr_id="boss-gr",
    )

    assert result == 0
    assert [call.label for call in client.calls] == [
        "handoff",
        "review-chain:sonnet",
        "address-code",
    ]
    assert not (session_dir / "chain" / "child-001").exists()


def test_boss_chain_runs_multiple_children(tmp_path, monkeypatch):
    plan_file, session_dir, _state_dir = _boss_test_setup(tmp_path, monkeypatch)
    client = BossPipelineClient(
        handoffs=[
            {
                "exit_state": "next-plan",
                "rolling_plan": "# Rolling 1\n",
                "child_plan": "# Child 1\n\n## Tasks\n- [ ] First chunk\n",
            },
            {
                "exit_state": "next-plan",
                "rolling_plan": "# Rolling 2\n",
                "child_plan": "# Child 2\n\n## Tasks\n- [ ] Second chunk\n",
            },
            {
                "exit_state": "chain-done",
                "rolling_plan": "# Complete\n",
            },
        ]
    )

    result = local_main(
        ["--pipeline", "boss", "--plan", str(plan_file)],
        client=client,
        gr_id="boss-gr",
    )

    assert result == 0
    assert [call.label for call in client.calls] == [
        "handoff",
        "implement",
        "review-code:sonnet",
        "address-code",
        "handoff",
        "implement",
        "review-code:sonnet",
        "address-code",
        "handoff",
        "review-chain:sonnet",
        "address-code",
    ]
    assert (session_dir / "chain" / "child-001" / "plan.md").exists()
    assert (session_dir / "chain" / "child-002" / "plan.md").exists()


def test_boss_resume_reenters_child_stage(tmp_path, monkeypatch):
    plan_file, _session_dir, state_dir = _boss_test_setup(tmp_path, monkeypatch)
    child_plan = tmp_path / "handoff-001-child.md"
    child_plan.write_text(
        "# Child 1\n\n## Tasks\n- [ ] Resume here\n", encoding="utf-8"
    )
    rolling_plan = tmp_path / "handoff-001.md"
    rolling_plan.write_text("# Rolling 1\n", encoding="utf-8")
    _write_state(
        state_dir,
        original_plan="# Boss plan\n\n## Tasks\n- [ ] Ship the full change\n",
        chain_base_ref="HEAD",
        current_child_stage="review-code",
        handoff_history=[
            {
                "rolling_plan": str(rolling_plan),
                "signal_file": str(tmp_path / "handoff-001.state.json"),
                "exit_state": "next-plan",
                "child_plan": str(child_plan),
                "reason": "",
            }
        ],
    )
    client = BossPipelineClient(
        handoffs=[
            {
                "exit_state": "chain-done",
                "rolling_plan": "# Complete\n",
            }
        ]
    )

    result = local_main(
        ["--pipeline", "boss", "--plan", str(plan_file), "--resume-from", "chain"],
        client=client,
        gr_id="boss-gr",
    )

    assert result == 0
    assert [call.label for call in client.calls] == [
        "review-code:sonnet",
        "address-code",
        "handoff",
        "review-chain:sonnet",
        "address-code",
    ]


def test_boss_child_bail_propagates_and_skips_followup_stages(tmp_path, monkeypatch):
    plan_file, _session_dir, state_dir = _boss_test_setup(tmp_path, monkeypatch)
    client = BossPipelineClient(
        handoffs=[
            {
                "exit_state": "next-plan",
                "rolling_plan": "# Rolling 1\n",
                "child_plan": "# Child 1\n\n## Tasks\n- [ ] First chunk\n",
            }
        ]
    )
    client._fixtures.pop("review-code:sonnet")

    def fail_with_security(gr_id, bail_class, bail_detail="", *, child_key=None):
        from gremlins.state import emit_bail as real_emit_bail

        real_emit_bail(gr_id, "security", "child failure", child_key=child_key)

    monkeypatch.setattr("gremlins.stages.review_code.emit_bail", fail_with_security)

    with pytest.raises(RuntimeError, match="child failure"):
        local_main(
            ["--pipeline", "boss", "--plan", str(plan_file)],
            client=client,
            gr_id="boss-gr",
        )

    state = json.loads((state_dir / "state.json").read_text(encoding="utf-8"))
    assert state["bail_class"] == "security"
    assert state["bail_source"] == "child"
    assert state["child_bail_class"] == "security"
    assert "review-chain:sonnet" not in [call.label for call in client.calls]


def test_boss_review_uses_original_plan(tmp_path, monkeypatch):
    plan_file, _session_dir, _state_dir = _boss_test_setup(tmp_path, monkeypatch)
    client = BossPipelineClient(
        handoffs=[
            {
                "exit_state": "chain-done",
                "rolling_plan": "# Different rolling plan\n\n## Tasks\n- [ ] Something else\n",
            }
        ]
    )

    result = local_main(
        ["--pipeline", "boss", "--plan", str(plan_file)],
        client=client,
        gr_id="boss-gr",
    )

    assert result == 0
    review_prompt = next(
        call.prompt for call in client.calls if call.label == "review-chain:sonnet"
    )
    assert "Ship the full change" in review_prompt
    assert "Different rolling plan" not in review_prompt
