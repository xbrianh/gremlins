"""Tests for the parallel three-stage decomposition and related state fixes.

Covers:
- Per-child bail shards: children write to parallel_bails, not top-level.
- bail_policy: 'any' and 'all' aggregation rules.
- fcntl.flock race: concurrent patch_state calls produce no lost updates.
- cancel_on_bail: children that haven't started are skipped on first bail.
- Fan-in resume: --resume-from <group>-fanin aggregates existing shards.
- Worktree lifecycle: fan-out creates worktrees, fan-in removes them.
- Existing review-lens pipeline behaviour is unchanged with defaults.
"""

from __future__ import annotations

import json
import pathlib
import subprocess
import threading
import time

import pytest

import gremlins.executor.state as state_mod
from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.pipeline import run_stages
from gremlins.executor.state import State
from gremlins.stages.parallel import ParallelStage
from gremlins.utils.state_file import locked_update as _state_locked_update

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


@pytest.fixture
def state_root(tmp_path: pathlib.Path, monkeypatch):
    root = tmp_path / "state"
    monkeypatch.setattr("gremlins.paths.state_root", lambda: root)
    return root


def _make_state(state_root: pathlib.Path, gremlin_id: str) -> pathlib.Path:
    state_dir = state_root / gremlin_id
    state_dir.mkdir(parents=True)
    sf = state_dir / "state.json"
    sf.write_text(json.dumps({"id": gremlin_id, "stage": ""}), encoding="utf-8")
    return sf


def _read_state(sf: pathlib.Path) -> dict:
    return json.loads(sf.read_text(encoding="utf-8"))


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _make_parallel_stages(
    group_name: str,
    child_runners: list,
    *,
    max_concurrent: int | None = None,
    set_stage_fn=None,
    cancel_on_bail: bool = False,
    bail_policy: str = "any",
    gremlin_id=None,
    project_root: pathlib.Path | None = None,
    parent_attempt: str = "",
) -> list:
    if set_stage_fn is None:

        def set_stage_fn(_n):
            return None

    if project_root is None:
        project_root = pathlib.Path.cwd()
    return ParallelStage(
        group_name,
        [],
        max_concurrent=max_concurrent,
        cancel_on_bail=cancel_on_bail,
        bail_policy=bail_policy,
    ).build_runtime_stages(
        child_runners,
        gremlin_id=gremlin_id,
        project_root=project_root,
        set_stage_fn=set_stage_fn,
        parent_attempt=parent_attempt,
    )


# ---------------------------------------------------------------------------
# Per-child bail via bail files
# ---------------------------------------------------------------------------


def test_write_bail_file_no_child_key_writes_bail_file(state_root):
    gremlin_id = "gr-bail-file-a"
    sf = _make_state(state_root, gremlin_id)
    state_dir = sf.parent
    state_mod.patch_state(gremlin_id, attempt="stage-abc")

    state_mod.write_bail_file(gremlin_id, "stage-abc", "other", "child A bailed")

    bail_path = state_dir / "bail_stage-abc.json"
    assert bail_path.exists()
    data = json.loads(bail_path.read_text())
    assert data["class"] == "other"
    assert data["detail"] == "child A bailed"


def test_check_bail_reads_attempt_from_state_json(state_root):
    gremlin_id = "gr-check-attempt"
    sf = _make_state(state_root, gremlin_id)
    state_dir = sf.parent
    state_mod.patch_state(gremlin_id, attempt="my-attempt-abc")

    # No bail file yet → no raise
    state_mod.check_bail(gremlin_id, "test")

    # Write bail file → raises
    (state_dir / "bail_my-attempt-abc.json").write_text(json.dumps({"class": "other"}))
    with pytest.raises(RuntimeError, match="bailed"):
        state_mod.check_bail(gremlin_id, "test")


def test_check_bail_child_key_reads_parallel_attempts(state_root):
    gremlin_id = "gr-parallel-attempt"
    sf = _make_state(state_root, gremlin_id)
    state_dir = sf.parent
    state_mod._patch_parallel_attempt(gremlin_id, "child-a", "attempt-a")

    # No bail file yet
    state_mod.check_bail(gremlin_id, "test", child_key="child-a")

    # Write bail for child-a
    (state_dir / "bail_attempt-a.json").write_text(json.dumps({"class": "other"}))
    with pytest.raises(RuntimeError, match="bailed"):
        state_mod.check_bail(gremlin_id, "test", child_key="child-a")

    # child-b has no bail
    state_mod.check_bail(gremlin_id, "test", child_key="child-b")


# ---------------------------------------------------------------------------
# fcntl.flock race: concurrent patch_state calls — no lost updates
# ---------------------------------------------------------------------------


def test_patch_state_concurrent_no_lost_updates(state_root):
    gremlin_id = "gr-flock-race"
    _make_state(state_root, gremlin_id)

    errors: list[Exception] = []
    n_threads = 20

    def _increment():
        try:
            sf = state_mod.resolve_state_file(gremlin_id)
            assert sf is not None
            for _ in range(5):
                _state_locked_update(
                    sf,
                    lambda data: data.update({"counter": data.get("counter", 0) + 1}),
                )
        except Exception as e:
            errors.append(e)

    threads = [threading.Thread(target=_increment) for _ in range(n_threads)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    assert not errors, f"threads raised: {errors}"
    sf = state_mod.resolve_state_file(gremlin_id)
    assert sf is not None
    data = _read_state(sf)
    assert data["counter"] == n_threads * 5, (
        f"expected {n_threads * 5} increments, got {data['counter']} (lost updates)"
    )


# ---------------------------------------------------------------------------
# bail_policy via build_parallel_stages
# ---------------------------------------------------------------------------


def _make_simple_ctx(tmp_path: pathlib.Path, child_key: str) -> State:
    return State(
        client=FakeClaudeClient(),
        session_dir=tmp_path / child_key,
        gremlin_id=None,
        child_key=child_key,
    )


def _build_fanin_test(
    tmp_path: pathlib.Path,
    state_root: pathlib.Path,
    gremlin_id: str,
    shards: dict[str, str],  # child_key -> bail_class (empty = no bail)
    bail_policy: str,
    parent_attempt: str = "parent-attempt",
) -> tuple[pathlib.Path, list]:
    sf = _make_state(state_root, gremlin_id)
    state_dir = sf.parent

    # Pre-populate parallel_attempts and bail files as fan-out+parallel would have done.
    parallel_attempts: dict[str, str] = {}
    for key, bc in shards.items():
        attempt = f"attempt-{key}"
        parallel_attempts[key] = attempt
        if bc:
            (state_dir / f"bail_{attempt}.json").write_text(
                json.dumps({"class": bc, "detail": ""})
            )
    state_mod.patch_state(gremlin_id, parallel_attempts=parallel_attempts)

    child_keys = list(shards.keys())
    children = [(k, _make_simple_ctx(tmp_path, k), lambda: None) for k in child_keys]

    # We need a git-less project root so fan-in's worktree logic is skipped.
    project_root = tmp_path / "nongit"
    project_root.mkdir(exist_ok=True)

    stages = _make_parallel_stages(
        "reviews",
        children,
        max_concurrent=None,
        set_stage_fn=lambda _n: None,
        cancel_on_bail=False,
        bail_policy=bail_policy,
        gremlin_id=gremlin_id,
        project_root=project_root,
        parent_attempt=parent_attempt,
    )
    return sf, stages


def test_bail_policy_any_one_bailed_sets_parent_bail(tmp_path, state_root):
    gremlin_id = "gr-policy-any"
    shards = {"child-a": "other", "child-b": ""}
    sf, stages = _build_fanin_test(tmp_path, state_root, gremlin_id, shards, "any")

    # Run just the fanin stage.
    with pytest.raises(RuntimeError, match="bailed"):
        stages[2][1]()  # fanin is index 2

    state_dir = sf.parent
    bail_path = state_dir / "bail_parent-attempt.json"
    assert bail_path.exists()
    bail_data = json.loads(bail_path.read_text())
    assert bail_data["class"] == "other"
    data = _read_state(sf)
    assert "parallel_attempts" not in data


def test_bail_policy_all_one_bailed_no_parent_bail(tmp_path, state_root):
    gremlin_id = "gr-policy-all-partial"
    shards = {"child-a": "other", "child-b": ""}
    sf, stages = _build_fanin_test(tmp_path, state_root, gremlin_id, shards, "all")

    # Only one bailed; policy=all requires all → no parent bail.
    stages[2][1]()  # fanin should not raise

    state_dir = sf.parent
    assert not (state_dir / "bail_parent-attempt.json").exists()
    data = _read_state(sf)
    assert "parallel_attempts" not in data


def test_bail_policy_all_both_bailed_sets_parent_bail(tmp_path, state_root):
    gremlin_id = "gr-policy-all-both"
    shards = {"child-a": "other", "child-b": "reviewer_requested_changes"}
    sf, stages = _build_fanin_test(tmp_path, state_root, gremlin_id, shards, "all")

    with pytest.raises(RuntimeError, match="bailed"):
        stages[2][1]()

    state_dir = sf.parent
    bail_path = state_dir / "bail_parent-attempt.json"
    assert bail_path.exists()
    data = _read_state(sf)
    assert "parallel_attempts" not in data


# ---------------------------------------------------------------------------
# cancel_on_bail: unstarted children are skipped after first bail
# ---------------------------------------------------------------------------


def test_cancel_on_bail_skips_unstarted_children():
    ran: list[str] = []
    barrier = threading.Barrier(2)  # synchronize first two children

    def child_a() -> None:
        barrier.wait()
        raise RuntimeError("child-a bailed")

    def child_b() -> None:
        barrier.wait()
        time.sleep(0.05)
        ran.append("b")

    def child_c() -> None:
        ran.append("c")

    ctx_a = State(
        client=FakeClaudeClient(),
        session_dir=pathlib.Path("/tmp"),
        gremlin_id=None,
        child_key="a",
    )
    ctx_b = State(
        client=FakeClaudeClient(),
        session_dir=pathlib.Path("/tmp"),
        gremlin_id=None,
        child_key="b",
    )
    ctx_c = State(
        client=FakeClaudeClient(),
        session_dir=pathlib.Path("/tmp"),
        gremlin_id=None,
        child_key="c",
    )

    children = [("a", ctx_a, child_a), ("b", ctx_b, child_b), ("c", ctx_c, child_c)]

    stages = _make_parallel_stages(
        "workers",
        children,
        max_concurrent=2,  # only 2 concurrent; c starts only after a or b finishes
        set_stage_fn=lambda _n: None,
        cancel_on_bail=True,
        bail_policy="any",
        gremlin_id=None,
        project_root=pathlib.Path.cwd(),
    )

    # Run just the parallel stage (index 1); skip fanout/fanin.
    with pytest.raises(RuntimeError, match="child-a bailed"):
        stages[1][1]()

    # child-c should not have run (cancel flag set before it started).
    assert "c" not in ran


# ---------------------------------------------------------------------------
# Fan-in resume: --resume-from <group>-fanin aggregates existing shards
# ---------------------------------------------------------------------------


def test_fanin_resume_aggregates_existing_shards(tmp_path, state_root):
    gremlin_id = "gr-fanin-resume"
    shards = {"child-a": "other", "child-b": ""}
    sf, stages = _build_fanin_test(tmp_path, state_root, gremlin_id, shards, "any")

    # Simulate resuming from fanin: only run the fanin stage.
    # Fanin raises because one child bailed; state must still be written.
    with pytest.raises(RuntimeError, match="bailed"):
        run_stages(stages[2:], resume_from="reviews-fanin")

    state_dir = sf.parent
    assert (state_dir / "bail_parent-attempt.json").exists()
    data = _read_state(sf)
    assert "parallel_attempts" not in data


def test_run_stages_resume_from_fanin_name(tmp_path, state_root):
    gremlin_id = "gr-resume-fanin-name"
    sf = _make_state(state_root, gremlin_id)
    state_dir = sf.parent
    # Pre-populate parallel_attempts and bail file as children would have written.
    state_mod.patch_state(gremlin_id, parallel_attempts={"c": "attempt-c"})
    (state_dir / "bail_attempt-c.json").write_text(
        json.dumps({"class": "other", "detail": ""})
    )

    project_root = tmp_path / "nongit2"
    project_root.mkdir()

    ctx = _make_simple_ctx(tmp_path, "c")
    stages = _make_parallel_stages(
        "reviews",
        [("c", ctx, lambda: None)],
        max_concurrent=None,
        set_stage_fn=lambda _n: None,
        cancel_on_bail=False,
        bail_policy="any",
        gremlin_id=gremlin_id,
        project_root=project_root,
        parent_attempt="fanin-resume-parent",
    )

    # The three stage names should be reviews-fanout, reviews, reviews-fanin.
    names = [n for n, _ in stages]
    assert names == ["reviews-fanout", "reviews", "reviews-fanin"]

    # run_stages with resume_from=reviews-fanin skips fanout and parallel.
    with pytest.raises(RuntimeError, match="bailed"):
        run_stages(stages, resume_from="reviews-fanin")

    assert (state_dir / "bail_fanin-resume-parent.json").exists()
    data = _read_state(sf)
    assert "parallel_attempts" not in data


# ---------------------------------------------------------------------------
# Worktree lifecycle
# ---------------------------------------------------------------------------


def _init_git_repo(path: pathlib.Path) -> None:
    subprocess.run(
        ["git", "init", "-b", "main", str(path)], check=True, capture_output=True
    )
    subprocess.run(
        ["git", "-C", str(path), "config", "user.email", "t@t.com"],
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "-C", str(path), "config", "user.name", "T"],
        check=True,
        capture_output=True,
    )
    (path / "README.md").write_text("init")
    subprocess.run(
        ["git", "-C", str(path), "add", "."], check=True, capture_output=True
    )
    subprocess.run(
        ["git", "-C", str(path), "commit", "-m", "init"],
        check=True,
        capture_output=True,
    )


def test_worktree_lifecycle_fanout_creates_and_fanin_removes(tmp_path):
    repo = tmp_path / "repo"
    repo.mkdir()
    _init_git_repo(repo)

    ctx_a = State(
        client=FakeClaudeClient(), session_dir=tmp_path / "a", gremlin_id=None, child_key="a"
    )
    ctx_b = State(
        client=FakeClaudeClient(), session_dir=tmp_path / "b", gremlin_id=None, child_key="b"
    )

    stages = _make_parallel_stages(
        "reviews",
        [("a", ctx_a, lambda: None), ("b", ctx_b, lambda: None)],
        max_concurrent=None,
        set_stage_fn=lambda _n: None,
        cancel_on_bail=False,
        bail_policy="any",
        gremlin_id=None,
        project_root=repo,
    )

    # Fan-out: worktrees should be created.
    import os

    orig_cwd = os.getcwd()
    os.chdir(str(repo))
    try:
        stages[0][1]()  # fanout
        wt_list_after_fanout = subprocess.run(
            ["git", "worktree", "list", "--porcelain"],
            cwd=str(repo),
            capture_output=True,
            text=True,
            check=True,
        ).stdout
        # Should have 2 extra worktrees (plus the main one).
        worktree_count = wt_list_after_fanout.count("worktree ")
        assert worktree_count >= 3, (
            f"expected >=3 worktrees, got:\n{wt_list_after_fanout}"
        )

        # ctx should have worktree paths set.
        assert ctx_a.worktree is not None and ctx_a.worktree.is_dir()
        assert ctx_b.worktree is not None and ctx_b.worktree.is_dir()

        # Parallel stage: noop children.
        stages[1][1]()  # parallel

        # Fan-in: worktrees should be removed.
        stages[2][1]()  # fanin (no bails, should not raise)

        wt_list_after_fanin = subprocess.run(
            ["git", "worktree", "list", "--porcelain"],
            cwd=str(repo),
            capture_output=True,
            text=True,
            check=True,
        ).stdout
        assert wt_list_after_fanin.count("worktree ") == 1, (
            f"expected only main worktree after fanin, got:\n{wt_list_after_fanin}"
        )
    finally:
        os.chdir(orig_cwd)


def test_fanout_persists_worktrees_and_fresh_fanin_can_clean_up(tmp_path, state_root):
    """Fan-out writes worktree paths to state.json; a fresh build_parallel_stages
    instance (simulating a resume in a new process) reads them back and cleans
    them up during fan-in."""
    gremlin_id = "gr-resume-wt"
    _make_state(state_root, gremlin_id)

    repo = tmp_path / "repo"
    repo.mkdir()
    _init_git_repo(repo)

    def _make_ctx(name: str) -> State:
        return State(
            client=FakeClaudeClient(),
            session_dir=tmp_path / name,
            gremlin_id=gremlin_id,
            child_key=name,
        )

    # First instance: only run fan-out, then drop the closure (simulating
    # process exit between fan-out and parallel/fan-in).
    stages_run1 = _make_parallel_stages(
        "reviews",
        [("a", _make_ctx("a"), lambda: None), ("b", _make_ctx("b"), lambda: None)],
        max_concurrent=None,
        set_stage_fn=lambda _n: None,
        cancel_on_bail=False,
        bail_policy="any",
        gremlin_id=gremlin_id,
        project_root=repo,
    )
    stages_run1[0][1]()  # fan-out only

    # state.json should now record both worktree paths.
    sf = state_mod.resolve_state_file(gremlin_id)
    assert sf is not None
    persisted = (_read_state(sf).get("parallel_worktrees") or {}).get("reviews") or {}
    assert set(persisted.get("paths", {}).keys()) == {"a", "b"}
    prior_paths = [pathlib.Path(p) for p in persisted["paths"].values()]
    for p in prior_paths:
        assert p.is_dir()

    # Second instance: simulate fresh process. New closures, empty in-process
    # state. Run fan-in directly — it should hydrate from state.json and tear
    # down the worktrees fan-out created in run 1.
    stages_run2 = _make_parallel_stages(
        "reviews",
        [("a", _make_ctx("a"), lambda: None), ("b", _make_ctx("b"), lambda: None)],
        max_concurrent=None,
        set_stage_fn=lambda _n: None,
        cancel_on_bail=False,
        bail_policy="any",
        gremlin_id=gremlin_id,
        project_root=repo,
    )
    stages_run2[2][1]()  # fan-in

    # Worktrees gone from disk and from state.json.
    for p in prior_paths:
        assert not p.is_dir()
    assert "reviews" not in (_read_state(sf).get("parallel_worktrees") or {})


def test_fanout_resume_tears_down_prior_worktrees(tmp_path, state_root):
    """A second fan-out (e.g. after `--resume-from <group>-fanout`) cleans up
    the previous run's worktrees before creating fresh ones."""
    gremlin_id = "gr-resume-fanout"
    _make_state(state_root, gremlin_id)

    repo = tmp_path / "repo"
    repo.mkdir()
    _init_git_repo(repo)

    def _make_ctx(name: str) -> State:
        return State(
            client=FakeClaudeClient(),
            session_dir=tmp_path / name,
            gremlin_id=gremlin_id,
            child_key=name,
        )

    stages_run1 = _make_parallel_stages(
        "reviews",
        [("a", _make_ctx("a"), lambda: None)],
        max_concurrent=None,
        set_stage_fn=lambda _n: None,
        cancel_on_bail=False,
        bail_policy="any",
        gremlin_id=gremlin_id,
        project_root=repo,
    )
    stages_run1[0][1]()  # fan-out

    sf = state_mod.resolve_state_file(gremlin_id)
    assert sf is not None
    prior_path_str = (_read_state(sf)["parallel_worktrees"]["reviews"]["paths"])["a"]
    prior_path = pathlib.Path(prior_path_str)
    assert prior_path.is_dir()

    # Re-run fan-out from a fresh closure → prior worktree should be torn down,
    # new one created at a fresh path.
    stages_run2 = _make_parallel_stages(
        "reviews",
        [("a", _make_ctx("a"), lambda: None)],
        max_concurrent=None,
        set_stage_fn=lambda _n: None,
        cancel_on_bail=False,
        bail_policy="any",
        gremlin_id=gremlin_id,
        project_root=repo,
    )
    stages_run2[0][1]()  # fan-out again

    new_path_str = (_read_state(sf)["parallel_worktrees"]["reviews"]["paths"])["a"]
    assert new_path_str != prior_path_str
    assert not prior_path.is_dir()
    assert pathlib.Path(new_path_str).is_dir()


# ---------------------------------------------------------------------------
# Existing review-lens pipeline behaviour unchanged
# ---------------------------------------------------------------------------


def test_build_parallel_stages_returns_three_named_stages():
    ctx = State(
        client=FakeClaudeClient(),
        session_dir=pathlib.Path("/tmp"),
        gremlin_id=None,
        child_key="r1",
    )
    stages = _make_parallel_stages(
        "reviews",
        [("r1", ctx, lambda: None)],
        max_concurrent=None,
        set_stage_fn=lambda _n: None,
        cancel_on_bail=False,
        bail_policy="any",
        gremlin_id=None,
        project_root=pathlib.Path.cwd(),
    )
    names = [n for n, _ in stages]
    assert names == ["reviews-fanout", "reviews", "reviews-fanin"]


def test_parallel_all_children_complete_with_defaults():
    ran: list[str] = []
    ctx_a = State(
        client=FakeClaudeClient(),
        session_dir=pathlib.Path("/tmp"),
        gremlin_id=None,
        child_key="a",
    )
    ctx_b = State(
        client=FakeClaudeClient(),
        session_dir=pathlib.Path("/tmp"),
        gremlin_id=None,
        child_key="b",
    )

    stages = _make_parallel_stages(
        "reviews",
        [
            ("a", ctx_a, lambda: ran.append("a")),
            ("b", ctx_b, lambda: ran.append("b")),
        ],
        max_concurrent=None,
        set_stage_fn=lambda _n: None,
        cancel_on_bail=False,
        bail_policy="any",
        gremlin_id=None,
        project_root=pathlib.Path.cwd(),
    )

    # Run all three stages end-to-end (no git repo → fanout is a no-op).
    for _, fn in stages:
        fn()

    assert sorted(ran) == ["a", "b"]


def test_pipeline_cancel_on_bail_and_bail_policy_parsed(tmp_path):
    from gremlins.pipeline import Pipeline

    yaml_content = """\
name: p
stages:
  - name: reviews
    cancel_on_bail: true
    bail_policy: all
    parallel:
      - {name: r1, type: verify}
      - {name: r2, type: verify}
"""
    p = tmp_path / "pipeline.yaml"
    p.write_text(yaml_content)
    pipeline = Pipeline.from_yaml(p)
    entry = pipeline.stages[0]
    assert entry.cancel_on_bail is True
    assert entry.bail_policy == "all"


def test_pipeline_bail_policy_invalid_raises(tmp_path):
    from gremlins.pipeline import Pipeline

    yaml_content = """\
name: p
stages:
  - name: reviews
    bail_policy: bogus
    parallel:
      - {name: r1, type: verify}
"""
    p = tmp_path / "pipeline.yaml"
    p.write_text(yaml_content)
    with pytest.raises(ValueError, match="bail_policy"):
        Pipeline.from_yaml(p)


# ---------------------------------------------------------------------------
# parent_stage pinning: parallel child failure records top-level stage name
# ---------------------------------------------------------------------------


def test_parallel_child_set_stage_writes_parent_as_stage(tmp_path, state_root):
    gremlin_id = "gr-parent-stage-pin"
    sf = _make_state(state_root, gremlin_id)

    state = State(
        client=FakeClaudeClient(),
        session_dir=tmp_path,
        gremlin_id=gremlin_id,
        parent_stage="reviews",
    )

    # Simulate what make_runner does at the start of a child stage transition.
    state.set_stage("github-review-pull-request")

    data = _read_state(sf)
    assert data["stage"] == "reviews"
    assert data["sub_stage"] == "github-review-pull-request"

    # The recorded stage must be a valid resume_from target in a pipeline that
    # has "reviews" as a top-level name.
    pipeline_stages: list[tuple[str, object]] = [
        ("plan", lambda: None),
        ("reviews-fanout", lambda: None),
        ("reviews", lambda: None),
        ("reviews-fanin", lambda: None),
        ("github-address-pull-request-reviews", lambda: None),
    ]
    # Should not raise — this is what gremlins resume does.
    run_stages(pipeline_stages, resume_from=data["stage"])


def test_parallel_child_set_stage_with_sub_stage_payload_writes_parent_as_stage(
    tmp_path, state_root
):
    gremlin_id = "gr-parent-stage-pin-sub"
    sf = _make_state(state_root, gremlin_id)

    state = State(
        client=FakeClaudeClient(),
        session_dir=tmp_path,
        gremlin_id=gremlin_id,
        parent_stage="reviews",
    )

    # Simulate a stage that calls set_stage with a dict sub_stage (e.g. review_code.py).
    state.set_stage("github-review-pull-request", {"model": "claude-opus"})

    data = _read_state(sf)
    assert data["stage"] == "reviews"
    assert data["sub_stage"] == "github-review-pull-request"
