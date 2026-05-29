"""Tests for the handoff YAML recipe."""

from __future__ import annotations

import asyncio
import json
import pathlib
import textwrap
from typing import Any

from gremlins.pipeline.preprocess import expand_pipeline
from gremlins.artifacts.uri import Uri
from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import StateData, build_state
from gremlins.recipes import BUNDLED_STAGE_DEF_DIR
from gremlins.stages.exec import Exec
from gremlins.utils.yaml_io import load_yaml_file


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _make_pipeline(tmp_path: pathlib.Path, body_entry: str) -> dict[str, Any]:
    p = tmp_path / "pipeline.yaml"
    p.write_text(
        textwrap.dedent(f"""\
            default_client: claude:sonnet
            stages:
              - name: chain
                type: loop
                body:
                  - {body_entry}
        """),
        encoding="utf-8",
    )
    return expand_pipeline(p)


def _handoff_sequence(tmp_path: pathlib.Path) -> dict[str, Any]:
    result = _make_pipeline(
        tmp_path,
        "name: handoff\n                    type: gremlins:handoff",
    )
    loop_body = result["stages"][0]["body"]
    return loop_body[0]


def _make_state(tmp_path: pathlib.Path) -> Any:
    session_dir = tmp_path / "artifacts"
    session_dir.mkdir(exist_ok=True)
    return build_state(
        data=StateData(),
        client=FakeClaudeClient(),
        session_dir=session_dir,
        worktree=tmp_path,
    )


def _load_translate_signal_exec(_state: Any) -> Exec:
    recipe = load_yaml_file(BUNDLED_STAGE_DEF_DIR / "handoff.yaml")
    body = recipe["stages"][0]["body"]
    ts = next(s for s in body if s["name"] == "translate-signal")
    return Exec(
        "translate-signal",
        {"cmds": ts["options"]["cmds"]},
        out_map=dict(ts["out"]),
    )


def _setup_session(session_dir: pathlib.Path, *, plan: str = "# Plan\n") -> None:
    (session_dir / "plan.md").write_text(plan, encoding="utf-8")
    (session_dir / "boss-spec.md").write_text(plan, encoding="utf-8")
    (session_dir / "rolling-plan.md").write_text(plan, encoding="utf-8")


def _bind_rolling_plan(state: Any) -> None:
    state.artifacts.bind(
        "rolling-plan",
        Uri.parse("file://session/rolling-plan.md"),
    )
    state.artifacts.bind(
        "boss-spec",
        Uri.parse("file://session/boss-spec.md"),
    )


def _write_signal(session_dir: pathlib.Path, **fields: Any) -> None:
    (session_dir / "signal.json").write_text(
        json.dumps(fields), encoding="utf-8"
    )


# ---------------------------------------------------------------------------
# Recipe structure tests
# ---------------------------------------------------------------------------


def test_recipe_expands_to_sequence(tmp_path: pathlib.Path) -> None:
    seq = _handoff_sequence(tmp_path)
    assert seq["type"] == "sequence"
    assert seq["name"] == "handoff"


def test_recipe_sequence_has_five_stages(tmp_path: pathlib.Path) -> None:
    seq = _handoff_sequence(tmp_path)
    names = [s["name"] for s in seq["body"]]
    assert names == [
        "handoff-init",
        "handoff",
        "translate-signal",
        "sanitize",
        "restore-rolling-plan",
    ]


def test_recipe_translate_signal_has_status_and_bail_out(tmp_path: pathlib.Path) -> None:
    seq = _handoff_sequence(tmp_path)
    ts = next(s for s in seq["body"] if s["name"] == "translate-signal")
    assert "status" in ts["out"]
    assert "bail" in ts["out"]


def test_recipe_handoff_agent_has_idle_timeout(tmp_path: pathlib.Path) -> None:
    seq = _handoff_sequence(tmp_path)
    agent = next(s for s in seq["body"] if s["name"] == "handoff")
    assert agent["options"].get("idle_timeout") == 3600


def test_recipe_sanitize_uses_haiku(tmp_path: pathlib.Path) -> None:
    seq = _handoff_sequence(tmp_path)
    sanitize = next(s for s in seq["body"] if s["name"] == "sanitize")
    assert sanitize["options"].get("model") == "haiku"


def test_recipe_client_propagates_to_sequence(tmp_path: pathlib.Path) -> None:
    result = _make_pipeline(
        tmp_path,
        "name: handoff\n                    type: gremlins:handoff\n                    client: claude:opus",
    )
    seq = result["stages"][0]["body"][0]
    assert seq.get("client") == "claude:opus"


# ---------------------------------------------------------------------------
# translate-signal behavior tests
# ---------------------------------------------------------------------------


def test_translate_signal_next_plan(tmp_path: pathlib.Path) -> None:
    state = _make_state(tmp_path)
    _setup_session(state.session_dir)
    _bind_rolling_plan(state)

    child = state.session_dir / "child-plan.md"
    child.write_text("# Child Plan\n", encoding="utf-8")
    _write_signal(
        state.session_dir,
        exit_state="next-plan",
        child_plan=str(child),
        reason=None,
        operator_followups=[],
    )

    stage = _load_translate_signal_exec(state)
    asyncio.run(stage.run(state))

    assert state.artifacts.read("status") == "needs_fix"
    plan_content = (state.session_dir / "plan.md").read_text(encoding="utf-8")
    assert plan_content == "# Child Plan\n"


def test_translate_signal_chain_done(tmp_path: pathlib.Path) -> None:
    state = _make_state(tmp_path)
    _setup_session(state.session_dir, plan="# Boss Spec\n")
    (state.session_dir / "boss-spec.md").write_text("# Boss Spec\n", encoding="utf-8")
    _bind_rolling_plan(state)
    _write_signal(
        state.session_dir,
        exit_state="chain-done",
        child_plan=None,
        reason=None,
        operator_followups=[],
    )

    stage = _load_translate_signal_exec(state)
    asyncio.run(stage.run(state))

    assert state.artifacts.read("status") == "pass"
    plan_content = (state.session_dir / "plan.md").read_text(encoding="utf-8")
    assert plan_content == "# Boss Spec\n"


def test_translate_signal_bail(tmp_path: pathlib.Path) -> None:
    state = _make_state(tmp_path)
    _setup_session(state.session_dir)
    _bind_rolling_plan(state)
    _write_signal(
        state.session_dir,
        exit_state="bail",
        child_plan=None,
        reason="incoherent plan",
        operator_followups=[],
    )

    stage = _load_translate_signal_exec(state)
    asyncio.run(stage.run(state))

    assert state.artifacts.produced("bail")
    reason = state.artifacts.read("bail")
    assert "incoherent plan" in str(reason)


def test_translate_signal_missing_signal_bails(tmp_path: pathlib.Path) -> None:
    state = _make_state(tmp_path)
    _setup_session(state.session_dir)
    _bind_rolling_plan(state)
    # No signal.json written

    stage = _load_translate_signal_exec(state)
    asyncio.run(stage.run(state))

    assert state.artifacts.produced("bail")


def test_translate_signal_next_plan_missing_child_bails(tmp_path: pathlib.Path) -> None:
    state = _make_state(tmp_path)
    _setup_session(state.session_dir)
    _bind_rolling_plan(state)
    _write_signal(
        state.session_dir,
        exit_state="next-plan",
        child_plan="/nonexistent/child-plan.md",
        reason=None,
        operator_followups=[],
    )

    stage = _load_translate_signal_exec(state)
    asyncio.run(stage.run(state))

    assert state.artifacts.produced("bail")
