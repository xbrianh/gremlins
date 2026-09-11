"""Boss chain loop orchestrator tests: handoff exit-state signal routing."""

from __future__ import annotations

import asyncio
import json
import pathlib
import textwrap

import pytest
from _gremlins_core.schemas import Pipeline
from _gremlins_core.stages import Bail
from conftest import MockGremlin

from gremlins.executor.state import StateData, build_state
from tests.fake_client import FakeClient

_MINIMAL = [
    {"type": "system", "subtype": "init"},
    {"type": "result", "subtype": "success"},
]

_CHAIN_YAML = textwrap.dedent("""\
    default_client: openai:gpt-4o
    stages:
      - name: chain
        type: loop
        max-iterations: 1
        stop_when_exists: "{loop_iter}/done"
        body:
          - { name: handoff, type: gremlins:handoff }
""")


class _SignalClient(FakeClient):
    """Writes signal.json and rolling-plan.md when agent stages run.

    Extracts the slugged output paths from the prompt so the written
    files match the paths the agent was instructed to use and
    verify_produced can find them.
    """

    def __init__(self, signal: dict, artifact_dir: pathlib.Path) -> None:
        super().__init__(fixtures={"handoff": _MINIMAL, "sanitize": _MINIMAL})
        self._signal = signal
        self._artifact_dir = artifact_dir

    async def run(self, prompt, *, label, **kwargs):
        import re

        ad = re.escape(str(self._artifact_dir))
        if label == "handoff":
            for fname in ("signal.json", "child-plan.md"):
                # Scoped form: <artifact_dir>/<loop_iter>/<fname>
                m = re.search(ad + r"/[-\w~]+" + re.escape("/" + fname) + r"\b", prompt)
                if not m:
                    # Non-scoped: <artifact_dir>/<fname>
                    m = re.search(ad + re.escape("/" + fname) + r"\b", prompt)
                if not m:
                    # legacy slugged form: <artifact_dir>/<hex>_<fname>
                    m = re.search(ad + r"/[a-f0-9]+" + re.escape("_" + fname), prompt)
                if m:
                    target = pathlib.Path(m.group(0))
                    if not target.exists():
                        target.parent.mkdir(parents=True, exist_ok=True)
                        if fname == "signal.json":
                            target.write_text(json.dumps(self._signal))
                        else:
                            src = self._artifact_dir / fname
                            if src.exists():
                                target.write_text(src.read_text(encoding="utf-8"))
        elif label == "sanitize":
            m = re.search(
                ad + r"/[-\w~]+" + re.escape("/rolling-plan.md") + r"\b", prompt
            )
            if not m:
                m = re.search(ad + re.escape("/rolling-plan.md") + r"\b", prompt)
            if not m:
                m = re.search(
                    ad + r"/[a-f0-9]+" + re.escape("_rolling-plan.md"), prompt
                )
            if m:
                target = pathlib.Path(m.group(0))
                target.parent.mkdir(parents=True, exist_ok=True)
                pre = self._artifact_dir / "rolling-plan-pre-sanitize.md"
                if pre.exists():
                    target.write_text(pre.read_text(encoding="utf-8"))
        return await super().run(prompt, label=label, **kwargs)

    def _find_slugged(self, name: str, prompt: str) -> pathlib.Path:
        """Extract the slugged path for *name* from the prompt."""
        import re

        ad = re.escape(str(self._artifact_dir))
        m = re.search(ad + r"/[a-f0-9]+" + re.escape("_" + name), prompt)
        if m:
            return pathlib.Path(m.group(0))
        return self._artifact_dir / name

    @staticmethod
    def _write(path: pathlib.Path, content: str) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")


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
    gremlin = MockGremlin(state=state)
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
    assert gremlin.state.artifacts.is_registered("artifact://chain~1/done")


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
    assert gremlin.state.artifacts.is_registered("artifact://chain~1/bail")
