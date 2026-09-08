"""Tests for LoopStage termination paths and Exec stage."""

from __future__ import annotations

import asyncio
import json
from typing import TYPE_CHECKING, Any, cast

import pytest
from _gremlins_core.artifacts import Uri
from conftest import MockGremlin, _make_gremlin_wrapper

from gremlins.executor.state import State as RuntimeState
from gremlins.executor.state import StateData, build_state
from gremlins.stages.loop import LoopStage
from gremlins.stages.outcome import Bail, Done

if TYPE_CHECKING:
    from gremlins.executor.gremlin import Gremlin


def _fake_client() -> Any:
    from tests.fake_client import FakeClient

    return FakeClient(fixtures={})


def _loop_state(tmp_path: Any) -> RuntimeState:
    (tmp_path / "artifacts").mkdir(exist_ok=True)
    return build_state(
        data=StateData(),
        client=_fake_client(),
        artifact_dir=tmp_path / "artifacts",
        worktree=tmp_path,
    )


def _set_done(state: RuntimeState) -> None:
    """Write the done artifact to signal loop completion."""
    done_uri = Uri.parse("artifact://done.txt")
    (state.artifact_dir / "done.txt").write_text("done")
    if not state.artifacts.exists(str(done_uri)):
        state.artifacts.register(done_uri)


# ---------------------------------------------------------------------------
# LoopStage termination paths
# ---------------------------------------------------------------------------


def test_loop_exhausted_bails_without_stop_condition(tmp_path):
    """No stop_when_exists and no bail → exhausts iterations then bails."""

    async def runner() -> Done:
        return Done()

    loop = LoopStage("loop", body_runners=[runner], max_iterations=3)
    with pytest.raises(Bail):
        asyncio.run(loop.run(_make_gremlin_wrapper(_loop_state(tmp_path))))


def test_loop_stops_when_stop_when_exists_artifact_is_bound(tmp_path):
    """Loop with stop_when_exists stops when the artifact is bound."""
    loop_state = _loop_state(tmp_path)
    calls: list[str] = []

    async def runner() -> Done:
        calls.append("run")
        _set_done(loop_state)
        return Done()

    loop = LoopStage(
        "loop",
        body_runners=[runner],
        max_iterations=3,
        stop_when_exists="artifact://done.txt",
    )
    outcome = asyncio.run(loop.run(_make_gremlin_wrapper(loop_state)))

    assert outcome == Done()
    assert calls == ["run"]


def test_loop_cmd_failure_then_fix_then_green(tmp_path):
    """Body runs fully each iteration. Fix sets done on second try."""
    loop_state = _loop_state(tmp_path)
    attempt = {"attempt": 0, "fixed": False}

    async def check() -> Done:
        attempt["attempt"] += 1
        return Done()

    async def fix() -> Done:
        if attempt["fixed"]:
            _set_done(loop_state)
        attempt["fixed"] = True
        return Done()

    loop = LoopStage(
        "loop",
        body_runners=[check, fix],
        max_iterations=3,
        stop_when_exists="artifact://done.txt",
    )
    asyncio.run(loop.run(_make_gremlin_wrapper(loop_state)))

    assert attempt["attempt"] == 2
    assert attempt["fixed"]


def test_loop_body_runs_fully_each_iteration(tmp_path):
    """All body runners execute every iteration — no conditional skipping."""
    fix_calls: list[int] = []

    async def check() -> Done:
        return Done()

    async def fix() -> Done:
        fix_calls.append(1)
        _set_done(loop_state)
        return Done()

    loop_state = _loop_state(tmp_path)
    loop = LoopStage(
        "loop",
        body_runners=[check, fix],
        max_iterations=3,
        stop_when_exists="artifact://done.txt",
    )
    asyncio.run(loop.run(_make_gremlin_wrapper(loop_state)))

    # fix always runs, even when check succeeds
    assert fix_calls == [1]


def test_loop_exhausted_returns_bail(tmp_path):
    loop_state = _loop_state(tmp_path)

    async def check() -> Done:
        return Done()

    async def fix() -> Done:
        return Done()

    # No stop_when_exists → runs max_iterations then bails
    loop = LoopStage("loop", body_runners=[check, fix], max_iterations=3)
    with pytest.raises(Bail):
        asyncio.run(loop.run(_make_gremlin_wrapper(loop_state)))


def test_loop_body_fully_executes_on_final_iteration(tmp_path):
    """Body always runs fully even on the last iteration."""
    fix_calls: list[int] = []
    attempt = [0]
    loop_state = _loop_state(tmp_path)

    async def check() -> Done:
        attempt[0] += 1
        return Done()

    async def fix() -> Done:
        fix_calls.append(attempt[0])
        return Done()

    loop = LoopStage("loop", body_runners=[check, fix], max_iterations=3)
    with pytest.raises(Bail):
        asyncio.run(loop.run(_make_gremlin_wrapper(loop_state)))
    # all body stages run every iteration, including the last
    assert fix_calls == [1, 2, 3]


def test_loop_bail_propagates_immediately(tmp_path):
    """Bail raised from a body runner propagates without continuing."""

    async def bail_runner() -> Done:
        raise Bail("stage bailed: bail_class=other")

    loop = LoopStage("loop", body_runners=[bail_runner], max_iterations=3)
    with pytest.raises(Bail) as exc_info:
        asyncio.run(loop.run(_make_gremlin_wrapper(_loop_state(tmp_path))))
    assert "bail_class=other" in exc_info.value.reason


def test_loop_exhausted_emits_bail_to_state(tmp_path, make_state_dir):
    import gremlins.executor.state as state_mod

    gremlin_id = "loop-test-gr"
    state_dir = make_state_dir(gremlin_id)
    attempt = "loop-test-attempt"
    state_mod.StateData(gremlin_id).patch(attempt=attempt)

    (tmp_path / "artifacts").mkdir(exist_ok=True)
    loop_state = build_state(
        data=StateData(gremlin_id=gremlin_id),
        client=_fake_client(),
        artifact_dir=tmp_path / "artifacts",
        worktree=tmp_path,
    )

    async def runner() -> Done:
        return Done()

    loop = LoopStage("loop", body_runners=[runner], max_iterations=2)
    with pytest.raises(Bail):
        asyncio.run(loop.run(_make_gremlin_wrapper(loop_state)))

    bail_file = state_dir / f"bail_{attempt}.json"
    assert bail_file.exists()
    data = json.loads(bail_file.read_text())
    assert data["class"] == "other"


# ---------------------------------------------------------------------------
# stop_when_exists from YAML
# ---------------------------------------------------------------------------


def test_stop_when_exists_from_yaml(tmp_path):
    """with_dict parses stop_when_exists from YAML."""
    loop = LoopStage.with_dict(
        {
            "type": "loop",
            "stop_when_exists": "done",
            "max-iterations": "3",
            "body": [],
        }
    )
    assert loop._stop_when_exists == "done"


def test_no_stop_when_exists_defaults_to_none(tmp_path):
    """with_dict leaves stop_when_exists None when not in YAML."""
    loop = LoopStage.with_dict(
        {
            "type": "loop",
            "max-iterations": "3",
            "body": [],
        }
    )
    assert loop._stop_when_exists is None


# ---------------------------------------------------------------------------
# loop_iteration written to state.json
# ---------------------------------------------------------------------------


def test_loop_patches_loop_iteration_to_state(tmp_path, make_state_dir):
    gremlin_id = "iter-patch-test"
    make_state_dir(gremlin_id)
    seen_iterations: list[str] = []

    (tmp_path / "artifacts").mkdir(exist_ok=True)
    loop_state = build_state(
        data=StateData(gremlin_id=gremlin_id),
        client=_fake_client(),
        artifact_dir=tmp_path / "artifacts",
        worktree=tmp_path,
    )

    async def runner() -> Done:
        seen_iterations.append(loop_state.loop_iter)
        return Done()

    loop = LoopStage("loop", body_runners=[runner], max_iterations=3)
    with pytest.raises(Bail):
        asyncio.run(loop.run(_make_gremlin_wrapper(loop_state)))

    assert seen_iterations == ["loop~1", "loop~2", "loop~3"]


def test_loop_registers_artifacts_across_iterations(tmp_path):
    """register across iterations — overwrite=True ensures clean re-registration."""
    from gremlins.stages.exec import Exec

    (tmp_path / "artifacts").mkdir(exist_ok=True)
    state = _loop_state(tmp_path)

    bound_count = [0]

    async def binder() -> Done:
        uri = Uri.parse(f"artifact://out-{bound_count[0]}.txt")
        state.artifacts.register(uri)
        bound_count[0] += 1
        if bound_count[0] == 2:
            _set_done(state)
        return Done()

    exec_stage = Exec("stage", {}, bind_map={"loop-out": "artifact://out-0.txt"})
    loop = LoopStage(
        "loop",
        body=[exec_stage],
        body_runners=[binder],
        max_iterations=3,
        stop_when_exists="artifact://done.txt",
    )
    asyncio.run(loop.run(cast("Gremlin", MockGremlin(state))))
    assert bound_count[0] == 2


# ---------------------------------------------------------------------------
# loop_iter in-memory stack
# ---------------------------------------------------------------------------


def test_nested_loop_stack(tmp_path):
    """Nested loops produce flat, name-qualified loop_iter values."""
    outer_state = _loop_state(tmp_path)
    recorded: list[str] = []

    async def inner_runner() -> Done:
        recorded.append(outer_state.loop_iter)
        return Done()

    inner = LoopStage("inner", body_runners=[inner_runner], max_iterations=2)

    async def outer_runner() -> Done:
        recorded.append(outer_state.loop_iter)
        try:
            await inner.run(_make_gremlin_wrapper(outer_state))
        except Bail:
            pass
        return Done()

    outer = LoopStage("outer", body_runners=[outer_runner], max_iterations=2)
    with pytest.raises(Bail):
        asyncio.run(outer.run(_make_gremlin_wrapper(outer_state)))

    assert recorded == [
        "outer~1",
        "outer~1~inner~1",
        "outer~1~inner~2",
        "outer~2",
        "outer~2~inner~1",
        "outer~2~inner~2",
    ]


def test_stop_when_exists_resolves_loop_iter(tmp_path):
    """stop_when_exists with {loop_iter} resolves per-iteration."""
    loop_state = _loop_state(tmp_path)

    async def runner() -> Done:
        it = loop_state.loop_iter
        p = loop_state.artifact_dir / it / "done"
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text("done")
        loop_state.artifacts.register(Uri.parse(f"artifact://{it}/done"))
        return Done()

    loop = LoopStage(
        "loop",
        body_runners=[runner],
        max_iterations=5,
        stop_when_exists="artifact://{loop_iter}/done",
    )
    outcome = asyncio.run(loop.run(_make_gremlin_wrapper(loop_state)))
    assert outcome == Done()


def test_loop_iter_scoping_with_exec_isolates_iterations(tmp_path, monkeypatch):
    """Exec bind URIs with {loop_iter} isolate artifacts per iteration.

    If {loop_iter} substitution breaks anywhere, the done artifact from one
    iteration would collide with another, causing the loop to terminate early
    or never stop.
    """
    import pathlib
    import subprocess

    from gremlins.stages.exec import Exec

    (tmp_path / "artifacts").mkdir()
    loop_state = _loop_state(tmp_path)

    shell_calls: list[str] = []

    async def controlled_shell(cmd, **kwargs):
        shell_calls.append(cmd)
        if len(shell_calls) >= 2:
            for key in loop_state.artifacts.keys():
                if key.endswith("/done"):
                    p = pathlib.Path(loop_state.artifacts.data_uri(key))
                    p.parent.mkdir(parents=True, exist_ok=True)
                    p.write_text("done")
        return subprocess.CompletedProcess(cmd, 0, "ok", "")

    monkeypatch.setattr(
        "gremlins.stages.exec._proc.run_shell_async", controlled_shell
    )

    exec_stage = Exec(
        "cmd",
        {"cmds": ["true"]},
        bind_map={"done?": "artifact://{loop_iter}/done"},
    )

    loop = LoopStage(
        "verify",
        body=[exec_stage],
        max_iterations=3,
        stop_when_exists="artifact://{loop_iter}/done",
    )

    outcome = asyncio.run(loop.run(_make_gremlin_wrapper(loop_state)))

    assert outcome == Done()
    assert len(shell_calls) == 2
    assert loop_state.artifacts.is_registered("artifact://verify~1/done")
    assert loop_state.artifacts.is_registered("artifact://verify~2/done")
    assert not loop_state.artifacts.is_registered("artifact://verify~3/done")


def test_loop_iter_not_in_framework_subs(tmp_path):
    """loop_iter and loop_iteration are absent from framework_subs."""
    loop_state = _loop_state(tmp_path)
    stage = LoopStage("test", body_runners=[], max_iterations=1)
    subs = loop_state.framework_subs(stage)
    assert "loop_iter" not in subs
    assert "loop_iteration" not in subs


# ---------------------------------------------------------------------------
# interval option
# ---------------------------------------------------------------------------


def test_loop_interval_sleeps_between_iterations(tmp_path, monkeypatch):
    sleep_calls: list[float] = []

    async def fake_sleep(secs: float) -> None:
        sleep_calls.append(secs)

    import gremlins.stages.loop as _loop_mod

    monkeypatch.setattr(_loop_mod.asyncio, "sleep", fake_sleep)

    loop_state = _loop_state(tmp_path)
    count = [0]

    async def runner() -> Done:
        count[0] += 1
        if count[0] == 2:
            _set_done(loop_state)
        return Done()

    loop = LoopStage(
        "loop",
        body_runners=[runner],
        max_iterations=3,
        interval=5.0,
        stop_when_exists="artifact://done.txt",
    )
    asyncio.run(loop.run(_make_gremlin_wrapper(loop_state)))

    assert count[0] == 2
    assert sleep_calls == [5.0]


def test_loop_no_interval_no_sleep(tmp_path, monkeypatch):
    sleep_calls: list[float] = []

    async def fake_sleep(secs: float) -> None:
        sleep_calls.append(secs)

    import gremlins.stages.loop as _loop_mod

    monkeypatch.setattr(_loop_mod.asyncio, "sleep", fake_sleep)

    async def runner() -> Done:
        return Done()

    loop = LoopStage("loop", body_runners=[runner], max_iterations=3)
    with pytest.raises(Bail):
        asyncio.run(loop.run(_make_gremlin_wrapper(_loop_state(tmp_path))))

    assert sleep_calls == []
