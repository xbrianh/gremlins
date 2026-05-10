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

import gremlins.state as state_mod
from gremlins.clients.fake import FakeClaudeClient
from gremlins.runner import run_stages
from gremlins.stages import StageContext
from gremlins.stages.parallel import ParallelStage

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


@pytest.fixture
def state_root(tmp_path: pathlib.Path, monkeypatch):
    root = tmp_path / "state"
    monkeypatch.setattr("gremlins.paths.state_root", lambda: root)
    return root


def _make_state(state_root: pathlib.Path, gr_id: str) -> pathlib.Path:
    state_dir = state_root / gr_id
    state_dir.mkdir(parents=True)
    sf = state_dir / "state.json"
    sf.write_text(json.dumps({"id": gr_id, "stage": ""}), encoding="utf-8")
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
    gr_id=None,
    project_root: pathlib.Path | None = None,
) -> list:
    if set_stage_fn is None:

        def set_stage_fn(_n):
            return None

    if project_root is None:
        project_root = pathlib.Path.cwd()
    return ParallelStage(
        group_name,
        child_runners,
        max_concurrent=max_concurrent,
        set_stage_fn=set_stage_fn,
        cancel_on_bail=cancel_on_bail,
        bail_policy=bail_policy,
        gr_id=gr_id,
        project_root=project_root,
    ).build_runtime_stages()


# ---------------------------------------------------------------------------
# Per-child bail shards
# ---------------------------------------------------------------------------


def test_emit_bail_with_child_key_writes_shard(state_root):
    gr_id = "gr-shard-a"
    sf = _make_state(state_root, gr_id)

    state_mod.emit_bail(gr_id, "other", "child A bailed", child_key="child-a")

    data = _read_state(sf)
    assert "bail_class" not in data or not data.get("bail_class")
    assert data["parallel_bails"]["child-a"]["bail_class"] == "other"
    assert data["parallel_bails"]["child-a"]["bail_detail"] == "child A bailed"


def test_emit_bail_two_children_both_shards_present(state_root):
    gr_id = "gr-shard-two"
    sf = _make_state(state_root, gr_id)

    state_mod.emit_bail(gr_id, "other", "A", child_key="child-a")
    state_mod.emit_bail(gr_id, "security", "B", child_key="child-b")

    data = _read_state(sf)
    assert data["parallel_bails"]["child-a"]["bail_class"] == "other"
    assert data["parallel_bails"]["child-b"]["bail_class"] == "security"
    # Top-level bail_class must not be set.
    assert not data.get("bail_class")


def test_check_bail_child_key_reads_only_own_shard(state_root):
    gr_id = "gr-check-shard"
    _make_state(state_root, gr_id)

    state_mod.emit_bail(gr_id, "other", "A failed", child_key="child-a")

    # child-a's shard has a bail → check_bail raises.
    with pytest.raises(RuntimeError, match="bailed"):
        state_mod.check_bail(gr_id, "test", child_key="child-a")

    # child-b's shard is empty → no raise.
    state_mod.check_bail(gr_id, "test", child_key="child-b")


def test_check_bail_no_child_key_reads_top_level(state_root):
    gr_id = "gr-check-toplevel"
    _make_state(state_root, gr_id)

    # Only a child shard is set; top-level should be clear.
    state_mod.emit_bail(gr_id, "other", "child failed", child_key="child-a")
    state_mod.check_bail(gr_id, "top-level")  # should not raise

    # Now set a top-level bail.
    state_mod.emit_bail(gr_id, "security", "top-level thing")
    with pytest.raises(RuntimeError, match="bailed"):
        state_mod.check_bail(gr_id, "top-level")


# ---------------------------------------------------------------------------
# fcntl.flock race: concurrent patch_state calls — no lost updates
# ---------------------------------------------------------------------------


def test_patch_state_concurrent_no_lost_updates(state_root):
    gr_id = "gr-flock-race"
    _make_state(state_root, gr_id)

    errors: list[Exception] = []
    n_threads = 20

    def _increment():
        try:
            sf = state_mod.resolve_state_file(gr_id)
            assert sf is not None
            for _ in range(5):
                state_mod._locked_update(
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
    sf = state_mod.resolve_state_file(gr_id)
    assert sf is not None
    data = _read_state(sf)
    assert data["counter"] == n_threads * 5, (
        f"expected {n_threads * 5} increments, got {data['counter']} (lost updates)"
    )


# ---------------------------------------------------------------------------
# bail_policy via build_parallel_stages
# ---------------------------------------------------------------------------


def _make_simple_ctx(tmp_path: pathlib.Path, child_key: str) -> StageContext:
    return StageContext(
        client=FakeClaudeClient(),
        session_dir=tmp_path / child_key,
        gr_id=None,
        child_key=child_key,
    )


def _build_fanin_test(
    tmp_path: pathlib.Path,
    state_root: pathlib.Path,
    gr_id: str,
    shards: dict[str, str],  # child_key -> bail_class (empty = no bail)
    bail_policy: str,
) -> tuple[pathlib.Path, list]:
    sf = _make_state(state_root, gr_id)
    # Pre-populate parallel_bails as fan-out+parallel would have done.
    parallel_bails = {}
    for key, bc in shards.items():
        if bc:
            parallel_bails[key] = {"bail_class": bc}
    state_mod.patch_state(gr_id, parallel_bails=parallel_bails)

    child_keys = list(shards.keys())
    children = [(k, _make_simple_ctx(tmp_path, k), lambda: None) for k in child_keys]

    # We need a git-less project root so fan-in's worktree logic is skipped.
    project_root = tmp_path / "nongit"
    project_root.mkdir()

    stages = _make_parallel_stages(
        "reviews",
        children,
        max_concurrent=None,
        set_stage_fn=lambda _n: None,
        cancel_on_bail=False,
        bail_policy=bail_policy,
        gr_id=gr_id,
        project_root=project_root,
    )
    return sf, stages


def test_bail_policy_any_one_bailed_sets_top_level(tmp_path, state_root):
    gr_id = "gr-policy-any"
    shards = {"child-a": "other", "child-b": ""}
    sf, stages = _build_fanin_test(tmp_path, state_root, gr_id, shards, "any")

    # Run just the fanin stage.
    with pytest.raises(RuntimeError, match="bailed"):
        stages[2][1]()  # fanin is index 2

    data = _read_state(sf)
    assert data.get("bail_class") == "other"
    assert "parallel_bails" not in data


def test_bail_policy_all_one_bailed_no_top_level(tmp_path, state_root):
    gr_id = "gr-policy-all-partial"
    shards = {"child-a": "other", "child-b": ""}
    sf, stages = _build_fanin_test(tmp_path, state_root, gr_id, shards, "all")

    # Only one bailed; policy=all requires all → no top-level bail.
    stages[2][1]()  # fanin should not raise

    data = _read_state(sf)
    assert not data.get("bail_class")
    assert "parallel_bails" not in data


def test_bail_policy_all_both_bailed_sets_top_level(tmp_path, state_root):
    gr_id = "gr-policy-all-both"
    shards = {"child-a": "other", "child-b": "reviewer_requested_changes"}
    sf, stages = _build_fanin_test(tmp_path, state_root, gr_id, shards, "all")

    with pytest.raises(RuntimeError, match="bailed"):
        stages[2][1]()

    data = _read_state(sf)
    assert data.get("bail_class")
    assert "parallel_bails" not in data


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

    ctx_a = StageContext(
        client=FakeClaudeClient(),
        session_dir=pathlib.Path("/tmp"),
        gr_id=None,
        child_key="a",
    )
    ctx_b = StageContext(
        client=FakeClaudeClient(),
        session_dir=pathlib.Path("/tmp"),
        gr_id=None,
        child_key="b",
    )
    ctx_c = StageContext(
        client=FakeClaudeClient(),
        session_dir=pathlib.Path("/tmp"),
        gr_id=None,
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
        gr_id=None,
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
    gr_id = "gr-fanin-resume"
    shards = {"child-a": "other", "child-b": ""}
    sf, stages = _build_fanin_test(tmp_path, state_root, gr_id, shards, "any")

    # Simulate resuming from fanin: only run the fanin stage.
    # Fanin raises because one child bailed; state must still be written.
    with pytest.raises(RuntimeError, match="bailed"):
        run_stages(stages[2:], resume_from="reviews-fanin")

    data = _read_state(sf)
    assert data.get("bail_class") == "other"
    assert "parallel_bails" not in data


def test_run_stages_resume_from_fanin_name(tmp_path, state_root):
    gr_id = "gr-resume-fanin-name"
    sf = _make_state(state_root, gr_id)
    state_mod.patch_state(
        gr_id,
        parallel_bails={"c": {"bail_class": "other"}},
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
        gr_id=gr_id,
        project_root=project_root,
    )

    # The three stage names should be reviews-fanout, reviews, reviews-fanin.
    names = [n for n, _ in stages]
    assert names == ["reviews-fanout", "reviews", "reviews-fanin"]

    # run_stages with resume_from=reviews-fanin skips fanout and parallel.
    with pytest.raises(RuntimeError, match="bailed"):
        run_stages(stages, resume_from="reviews-fanin")

    data = _read_state(sf)
    assert data.get("bail_class") == "other"
    assert "parallel_bails" not in data


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

    ctx_a = StageContext(
        client=FakeClaudeClient(), session_dir=tmp_path / "a", gr_id=None, child_key="a"
    )
    ctx_b = StageContext(
        client=FakeClaudeClient(), session_dir=tmp_path / "b", gr_id=None, child_key="b"
    )

    stages = _make_parallel_stages(
        "reviews",
        [("a", ctx_a, lambda: None), ("b", ctx_b, lambda: None)],
        max_concurrent=None,
        set_stage_fn=lambda _n: None,
        cancel_on_bail=False,
        bail_policy="any",
        gr_id=None,
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
    gr_id = "gr-resume-wt"
    _make_state(state_root, gr_id)

    repo = tmp_path / "repo"
    repo.mkdir()
    _init_git_repo(repo)

    def _make_ctx(name: str) -> StageContext:
        return StageContext(
            client=FakeClaudeClient(),
            session_dir=tmp_path / name,
            gr_id=gr_id,
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
        gr_id=gr_id,
        project_root=repo,
    )
    stages_run1[0][1]()  # fan-out only

    # state.json should now record both worktree paths.
    sf = state_mod.resolve_state_file(gr_id)
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
        gr_id=gr_id,
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
    gr_id = "gr-resume-fanout"
    _make_state(state_root, gr_id)

    repo = tmp_path / "repo"
    repo.mkdir()
    _init_git_repo(repo)

    def _make_ctx(name: str) -> StageContext:
        return StageContext(
            client=FakeClaudeClient(),
            session_dir=tmp_path / name,
            gr_id=gr_id,
            child_key=name,
        )

    stages_run1 = _make_parallel_stages(
        "reviews",
        [("a", _make_ctx("a"), lambda: None)],
        max_concurrent=None,
        set_stage_fn=lambda _n: None,
        cancel_on_bail=False,
        bail_policy="any",
        gr_id=gr_id,
        project_root=repo,
    )
    stages_run1[0][1]()  # fan-out

    sf = state_mod.resolve_state_file(gr_id)
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
        gr_id=gr_id,
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
    ctx = StageContext(
        client=FakeClaudeClient(),
        session_dir=pathlib.Path("/tmp"),
        gr_id=None,
        child_key="r1",
    )
    stages = _make_parallel_stages(
        "reviews",
        [("r1", ctx, lambda: None)],
        max_concurrent=None,
        set_stage_fn=lambda _n: None,
        cancel_on_bail=False,
        bail_policy="any",
        gr_id=None,
        project_root=pathlib.Path.cwd(),
    )
    names = [n for n, _ in stages]
    assert names == ["reviews-fanout", "reviews", "reviews-fanin"]


def test_parallel_all_children_complete_with_defaults():
    ran: list[str] = []
    ctx_a = StageContext(
        client=FakeClaudeClient(),
        session_dir=pathlib.Path("/tmp"),
        gr_id=None,
        child_key="a",
    )
    ctx_b = StageContext(
        client=FakeClaudeClient(),
        session_dir=pathlib.Path("/tmp"),
        gr_id=None,
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
        gr_id=None,
        project_root=pathlib.Path.cwd(),
    )

    # Run all three stages end-to-end (no git repo → fanout is a no-op).
    for _, fn in stages:
        fn()

    assert sorted(ran) == ["a", "b"]


def test_pipeline_cancel_on_bail_and_bail_policy_parsed(tmp_path):
    from gremlins.pipeline import load_pipeline

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
    pipeline = load_pipeline(p)
    entry = pipeline.stages[0]
    assert entry.cancel_on_bail is True
    assert entry.bail_policy == "all"


def test_pipeline_bail_policy_invalid_raises(tmp_path):
    from gremlins.pipeline import load_pipeline

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
        load_pipeline(p)
