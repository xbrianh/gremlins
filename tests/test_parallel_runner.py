"""Tests for make_parallel_wrapper and run_stages with parallel groups."""

from __future__ import annotations

import pathlib
import threading
import time

import pytest

from gremlins.pipeline import load_pipeline
from gremlins.runner import make_parallel_wrapper, run_stages

# ---------------------------------------------------------------------------
# Fixtures helpers
# ---------------------------------------------------------------------------


def _write_yaml(path: pathlib.Path, content: str) -> pathlib.Path:
    path.write_text(content, encoding="utf-8")
    return path


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
  - {name: test-pre, type: verify}
  - {name: implement, type: verify}
  - {name: test-post, type: verify}
""",
    )
    pipeline = load_pipeline(tmp_path / "pipeline.yaml")
    names = [s.name for s in pipeline.stages]
    assert names == ["test-pre", "implement", "test-post"]


def test_run_stages_two_test_entries_both_execute() -> None:
    log: list[str] = []
    stages = [
        ("test-pre", lambda: log.append("test-pre")),
        ("implement", lambda: log.append("implement")),
        ("test-post", lambda: log.append("test-post")),
    ]
    run_stages(stages)
    assert log == ["test-pre", "implement", "test-post"]


def test_run_stages_resume_targets_first_test() -> None:
    log: list[str] = []
    stages = [
        ("test-pre", lambda: log.append("test-pre")),
        ("implement", lambda: log.append("implement")),
        ("test-post", lambda: log.append("test-post")),
    ]
    run_stages(stages, resume_from="test-pre")
    assert log == ["test-pre", "implement", "test-post"]


def test_run_stages_resume_targets_second_test() -> None:
    log: list[str] = []
    stages = [
        ("test-pre", lambda: log.append("test-pre")),
        ("implement", lambda: log.append("implement")),
        ("test-post", lambda: log.append("test-post")),
    ]
    run_stages(stages, resume_from="test-post")
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
        def fn():
            with lock:
                log.append(name)

        return fn

    children = [("a", track("a")), ("b", track("b"))]
    wrapper = make_parallel_wrapper(
        children,
        max_concurrent=None,
        resume_from=None,
        set_stage_fn=lambda: None,
    )
    wrapper()
    assert sorted(log) == ["a", "b"]


# ---------------------------------------------------------------------------
# Task 4: parallel group — concurrency
# ---------------------------------------------------------------------------


def test_parallel_wrapper_children_overlap_in_time() -> None:
    timings: dict[str, dict[str, float]] = {}
    lock = threading.Lock()

    def make_timed(name: str, delay: float = 0.05):
        def fn() -> None:
            with lock:
                timings[name] = {"start": time.monotonic()}
            time.sleep(delay)
            with lock:
                timings[name]["end"] = time.monotonic()

        return fn

    children = [("a", make_timed("a")), ("b", make_timed("b"))]
    wrapper = make_parallel_wrapper(
        children,
        max_concurrent=None,
        resume_from=None,
        set_stage_fn=lambda: None,
    )
    wrapper()

    # Both started before either finished → overlapping execution
    assert timings["a"]["start"] < timings["b"]["end"]
    assert timings["b"]["start"] < timings["a"]["end"]


# ---------------------------------------------------------------------------
# Task 4: parallel group — failure semantics
# ---------------------------------------------------------------------------


def test_parallel_wrapper_sibling_runs_when_one_child_fails() -> None:
    ran: list[str] = []

    def failing() -> None:
        raise RuntimeError("child failed")

    def sibling() -> None:
        time.sleep(0.02)
        ran.append("sibling")

    children = [("fail", failing), ("sibling", sibling)]
    wrapper = make_parallel_wrapper(
        children,
        max_concurrent=None,
        resume_from=None,
        set_stage_fn=lambda: None,
    )
    with pytest.raises(RuntimeError, match="child failed"):
        wrapper()
    assert "sibling" in ran


# ---------------------------------------------------------------------------
# Task 4: parallel group — resume semantics
# ---------------------------------------------------------------------------


def test_parallel_wrapper_resume_from_group_name_runs_all() -> None:
    log: list[str] = []
    children = [("a", lambda: log.append("a")), ("b", lambda: log.append("b"))]
    # "reviews" is the group name, not a child name → all children run
    wrapper = make_parallel_wrapper(
        children,
        max_concurrent=None,
        resume_from="reviews",
        set_stage_fn=lambda: None,
    )
    wrapper()
    assert sorted(log) == ["a", "b"]


def test_parallel_wrapper_resume_from_child_name_skips_earlier() -> None:
    log: list[str] = []
    children = [
        ("a", lambda: log.append("a")),
        ("b", lambda: log.append("b")),
        ("c", lambda: log.append("c")),
    ]
    wrapper = make_parallel_wrapper(
        children,
        max_concurrent=None,
        resume_from="b",
        set_stage_fn=lambda: None,
    )
    wrapper()
    assert "a" not in log
    assert "b" in log
    assert "c" in log


def test_parallel_wrapper_resume_from_last_child_runs_only_last() -> None:
    log: list[str] = []
    children = [
        ("a", lambda: log.append("a")),
        ("b", lambda: log.append("b")),
    ]
    wrapper = make_parallel_wrapper(
        children,
        max_concurrent=None,
        resume_from="b",
        set_stage_fn=lambda: None,
    )
    wrapper()
    assert log == ["b"]


# ---------------------------------------------------------------------------
# Task 4: run_stages with parallel group registered as single entry
# ---------------------------------------------------------------------------


def test_run_stages_drives_parallel_group() -> None:
    log: list[str] = []

    children = [("r1", lambda: log.append("r1")), ("r2", lambda: log.append("r2"))]
    parallel = make_parallel_wrapper(
        children,
        max_concurrent=None,
        resume_from=None,
        set_stage_fn=lambda: None,
    )

    stages = [
        ("plan", lambda: log.append("plan")),
        ("reviews", parallel),
        ("address", lambda: log.append("address")),
    ]
    run_stages(stages)
    assert log[0] == "plan"
    assert sorted(log[1:3]) == ["r1", "r2"]
    assert log[3] == "address"


def test_run_stages_resume_from_group_skips_before_group() -> None:
    log: list[str] = []

    children = [("r1", lambda: log.append("r1")), ("r2", lambda: log.append("r2"))]
    parallel = make_parallel_wrapper(
        children,
        max_concurrent=None,
        resume_from=None,
        set_stage_fn=lambda: None,
    )

    stages = [
        ("plan", lambda: log.append("plan")),
        ("reviews", parallel),
        ("address", lambda: log.append("address")),
    ]
    run_stages(stages, resume_from="reviews")
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
      - {name: r1, type: verify}
      - {name: r2, type: verify}
""",
    )
    pipeline = load_pipeline(tmp_path / "pipeline.yaml")
    assert pipeline.stages[0].max_concurrent == 2


def test_pipeline_max_concurrent_on_leaf_stage_raises(tmp_path: pathlib.Path) -> None:
    _write_yaml(
        tmp_path / "pipeline.yaml",
        """\
name: p
clients: {}
stages:
  - {name: s1, type: verify, max_concurrent: 2}
""",
    )
    with pytest.raises(ValueError, match="max_concurrent"):
        load_pipeline(tmp_path / "pipeline.yaml")
