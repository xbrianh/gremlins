"""Tests for build_parallel_stages and run_stages with parallel groups."""

from __future__ import annotations

import asyncio
import inspect
import pathlib
import threading
import time
from collections.abc import Callable
from typing import Any

import pytest
from conftest import _TestGremlin

from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.gremlin import run_stages
from gremlins.executor.state import State, StateData, build_state
from gremlins.pipeline import Pipeline
from gremlins.stages.parallel import ParallelStage

# ---------------------------------------------------------------------------
# Fixtures helpers
# ---------------------------------------------------------------------------


def _make_afn(name: str, log: list[str]):
    async def fn() -> None:
        log.append(name)

    return fn


def _write_yaml(path: pathlib.Path, content: str) -> pathlib.Path:
    path.write_text(content, encoding="utf-8")
    return path


def _make_ctx(child_key: str) -> State:
    return build_state(
        data=StateData(),
        client=FakeClaudeClient(),
        artifact_dir=pathlib.Path("/tmp"),
        child_key=child_key,
    )


def _make_parallel_stages(
    group_name: str,
    child_runners: list[tuple[str, State, Callable[[], Any]]],
    *,
    max_concurrent: int | None = None,
    set_stage_fn: Callable[[str], None] | None = None,
    cancel_on_bail: bool = False,
    bail_policy: str = "any",
    parent_data: StateData | None = None,
    project_root: pathlib.Path | None = None,
) -> list[tuple[str, Callable[[], Any]]]:
    return ParallelStage(
        group_name,
        [],
        max_concurrent=max_concurrent,
        cancel_on_bail=cancel_on_bail,
        bail_policy=bail_policy,
    ).build_runtime_stages(
        child_runners,
        parent_data=parent_data,
        project_root=project_root or pathlib.Path.cwd(),
        set_stage_fn=set_stage_fn,
    )


def _parallel_wrapper(
    children: list[tuple[str, Callable[[], Any]]],
    *,
    max_concurrent: int | None = None,
    set_stage_fn: Callable[[str], None] | None = None,
) -> Callable[[], Any]:
    """Return the parallel-stage callable from ParallelStage."""
    triples = [(n, _make_ctx(n), fn) for n, fn in children]
    stages = _make_parallel_stages(
        "test-group",
        triples,
        max_concurrent=max_concurrent,
        set_stage_fn=set_stage_fn,
    )
    return stages[1][1]  # index 1 is the parallel stage


# ---------------------------------------------------------------------------
# Task 5: pipeline with 'test' type twice
# ---------------------------------------------------------------------------


def test_pipeline_two_test_stages_both_names_present(tmp_path: pathlib.Path) -> None:
    _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: p
clients: {}
stages:
  - {name: test-pre, type: exec}
  - {name: implement, type: exec}
  - {name: test-post, type: exec}
""",
    )
    pipeline = Pipeline.from_yaml(tmp_path / "pipeline.yaml")
    names = [s.name for s in pipeline.stages]
    assert names == ["test-pre", "implement", "test-post"]


def test_run_stages_two_test_entries_both_execute() -> None:
    log: list[str] = []
    stages = [
        ("test-pre", _make_afn("test-pre", log)),
        ("implement", _make_afn("implement", log)),
        ("test-post", _make_afn("test-post", log)),
    ]
    asyncio.run(_TestGremlin(run_stages(stages)))
    assert log == ["test-pre", "implement", "test-post"]


def test_run_stages_resume_targets_first_test() -> None:
    log: list[str] = []
    stages = [
        ("test-pre", _make_afn("test-pre", log)),
        ("implement", _make_afn("implement", log)),
        ("test-post", _make_afn("test-post", log)),
    ]
    asyncio.run(_TestGremlin(run_stages(stages, resume_from="test-pre")))
    assert log == ["test-pre", "implement", "test-post"]


def test_run_stages_resume_targets_second_test() -> None:
    log: list[str] = []
    stages = [
        ("test-pre", _make_afn("test-pre", log)),
        ("implement", _make_afn("implement", log)),
        ("test-post", _make_afn("test-post", log)),
    ]
    asyncio.run(_TestGremlin(run_stages(stages, resume_from="test-post")))
    assert log == ["test-post"]
    assert "test-pre" not in log
    assert "implement" not in log


# ---------------------------------------------------------------------------
# Task 4: parallel group — basic execution
# ---------------------------------------------------------------------------


def test_parallel_wrapper_runs_all_children() -> None:
    log: list[str] = []
    lock = threading.Lock()

    def track(name: str):
        async def fn() -> None:
            with lock:
                log.append(name)

        return fn

    children = [("a", track("a")), ("b", track("b"))]
    wrapper = _parallel_wrapper(children)
    asyncio.run(wrapper())  # type: ignore[operator]
    assert sorted(log) == ["a", "b"]


# ---------------------------------------------------------------------------
# Task 4: parallel group — concurrency
# ---------------------------------------------------------------------------


def test_parallel_wrapper_children_overlap_in_time() -> None:
    timings: dict[str, dict[str, float]] = {}
    lock = threading.Lock()

    def make_timed(name: str, delay: float = 0.05):
        async def fn() -> None:
            with lock:
                timings[name] = {"start": time.monotonic()}
            await asyncio.sleep(delay)
            with lock:
                timings[name]["end"] = time.monotonic()

        return fn

    children = [("a", make_timed("a")), ("b", make_timed("b"))]
    wrapper = _parallel_wrapper(children)
    asyncio.run(wrapper())  # type: ignore[operator]

    # Both started before either finished → overlapping execution
    assert timings["a"]["start"] < timings["b"]["end"]
    assert timings["b"]["start"] < timings["a"]["end"]


# ---------------------------------------------------------------------------
# Task 4: parallel group — failure semantics
# ---------------------------------------------------------------------------


def test_parallel_wrapper_sibling_runs_when_one_child_fails() -> None:
    ran: list[str] = []

    async def failing() -> None:
        raise RuntimeError("child failed")

    async def sibling() -> None:
        await asyncio.sleep(0.02)
        ran.append("sibling")

    children = [("fail", failing), ("sibling", sibling)]
    wrapper = _parallel_wrapper(children)
    with pytest.raises(RuntimeError, match="child failed"):
        asyncio.run(wrapper())  # type: ignore[operator]
    assert "sibling" in ran


# ---------------------------------------------------------------------------
# Task 4: parallel group — resume semantics
# ---------------------------------------------------------------------------


def test_parallel_wrapper_runs_all_children_unconditionally() -> None:
    # Resuming a parallel group always reruns the whole group; child names
    # are not valid resume targets (enforced at the orchestrator layer).
    log: list[str] = []
    children = [
        ("a", _make_afn("a", log)),
        ("b", _make_afn("b", log)),
        ("c", _make_afn("c", log)),
    ]
    wrapper = _parallel_wrapper(children)
    asyncio.run(wrapper())  # type: ignore[operator]
    assert sorted(log) == ["a", "b", "c"]


# ---------------------------------------------------------------------------
# Task 4: run_stages with parallel group registered as three entries
# ---------------------------------------------------------------------------


def test_run_stages_drives_parallel_group() -> None:
    log: list[str] = []

    children = [("r1", _make_afn("r1", log)), ("r2", _make_afn("r2", log))]
    parallel_stages = _make_parallel_stages(
        "reviews",
        [(_n, _make_ctx(_n), _fn) for _n, _fn in children],
    )

    stages = [
        ("plan", _make_afn("plan", log)),
        *parallel_stages,
        ("address", _make_afn("address", log)),
    ]
    asyncio.run(_TestGremlin(run_stages(stages)))
    assert log[0] == "plan"
    assert sorted(log[1:3]) == ["r1", "r2"]
    assert log[3] == "address"


def test_run_stages_resume_from_group_skips_before_group() -> None:
    log: list[str] = []

    children = [("r1", _make_afn("r1", log)), ("r2", _make_afn("r2", log))]
    parallel_stages = _make_parallel_stages(
        "reviews",
        [(_n, _make_ctx(_n), _fn) for _n, _fn in children],
    )

    stages = [
        ("plan", _make_afn("plan", log)),
        *parallel_stages,
        ("address", _make_afn("address", log)),
    ]
    asyncio.run(_TestGremlin(run_stages(stages, resume_from="reviews")))
    assert "plan" not in log
    assert "r1" in log
    assert "r2" in log
    assert "address" in log


# ---------------------------------------------------------------------------
# Task 4: max_concurrent loaded from YAML
# ---------------------------------------------------------------------------


def test_pipeline_parallel_max_concurrent_parsed(tmp_path: pathlib.Path) -> None:
    _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: p
clients: {}
stages:
  - name: reviews
    max_concurrent: 2
    parallel:
      - {name: r1, type: exec}
      - {name: r2, type: exec}
""",
    )
    pipeline = Pipeline.from_yaml(tmp_path / "pipeline.yaml")
    stage = pipeline.stages[0]
    assert isinstance(stage, ParallelStage)
    assert stage.max_concurrent == 2


def test_pipeline_max_concurrent_on_leaf_stage_raises(tmp_path: pathlib.Path) -> None:
    _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: p
clients: {}
stages:
  - {name: s1, type: exec, max_concurrent: 2}
""",
    )
    with pytest.raises(ValueError, match="max_concurrent"):
        Pipeline.from_yaml(tmp_path / "pipeline.yaml")


# ---------------------------------------------------------------------------
# build_parallel_stages stage naming
# ---------------------------------------------------------------------------


def test_build_parallel_stages_names() -> None:
    async def noop() -> None:
        pass

    stages = _make_parallel_stages("reviews", [("r1", _make_ctx("r1"), noop)])
    assert [n for n, _ in stages] == ["reviews-fanout", "reviews", "reviews-fanin"]


# ---------------------------------------------------------------------------
# SequenceStage as a parallel child — worktree propagation
# ---------------------------------------------------------------------------


def test_parallel_sequence_child_worktree_flows() -> None:
    """SequenceStage inside a parallel group sees the fanout worktree in all sub-stages."""
    from gremlins.stages.base import Stage
    from gremlins.stages.outcome import Done, Outcome
    from gremlins.stages.sequence import SequenceStage

    observed: list[pathlib.Path | None] = []

    class _CaptureStage(Stage):
        def __init__(self, name: str) -> None:
            super().__init__(name)

        async def run(self, state: State) -> Outcome:
            observed.append(state.worktree)
            return Done()

    seq_stage = SequenceStage("seq", body=[_CaptureStage("a"), _CaptureStage("b")])

    seq_ctx = build_state(
        data=StateData(),
        client=FakeClaudeClient(),
        artifact_dir=pathlib.Path("/tmp"),
        child_key="seq",
    )

    async def seq_runner() -> None:
        await seq_stage.run(_TestGremlin(seq_ctx))

    project_root = pathlib.Path.cwd()
    stages = _make_parallel_stages(
        "reviews",
        [("seq", seq_ctx, seq_runner)],
        project_root=project_root,
    )

    async def _run_all() -> None:
        for _, fn in stages:
            await fn()  # type: ignore[operator]

    asyncio.run(_run_all())

    assert len(observed) == 2
    # Both sub-stages saw the same non-None worktree that is not the project root
    assert all(wt is not None for wt in observed)
    assert all(wt != project_root for wt in observed)
    assert observed[0] == observed[1]


# ---------------------------------------------------------------------------
# Async stage protocol
# ---------------------------------------------------------------------------


def test_run_stages_async_callable_executes() -> None:
    log: list[str] = []

    async def async_fn() -> None:
        log.append("async")

    asyncio.run(_TestGremlin(run_stages([("a", async_fn)])))
    assert log == ["async"]


def test_make_runner_returns_async_for_any_stage() -> None:
    from gremlins.stages.base import Stage
    from gremlins.stages.outcome import Done, Outcome

    class AStage(Stage):
        type = "a-test"

        async def run(self, state: State) -> Outcome:
            return Done()

    state = build_state(
        data=StateData(), client=FakeClaudeClient(), artifact_dir=pathlib.Path("/tmp")
    )
    runner = state.make_runner(AStage("a"))
    assert inspect.iscoroutinefunction(runner)


def test_stages_run_in_order_via_make_runner() -> None:
    from gremlins.stages.base import Stage
    from gremlins.stages.outcome import Done, Outcome

    executed: list[str] = []

    class StageA(Stage):
        type = "stage-a"

        async def run(self, state: State) -> Outcome:
            executed.append("a")
            return Done()

    class StageB(Stage):
        type = "stage-b"

        async def run(self, state: State) -> Outcome:
            executed.append("b")
            return Done()

    base_state = build_state(
        data=StateData(), client=FakeClaudeClient(), artifact_dir=pathlib.Path("/tmp")
    )
    stages = [
        ("a", base_state.make_runner(StageA("a"))),
        ("b", base_state.make_runner(StageB("b"))),
    ]
    asyncio.run(_TestGremlin(run_stages(stages)))
    assert executed == ["a", "b"]
