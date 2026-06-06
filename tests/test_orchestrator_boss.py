"""Boss chain loop orchestrator tests: handoff exit-state signal routing."""

from __future__ import annotations

import asyncio
import dataclasses
import json
import pathlib
import textwrap

import pytest

from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import State, StateData, build_state
from gremlins.pipeline import Pipeline
from gremlins.stages.outcome import Bail


@dataclasses.dataclass
class _MockGremlin:
    state: State | None = None

_MINIMAL = [
    {"type": "system", "subtype": "init"},
    {"type": "result", "subtype": "success"},
]

_CHAIN_YAML = textwrap.dedent("""\
    stages:
      - name: chain
        type: loop
        max-iterations: 1
        body:
          - { name: handoff, type: gremlins:handoff }
""")


class _SignalClient(FakeClaudeClient):
    """Writes signal.json when the handoff agent runs."""

    def __init__(self, signal: dict, artifact_dir: pathlib.Path) -> None:
        super().__init__(fixtures={"handoff": _MINIMAL, "sanitize": _MINIMAL})
        self._signal = signal
        self._artifact_dir = artifact_dir

    async def run(self, prompt, *, label, **kwargs):
        if label == "handoff":
            (self._artifact_dir / "signal.json").write_text(
                json.dumps(self._signal), encoding="utf-8"
            )
        return await super().run(prompt, label=label, **kwargs)


def _make_loop(tmp_path: pathlib.Path, worktree: pathlib.Path, signal: dict):
    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir(parents=True, exist_ok=True)
    (artifact_dir / "plan.md").write_text("# Plan\n", encoding="utf-8")

    pipeline_file = tmp_path / "boss-test.yaml"
    pipeline_file.write_text(_CHAIN_YAML, encoding="utf-8")

    client = _SignalClient(signal=signal, artifact_dir=artifact_dir)
    loop_stage = Pipeline.from_yaml(pipeline_file).stages[0]
    state = build_state(
        data=StateData(),
        client=client,
        artifact_dir=artifact_dir,
        worktree=worktree,
    )
    gremlin = _MockGremlin(state=state)
    return gremlin, loop_stage


def test_boss_chain_done_exits_loop(sandbox, tmp_path):
    signal = {
        "exit_state": "chain-done",
        "child_plan": None,
        "reason": None,
        "operator_followups": [],
    }
    gremlin, loop = _make_loop(tmp_path, sandbox.project, signal)
    asyncio.run(loop.run(gremlin))
    assert gremlin.state.artifacts.read("status") == "pass"


def test_boss_next_plan_needs_fix_and_plan_swap(sandbox, tmp_path):
    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir(parents=True, exist_ok=True)
    child_plan = artifact_dir / "child-plan.md"
    child_plan.write_text("# Next\n", encoding="utf-8")
    signal = {
        "exit_state": "next-plan",
        "child_plan": str(child_plan),
        "reason": None,
        "operator_followups": [],
    }
    gremlin, loop = _make_loop(tmp_path, sandbox.project, signal)
    with pytest.raises(Bail):
        asyncio.run(loop.run(gremlin))
    assert gremlin.state.artifacts.read("status") == "needs_fix"
    assert (artifact_dir / "plan.md").read_text(encoding="utf-8") == "# Next\n"


def test_boss_bail_raises_with_reason(sandbox, tmp_path):
    signal = {
        "exit_state": "bail",
        "child_plan": None,
        "reason": "bad state",
        "operator_followups": [],
    }
    gremlin, loop = _make_loop(tmp_path, sandbox.project, signal)
    with pytest.raises(Bail, match="bad state"):
        asyncio.run(loop.run(gremlin))
    assert gremlin.state.artifacts.produced("bail")
