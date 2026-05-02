"""Tests for gremlins.orchestrators.boss."""
from __future__ import annotations

import json
import os
import pathlib

import pytest

import gremlins.git as git_mod
import gremlins.orchestrators.boss as boss_mod
from gremlins.orchestrators.boss import (
    _resolve_plan_source,
    _summarize_for_log,
    boss_main,
    get_child_bail_detail,
    get_child_bail_reason,
    init_boss_state,
    load_boss_state,
    save_boss_state,
)

FIXTURES_DIR = pathlib.Path(__file__).parent / "fixtures"


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _make_gremlin_state(tmp_path, gr_id="test-boss-aabb12"):
    """Write minimal state.json and directory for boss_main."""
    state_dir = tmp_path / gr_id
    state_dir.mkdir()
    project_root = tmp_path / "project"
    project_root.mkdir()
    workdir = tmp_path / "workdir"
    workdir.mkdir()
    (state_dir / "state.json").write_text(json.dumps({
        "id": gr_id,
        "kind": "bossgremlin",
        "project_root": str(project_root),
        "workdir": str(workdir),
        "status": "running",
    }))
    return state_dir, project_root, workdir


def _common_boss_patches(monkeypatch, tmp_path, gr_id):
    """Shared monkeypatches for boss_main integration tests."""
    monkeypatch.setenv("GR_ID", gr_id)
    monkeypatch.setattr(boss_mod, "STATE_ROOT", str(tmp_path))
    monkeypatch.setattr(boss_mod, "_stop_requested", False)
    # Stub out set_stage so tests don't touch the developer's real ~/.local/state.
    # (state.set_stage resolves XDG_STATE_HOME at call time; patching the name
    # imported into boss_mod keeps all boss call-sites covered.)
    monkeypatch.setattr(boss_mod, "set_stage", lambda *a, **kw: None)
    monkeypatch.setattr(boss_mod, "get_head_ref", lambda p: "abc123def456abc1")
    monkeypatch.setattr(boss_mod, "get_current_branch", lambda p: "main")
    # Stub git_head_of_workdir so tests don't need a real git worktree.
    # Individual tests that care about specific SHA values can override this.
    monkeypatch.setattr(git_mod, "git_head_of_workdir", lambda w: "aaaa1111bbbb2222cccc3333dddd4444eeee5555")


# ---------------------------------------------------------------------------
# _summarize_for_log
# ---------------------------------------------------------------------------

def test_summarize_empty():
    assert _summarize_for_log("") == ""


def test_summarize_single_line():
    assert _summarize_for_log("hello world") == "hello world"


def test_summarize_collapses_newlines():
    assert _summarize_for_log("line one\nline two\nline three") == "line one line two line three"


def test_summarize_truncates():
    long_text = "x" * 300
    result = _summarize_for_log(long_text, limit=240)
    assert len(result) == 240
    assert result.endswith("...")


def test_summarize_exact_limit():
    text = "y" * 240
    assert _summarize_for_log(text, limit=240) == text


# ---------------------------------------------------------------------------
# get_child_bail_reason / get_child_bail_detail
# ---------------------------------------------------------------------------

def test_get_child_bail_reason_missing_state(tmp_path):
    orig = boss_mod.STATE_ROOT
    boss_mod.STATE_ROOT = str(tmp_path)
    try:
        assert get_child_bail_reason("no-such-child") == ""
    finally:
        boss_mod.STATE_ROOT = orig


def test_get_child_bail_reason_reads_bail_reason(tmp_path):
    child_dir = tmp_path / "child-aaa"
    child_dir.mkdir()
    (child_dir / "state.json").write_text(json.dumps({
        "bail_reason": "structural",
        "bail_class": "something_else",
    }))
    orig = boss_mod.STATE_ROOT
    boss_mod.STATE_ROOT = str(tmp_path)
    try:
        assert get_child_bail_reason("child-aaa") == "structural"
    finally:
        boss_mod.STATE_ROOT = orig


def test_get_child_bail_reason_falls_back_to_bail_class(tmp_path):
    child_dir = tmp_path / "child-bbb"
    child_dir.mkdir()
    (child_dir / "state.json").write_text(json.dumps({"bail_class": "unsalvageable"}))
    orig = boss_mod.STATE_ROOT
    boss_mod.STATE_ROOT = str(tmp_path)
    try:
        assert get_child_bail_reason("child-bbb") == "unsalvageable"
    finally:
        boss_mod.STATE_ROOT = orig


def test_get_child_bail_detail_missing(tmp_path):
    orig = boss_mod.STATE_ROOT
    boss_mod.STATE_ROOT = str(tmp_path)
    try:
        assert get_child_bail_detail("no-such-child") == ""
    finally:
        boss_mod.STATE_ROOT = orig


def test_get_child_bail_detail_reads_field(tmp_path):
    child_dir = tmp_path / "child-ccc"
    child_dir.mkdir()
    (child_dir / "state.json").write_text(json.dumps({"bail_detail": "phase A failed: no plan found"}))
    orig = boss_mod.STATE_ROOT
    boss_mod.STATE_ROOT = str(tmp_path)
    try:
        assert get_child_bail_detail("child-ccc") == "phase A failed: no plan found"
    finally:
        boss_mod.STATE_ROOT = orig


# ---------------------------------------------------------------------------
# init_boss_state / save / load round-trip
# ---------------------------------------------------------------------------

def test_init_boss_state_schema(tmp_path):
    state = init_boss_state(
        spec_path="/tmp/spec.md",
        chain_kind="local",
        chain_base_ref="abc123def456",
        target_branch="main",
        state_dir=str(tmp_path),
    )
    assert state["spec_path"] == "/tmp/spec.md"
    assert state["chain_kind"] == "local"
    assert state["chain_base_ref"] == "abc123def456"
    assert state["target_branch"] == "main"
    assert state["current_plan"] == "/tmp/spec.md"
    assert state["handoff_count"] == 0
    assert state["current_child_id"] is None
    assert state["children"] == []
    assert state["handoff_records"] == []
    assert state["operator_followups"] == []

    on_disk = json.loads((tmp_path / "boss_state.json").read_text())
    assert on_disk == state


def test_save_load_round_trip(tmp_path):
    state = {
        "spec_path": "/tmp/spec.md",
        "chain_kind": "gh",
        "chain_base_ref": "deadbeef12345678",
        "target_branch": "main",
        "current_plan": "/tmp/spec.md",
        "handoff_count": 2,
        "current_child_id": "child-xyz-abc123",
        "children": [{"id": "child-abc", "outcome": "landed"}],
        "handoff_records": [],
        "operator_followups": ["Do task X"],
    }
    save_boss_state(str(tmp_path), state)
    loaded = load_boss_state(str(tmp_path))
    assert loaded == state


# ---------------------------------------------------------------------------
# Resume fixture: load sample boss_state.json
# ---------------------------------------------------------------------------

def test_resume_fixture_parses():
    """boss_state_sample.json loads without error and has the expected shape."""
    fixture = FIXTURES_DIR / "boss_state_sample.json"
    state = json.loads(fixture.read_text())

    assert state["chain_kind"] == "gh"
    assert state["handoff_count"] == 5
    assert len(state["children"]) == 4
    assert state["current_child_id"] is not None
    assert all("id" in c and "outcome" in c for c in state["children"])

    required_record_keys = {
        "timestamp", "n", "plan_in", "plan_out", "signal_file",
        "exit_state", "child_plan", "bail_reason", "operator_followups",
    }
    assert all(required_record_keys <= set(r.keys()) for r in state["handoff_records"])


def test_resume_fixture_child_outcomes():
    """Completed children have expected outcomes (landed or rescued-then-landed)."""
    state = json.loads((FIXTURES_DIR / "boss_state_sample.json").read_text())
    outcomes = {c["outcome"] for c in state["children"]}
    assert outcomes <= {"landed", "rescued-then-landed"}


def test_resume_fixture_handoff_exit_states():
    """All handoff records in the fixture have recognized exit_states."""
    state = json.loads((FIXTURES_DIR / "boss_state_sample.json").read_text())
    valid = {"next-plan", "chain-done", "bail"}
    for rec in state["handoff_records"]:
        assert rec["exit_state"] in valid


# ---------------------------------------------------------------------------
# Child sequencing: handoff → launch → wait → land → handoff → chain-done
# ---------------------------------------------------------------------------

def test_chain_done_after_one_child(tmp_path, monkeypatch):
    """Boss completes: handoff1→next-plan, child runs and lands, handoff2→chain-done."""
    gr_id = "test-boss-aabb12"
    state_dir, project_root, workdir = _make_gremlin_state(tmp_path, gr_id)
    _common_boss_patches(monkeypatch, tmp_path, gr_id)

    spec = tmp_path / "spec.md"
    spec.write_text("# Spec\n")
    child_plan = tmp_path / "child-plan.md"
    child_plan.write_text("# Child plan\n")

    calls = []
    handoff_results = iter([
        ("next-plan", {"exit_state": "next-plan", "child_plan": str(child_plan), "operator_followups": []}),
        ("chain-done", {"exit_state": "chain-done", "operator_followups": []}),
    ])

    def fake_run_handoff(gr_id, state_dir, boss_state, project_root, boss_workdir, model):
        exit_state, sig = next(handoff_results)
        n = boss_state["handoff_count"] + 1
        out_path = os.path.join(state_dir, f"handoff-{n:03d}.md")
        pathlib.Path(out_path).write_text(f"# Handoff {n}\n")
        boss_state["handoff_count"] = n
        boss_state["current_plan"] = out_path
        boss_state["operator_followups"] = sig.get("operator_followups", [])
        boss_state["handoff_records"].append({
            "timestamp": "2026-01-01T00:00:00Z",
            "n": n,
            "plan_in": boss_state["spec_path"],
            "plan_out": out_path,
            "signal_file": out_path.replace(".md", ".state.json"),
            "exit_state": exit_state,
            "child_plan": sig.get("child_plan"),
            "bail_reason": None,
            "operator_followups": sig.get("operator_followups", []),
        })
        calls.append(("handoff", exit_state))
        return exit_state, sig

    def fake_launch_child(gr_id, launch_kind, child_plan_path):
        calls.append(("launch", launch_kind))
        child_id = "child-abc-123456"
        child_dir = tmp_path / child_id
        child_dir.mkdir(exist_ok=True)
        (child_dir / "state.json").write_text(json.dumps({"exit_code": 0}))
        (child_dir / "finished").write_text("")
        return child_id

    def fake_land_child(child_id, into_dir=""):
        calls.append(("land", child_id))
        return True

    monkeypatch.setattr(boss_mod, "run_handoff", fake_run_handoff)
    monkeypatch.setattr(boss_mod, "launch_child", fake_launch_child)
    monkeypatch.setattr(boss_mod, "land_child", fake_land_child)

    result = boss_main(["--plan", str(spec), "--chain-kind", "local"])
    assert result == 0
    assert calls == [
        ("handoff", "next-plan"),
        ("launch", "localgremlin"),
        ("land", "child-abc-123456"),
        ("handoff", "chain-done"),
    ]

    final_state = load_boss_state(str(state_dir))
    assert len(final_state["children"]) == 1
    assert final_state["children"][0] == {"id": "child-abc-123456", "outcome": "landed"}
    assert final_state["current_child_id"] is None


def test_chain_uses_ghgremlin_for_gh_kind(tmp_path, monkeypatch):
    """Boss passes 'ghgremlin' as the launch_kind when chain-kind=gh."""
    gr_id = "test-boss-gh-cc3344"
    state_dir, project_root, workdir = _make_gremlin_state(tmp_path, gr_id)
    _common_boss_patches(monkeypatch, tmp_path, gr_id)
    monkeypatch.setattr(boss_mod, "get_default_branch", lambda p: "main")
    monkeypatch.setattr(boss_mod, "get_remote_branch_sha", lambda p, b: "deadbeef12345678")

    spec = tmp_path / "spec.md"
    spec.write_text("# Spec\n")
    child_plan = tmp_path / "child-plan.md"
    child_plan.write_text("# Child plan\n")

    launch_kinds = []
    handoff_results = iter([
        ("next-plan", {"exit_state": "next-plan", "child_plan": str(child_plan), "operator_followups": []}),
        ("chain-done", {"exit_state": "chain-done", "operator_followups": []}),
    ])

    def fake_run_handoff(gr_id, state_dir, boss_state, project_root, boss_workdir, model):
        exit_state, sig = next(handoff_results)
        n = boss_state["handoff_count"] + 1
        out_path = os.path.join(state_dir, f"handoff-{n:03d}.md")
        pathlib.Path(out_path).write_text(f"# Handoff {n}\n")
        boss_state["handoff_count"] = n
        boss_state["current_plan"] = out_path
        boss_state["operator_followups"] = sig.get("operator_followups", [])
        boss_state["handoff_records"].append({
            "timestamp": "2026-01-01T00:00:00Z", "n": n,
            "plan_in": boss_state["spec_path"], "plan_out": out_path,
            "signal_file": "", "exit_state": exit_state,
            "child_plan": sig.get("child_plan"), "bail_reason": None,
            "operator_followups": sig.get("operator_followups", []),
        })
        return exit_state, sig

    def fake_launch_child(gr_id, launch_kind, child_plan_path):
        launch_kinds.append(launch_kind)
        child_id = "child-gh-cc3344"
        child_dir = tmp_path / child_id
        child_dir.mkdir(exist_ok=True)
        (child_dir / "state.json").write_text(json.dumps({"exit_code": 0}))
        (child_dir / "finished").write_text("")
        return child_id

    monkeypatch.setattr(boss_mod, "run_handoff", fake_run_handoff)
    monkeypatch.setattr(boss_mod, "launch_child", fake_launch_child)
    monkeypatch.setattr(boss_mod, "land_child", lambda cid, into_dir="": True)

    result = boss_main(["--plan", str(spec), "--chain-kind", "gh"])
    assert result == 0
    assert launch_kinds == ["ghgremlin"]


def test_chain_bail_on_handoff(tmp_path, monkeypatch):
    """Boss calls die() when handoff returns bail."""
    gr_id = "test-boss-bail-dd5566"
    state_dir, project_root, workdir = _make_gremlin_state(tmp_path, gr_id)
    _common_boss_patches(monkeypatch, tmp_path, gr_id)

    spec = tmp_path / "spec.md"
    spec.write_text("# Spec\n")

    def fake_run_handoff(gr_id, state_dir, boss_state, project_root, boss_workdir, model):
        n = boss_state["handoff_count"] + 1
        out_path = os.path.join(state_dir, f"handoff-{n:03d}.md")
        pathlib.Path(out_path).write_text("# Handoff\n")
        boss_state["handoff_count"] = n
        boss_state["current_plan"] = out_path
        boss_state["operator_followups"] = []
        boss_state["handoff_records"].append({
            "timestamp": "2026-01-01T00:00:00Z", "n": n,
            "plan_in": boss_state["spec_path"], "plan_out": out_path,
            "signal_file": "", "exit_state": "bail",
            "child_plan": None, "bail_reason": "spec is done", "operator_followups": [],
        })
        return "bail", {"exit_state": "bail", "reason": "spec is done"}

    monkeypatch.setattr(boss_mod, "run_handoff", fake_run_handoff)

    with pytest.raises(SystemExit) as exc_info:
        boss_main(["--plan", str(spec), "--chain-kind", "local"])
    assert exc_info.value.code == 1


# ---------------------------------------------------------------------------
# Handoff signal parsing: operator_followups separation from child_plan
# ---------------------------------------------------------------------------

def test_operator_followups_stored_in_boss_state(tmp_path, monkeypatch):
    """operator_followups from handoff signal are persisted in boss_state, not forwarded to child.

    The contract under test: boss stores operator_followups in boss_state["operator_followups"]
    and passes the child_plan path (not the operator items) to launch_child.
    """
    gr_id = "test-boss-opfollowup-ee7788"
    state_dir, project_root, workdir = _make_gremlin_state(tmp_path, gr_id)
    _common_boss_patches(monkeypatch, tmp_path, gr_id)

    spec = tmp_path / "spec.md"
    spec.write_text("# Spec\n")
    child_plan = tmp_path / "child-plan.md"
    child_plan.write_text("# Child plan\nDo the implementation.\n")

    operator_items = ["After landing: run sync.sh push", "After landing: verify e2e"]
    launch_args = []  # (launch_kind, child_plan_path) captured per call

    handoff_results = iter([
        (
            "next-plan",
            {
                "exit_state": "next-plan",
                "child_plan": str(child_plan),
                "operator_followups": operator_items,
            },
        ),
        ("chain-done", {"exit_state": "chain-done", "operator_followups": operator_items}),
    ])

    def fake_run_handoff(gr_id, state_dir, boss_state, project_root, boss_workdir, model):
        exit_state, sig = next(handoff_results)
        n = boss_state["handoff_count"] + 1
        out_path = os.path.join(state_dir, f"handoff-{n:03d}.md")
        pathlib.Path(out_path).write_text(f"# Handoff {n}\n")
        boss_state["handoff_count"] = n
        boss_state["current_plan"] = out_path
        boss_state["operator_followups"] = sig.get("operator_followups", [])
        boss_state["handoff_records"].append({
            "timestamp": "2026-01-01T00:00:00Z", "n": n,
            "plan_in": boss_state["spec_path"], "plan_out": out_path,
            "signal_file": "", "exit_state": exit_state,
            "child_plan": sig.get("child_plan"), "bail_reason": None,
            "operator_followups": sig.get("operator_followups", []),
        })
        return exit_state, sig

    def fake_launch_child(gr_id, launch_kind, child_plan_path):
        launch_args.append((launch_kind, child_plan_path))
        child_id = "child-op-test-ff9900"
        child_dir = tmp_path / child_id
        child_dir.mkdir(exist_ok=True)
        (child_dir / "state.json").write_text(json.dumps({"exit_code": 0}))
        (child_dir / "finished").write_text("")
        return child_id

    monkeypatch.setattr(boss_mod, "run_handoff", fake_run_handoff)
    monkeypatch.setattr(boss_mod, "launch_child", fake_launch_child)
    monkeypatch.setattr(boss_mod, "land_child", lambda cid, into_dir="": True)

    result = boss_main(["--plan", str(spec), "--chain-kind", "local"])
    assert result == 0

    # Boss launched exactly one child using the child_plan path from the handoff signal.
    assert len(launch_args) == 1
    _, launched_plan_path = launch_args[0]
    assert launched_plan_path == str(child_plan)

    # operator_followups are stored in boss_state, not forwarded as a separate argument.
    final_state = load_boss_state(str(state_dir))
    assert final_state["operator_followups"] == operator_items


# ---------------------------------------------------------------------------
# Resume path: boss_state.json with current_child_id set
# ---------------------------------------------------------------------------

def test_resume_picks_up_in_flight_child(tmp_path, monkeypatch):
    """When boss_state.json has current_child_id, boss resumes from the wait loop."""
    gr_id = "test-resume-boss-aabb12"
    state_dir, project_root, workdir = _make_gremlin_state(tmp_path, gr_id)
    _common_boss_patches(monkeypatch, tmp_path, gr_id)

    spec = tmp_path / "spec.md"
    spec.write_text("# Spec\n")

    child_id = "in-flight-child-cc3344"
    child_dir = tmp_path / child_id
    child_dir.mkdir()
    (child_dir / "state.json").write_text(json.dumps({"exit_code": 0}))
    (child_dir / "finished").write_text("")

    # Pre-populate boss_state.json with current_child_id already set.
    boss_state = {
        "spec_path": str(spec),
        "chain_kind": "local",
        "chain_base_ref": "abc123def456abc1",
        "target_branch": "main",
        "current_plan": str(spec),
        "handoff_count": 1,
        "current_child_id": child_id,
        "children": [],
        "handoff_records": [],
        "operator_followups": [],
    }
    (state_dir / "boss_state.json").write_text(json.dumps(boss_state))

    calls = []

    def fake_run_handoff(gr_id, state_dir, boss_state, project_root, boss_workdir, model):
        n = boss_state["handoff_count"] + 1
        out_path = os.path.join(state_dir, f"handoff-{n:03d}.md")
        pathlib.Path(out_path).write_text(f"# Handoff {n}\n")
        boss_state["handoff_count"] = n
        boss_state["current_plan"] = out_path
        boss_state["operator_followups"] = []
        boss_state["handoff_records"].append({
            "timestamp": "2026-01-01T00:00:00Z", "n": n,
            "plan_in": str(spec), "plan_out": out_path,
            "signal_file": "", "exit_state": "chain-done",
            "child_plan": None, "bail_reason": None, "operator_followups": [],
        })
        calls.append("handoff")
        return "chain-done", {"exit_state": "chain-done", "operator_followups": []}

    def fake_land_child(cid, into_dir=""):
        calls.append(("land", cid))
        return True

    monkeypatch.setattr(boss_mod, "run_handoff", fake_run_handoff)
    monkeypatch.setattr(boss_mod, "land_child", fake_land_child)

    result = boss_main(["--plan", str(spec), "--chain-kind", "local"])
    assert result == 0

    # Boss should have landed the in-flight child first, then run handoff.
    assert calls == [("land", child_id), "handoff"]

    final_state = load_boss_state(str(state_dir))
    assert final_state["children"][0] == {"id": child_id, "outcome": "landed"}


def test_resume_fixture_in_boss_main(tmp_path, monkeypatch):
    """Load boss_state_sample.json fixture, simulate resume, verify correct child index."""
    gr_id = "test-resume-fixture-boss-112233"
    state_dir, project_root, workdir = _make_gremlin_state(tmp_path, gr_id)
    _common_boss_patches(monkeypatch, tmp_path, gr_id)

    # Load the fixture and adapt paths to tmp_path.
    fixture_state = json.loads((FIXTURES_DIR / "boss_state_sample.json").read_text())
    child_id = fixture_state["current_child_id"]

    spec = tmp_path / "spec.md"
    spec.write_text("# Spec\n")
    fixture_state["spec_path"] = str(spec)
    fixture_state["current_plan"] = str(spec)

    # Create child dir with finished marker.
    child_dir = tmp_path / child_id
    child_dir.mkdir()
    (child_dir / "state.json").write_text(json.dumps({"exit_code": 0}))
    (child_dir / "finished").write_text("")

    (state_dir / "boss_state.json").write_text(json.dumps(fixture_state))

    calls = []

    def fake_run_handoff(gr_id, state_dir, boss_state, project_root, boss_workdir, model):
        n = boss_state["handoff_count"] + 1
        out_path = os.path.join(state_dir, f"handoff-{n:03d}.md")
        pathlib.Path(out_path).write_text(f"# Handoff {n}\n")
        boss_state["handoff_count"] = n
        boss_state["current_plan"] = out_path
        boss_state["operator_followups"] = []
        boss_state["handoff_records"].append({
            "timestamp": "2026-01-01T00:00:00Z", "n": n,
            "plan_in": str(spec), "plan_out": out_path,
            "signal_file": "", "exit_state": "chain-done",
            "child_plan": None, "bail_reason": None, "operator_followups": [],
        })
        calls.append("handoff")
        return "chain-done", {"exit_state": "chain-done", "operator_followups": []}

    def fake_land_child(cid, into_dir=""):
        calls.append(("land", cid))
        return True

    monkeypatch.setattr(boss_mod, "run_handoff", fake_run_handoff)
    monkeypatch.setattr(boss_mod, "land_child", fake_land_child)

    result = boss_main(["--plan", str(spec), "--chain-kind", "gh"])
    assert result == 0

    # Resumed with the in-flight child from the fixture.
    assert ("land", child_id) in calls
    final_state = load_boss_state(str(state_dir))
    # The 4 completed children from the fixture + the resumed one = 5 total.
    assert len(final_state["children"]) == 5


# ---------------------------------------------------------------------------
# Rescue-then-land
# ---------------------------------------------------------------------------

def test_rescue_then_land(tmp_path, monkeypatch):
    """Child fails rescue once then succeeds; outcome recorded as rescued-then-landed."""
    gr_id = "test-boss-rescue-gg9900"
    state_dir, project_root, workdir = _make_gremlin_state(tmp_path, gr_id)
    _common_boss_patches(monkeypatch, tmp_path, gr_id)

    spec = tmp_path / "spec.md"
    spec.write_text("# Spec\n")
    child_plan = tmp_path / "child-plan.md"
    child_plan.write_text("# Child plan\n")

    # Child starts failed, then succeeds after rescue.
    child_id = "rescue-child-hh1122"
    child_dir = tmp_path / child_id
    child_dir.mkdir()
    # Initially failed (exit_code != 0, finished marker present).
    child_state = {"exit_code": 1}
    (child_dir / "state.json").write_text(json.dumps(child_state))
    (child_dir / "finished").write_text("")

    calls = []
    rescue_call_count = [0]

    handoff_results = iter([
        ("next-plan", {"exit_state": "next-plan", "child_plan": str(child_plan), "operator_followups": []}),
        ("chain-done", {"exit_state": "chain-done", "operator_followups": []}),
    ])

    def fake_run_handoff(gr_id, state_dir, boss_state, project_root, boss_workdir, model):
        exit_state, sig = next(handoff_results)
        n = boss_state["handoff_count"] + 1
        out_path = os.path.join(state_dir, f"handoff-{n:03d}.md")
        pathlib.Path(out_path).write_text(f"# Handoff {n}\n")
        boss_state["handoff_count"] = n
        boss_state["current_plan"] = out_path
        boss_state["operator_followups"] = sig.get("operator_followups", [])
        boss_state["handoff_records"].append({
            "timestamp": "2026-01-01T00:00:00Z", "n": n,
            "plan_in": boss_state["spec_path"], "plan_out": out_path,
            "signal_file": "", "exit_state": exit_state,
            "child_plan": sig.get("child_plan"), "bail_reason": None,
            "operator_followups": sig.get("operator_followups", []),
        })
        calls.append(("handoff", exit_state))
        return exit_state, sig

    def fake_launch_child(gr_id, launch_kind, child_plan_path):
        calls.append(("launch", launch_kind))
        return child_id

    def fake_rescue_child(cid):
        calls.append(("rescue", cid))
        rescue_call_count[0] += 1
        # After rescue, flip the child to success.
        (child_dir / "state.json").write_text(json.dumps({"exit_code": 0}))
        return True

    def fake_land_child(cid, into_dir=""):
        calls.append(("land", cid))
        return True

    monkeypatch.setattr(boss_mod, "run_handoff", fake_run_handoff)
    monkeypatch.setattr(boss_mod, "launch_child", fake_launch_child)
    monkeypatch.setattr(boss_mod, "rescue_child", fake_rescue_child)
    monkeypatch.setattr(boss_mod, "land_child", fake_land_child)

    result = boss_main(["--plan", str(spec), "--chain-kind", "local"])
    assert result == 0

    assert calls == [
        ("handoff", "next-plan"),
        ("launch", "localgremlin"),
        ("rescue", child_id),
        ("land", child_id),
        ("handoff", "chain-done"),
    ]

    final_state = load_boss_state(str(state_dir))
    assert final_state["children"][0] == {"id": child_id, "outcome": "rescued-then-landed"}


def test_bail_after_rescue_refused(tmp_path, monkeypatch):
    """Boss halts (die) when rescue is refused for a failed child."""
    gr_id = "test-boss-bail-rescue-ii3344"
    state_dir, project_root, workdir = _make_gremlin_state(tmp_path, gr_id)
    _common_boss_patches(monkeypatch, tmp_path, gr_id)

    spec = tmp_path / "spec.md"
    spec.write_text("# Spec\n")
    child_plan = tmp_path / "child-plan.md"
    child_plan.write_text("# Child plan\n")

    child_id = "bail-child-jj5566"
    child_dir = tmp_path / child_id
    child_dir.mkdir()
    (child_dir / "state.json").write_text(json.dumps({
        "exit_code": 1,
        "bail_reason": "unsalvageable",
    }))
    (child_dir / "finished").write_text("")

    def fake_run_handoff(gr_id, state_dir, boss_state, project_root, boss_workdir, model):
        n = boss_state["handoff_count"] + 1
        out_path = os.path.join(state_dir, f"handoff-{n:03d}.md")
        pathlib.Path(out_path).write_text(f"# Handoff {n}\n")
        boss_state["handoff_count"] = n
        boss_state["current_plan"] = out_path
        boss_state["operator_followups"] = []
        boss_state["handoff_records"].append({
            "timestamp": "2026-01-01T00:00:00Z", "n": n,
            "plan_in": boss_state["spec_path"], "plan_out": out_path,
            "signal_file": "", "exit_state": "next-plan",
            "child_plan": str(child_plan), "bail_reason": None, "operator_followups": [],
        })
        return "next-plan", {"exit_state": "next-plan", "child_plan": str(child_plan), "operator_followups": []}

    monkeypatch.setattr(boss_mod, "run_handoff", fake_run_handoff)
    monkeypatch.setattr(boss_mod, "launch_child", lambda *a: child_id)
    monkeypatch.setattr(boss_mod, "rescue_child", lambda cid: False)

    with pytest.raises(SystemExit) as exc_info:
        boss_main(["--plan", str(spec), "--chain-kind", "local"])
    assert exc_info.value.code == 1

    final_state = load_boss_state(str(state_dir))
    child_entry = final_state["children"][0]
    assert child_entry["id"] == child_id
    assert "bailed" in child_entry["outcome"]
    assert final_state["current_child_id"] is None


# ---------------------------------------------------------------------------
# _resolve_plan_source: file path, issue ref, idempotent rescue
# ---------------------------------------------------------------------------

def test_resolve_plan_source_file(tmp_path):
    """File path inputs are copied verbatim into <state-dir>/spec.md."""
    state_dir = tmp_path / "state"
    state_dir.mkdir()
    src = tmp_path / "in.md"
    src.write_text("# Spec\nContent.\n")

    spec_path, issue_url, issue_num = _resolve_plan_source(str(src), str(state_dir))

    assert spec_path == str(state_dir / "spec.md")
    assert (state_dir / "spec.md").read_text() == "# Spec\nContent.\n"
    assert issue_url == ""
    assert issue_num == ""


def test_resolve_plan_source_empty_file(tmp_path):
    """Empty file inputs are rejected."""
    state_dir = tmp_path / "state"
    state_dir.mkdir()
    src = tmp_path / "empty.md"
    src.write_text("")

    with pytest.raises(SystemExit):
        _resolve_plan_source(str(src), str(state_dir))


def test_resolve_plan_source_issue_ref(tmp_path, monkeypatch):
    """Issue refs fetch the body via gh and snapshot it to spec.md."""
    state_dir = tmp_path / "state"
    state_dir.mkdir()

    monkeypatch.setattr(boss_mod, "get_repo", lambda: "owner/repo")
    monkeypatch.setattr(
        boss_mod, "view_issue",
        lambda ref, repo: {
            "number": 42,
            "url": "https://github.com/owner/repo/issues/42",
            "body": "# Issue Spec\nFetched from gh.\n",
        },
    )
    import shutil as _shutil
    monkeypatch.setattr(_shutil, "which", lambda n: "/fake/gh" if n == "gh" else None)

    spec_path, issue_url, issue_num = _resolve_plan_source("42", str(state_dir))

    assert spec_path == str(state_dir / "spec.md")
    body = (state_dir / "spec.md").read_text()
    assert "# Issue Spec" in body
    assert "Fetched from gh." in body
    assert issue_url == "https://github.com/owner/repo/issues/42"
    assert issue_num == "42"


def test_resolve_plan_source_cross_repo_issue_ref(tmp_path, monkeypatch):
    """``owner/repo#42`` resolves against the named repo, not get_repo()."""
    state_dir = tmp_path / "state"
    state_dir.mkdir()

    captured = {}

    def fake_view_issue(ref, repo):
        captured["ref"] = ref
        captured["repo"] = repo
        return {
            "number": 7,
            "url": "https://github.com/other/proj/issues/7",
            "body": "# Cross-repo\nspec.\n",
        }

    monkeypatch.setattr(boss_mod, "get_repo", lambda: "owner/repo")
    monkeypatch.setattr(boss_mod, "view_issue", fake_view_issue)
    import shutil as _shutil
    monkeypatch.setattr(_shutil, "which", lambda n: "/fake/gh" if n == "gh" else None)

    spec_path, issue_url, issue_num = _resolve_plan_source(
        "other/proj#7", str(state_dir)
    )

    assert captured == {"ref": "7", "repo": "other/proj"}
    assert issue_url == "https://github.com/other/proj/issues/7"
    assert issue_num == "7"
    assert (state_dir / "spec.md").read_text().startswith("# Cross-repo")


def test_resolve_plan_source_unknown_shape(tmp_path, monkeypatch):
    """Non-file, non-issue-ref inputs fail fast."""
    state_dir = tmp_path / "state"
    state_dir.mkdir()
    monkeypatch.setattr(boss_mod, "get_repo", lambda: "owner/repo")
    import shutil as _shutil
    monkeypatch.setattr(_shutil, "which", lambda n: "/fake/gh" if n == "gh" else None)

    with pytest.raises(SystemExit):
        _resolve_plan_source("not-a-ref", str(state_dir))


def test_resolve_plan_source_idempotent(tmp_path, monkeypatch):
    """A pre-existing non-empty spec.md is reused without re-fetching."""
    state_dir = tmp_path / "state"
    state_dir.mkdir()
    (state_dir / "spec.md").write_text("# Already snapshotted\n")

    fetch_called = []

    def boom(*a, **kw):
        fetch_called.append(True)
        raise AssertionError("view_issue should not be called when snapshot exists")

    monkeypatch.setattr(boss_mod, "view_issue", boom)
    monkeypatch.setattr(boss_mod, "get_repo", lambda: (_ for _ in ()).throw(AssertionError("get_repo should not be called")))

    spec_path, issue_url, issue_num = _resolve_plan_source("42", str(state_dir))

    assert spec_path == str(state_dir / "spec.md")
    assert (state_dir / "spec.md").read_text() == "# Already snapshotted\n"
    assert fetch_called == []


def test_resolve_plan_source_idempotent_recovers_issue_metadata(tmp_path, monkeypatch):
    """On rescue, issue_url / issue_num are recovered from state.json so the
    issue link survives the snapshot-already-exists short-circuit. Regression
    for the bug where the idempotent path always returned empty strings."""
    state_dir = tmp_path / "state"
    state_dir.mkdir()
    (state_dir / "spec.md").write_text("# Already snapshotted\n")
    (state_dir / "state.json").write_text(json.dumps({
        "issue_url": "https://github.com/owner/repo/issues/77",
        "issue_num": "77",
    }))

    monkeypatch.setattr(
        boss_mod, "view_issue",
        lambda *a, **kw: (_ for _ in ()).throw(AssertionError("should not refetch")),
    )

    spec_path, issue_url, issue_num = _resolve_plan_source("77", str(state_dir))

    assert spec_path == str(state_dir / "spec.md")
    assert issue_url == "https://github.com/owner/repo/issues/77"
    assert issue_num == "77"


def test_resolve_plan_source_persists_issue_metadata_on_first_fetch(
    tmp_path, monkeypatch
):
    """First-run issue-ref path persists issue_url / issue_num to state.json
    so a crash before init_boss_state still leaves the rescue path able to
    recover the link."""
    xdg_home = tmp_path / "xdg"
    state_root = xdg_home / "claude-gremlins"
    state_dir = state_root / "test-boss-persist-aa1122"
    state_dir.mkdir(parents=True)
    (state_dir / "state.json").write_text(json.dumps({"id": "test-boss-persist-aa1122"}))
    monkeypatch.setenv("XDG_STATE_HOME", str(xdg_home))
    monkeypatch.setenv("GR_ID", "test-boss-persist-aa1122")

    monkeypatch.setattr(boss_mod, "get_repo", lambda: "owner/repo")
    monkeypatch.setattr(
        boss_mod, "view_issue",
        lambda ref, repo: {
            "number": 99,
            "url": "https://github.com/owner/repo/issues/99",
            "body": "# Spec\nbody.\n",
        },
    )
    import shutil as _shutil
    monkeypatch.setattr(_shutil, "which", lambda n: "/fake/gh" if n == "gh" else None)

    spec_path, issue_url, issue_num = _resolve_plan_source("99", str(state_dir))

    assert issue_url == "https://github.com/owner/repo/issues/99"
    assert issue_num == "99"
    state_data = json.loads((state_dir / "state.json").read_text())
    assert state_data.get("issue_url") == "https://github.com/owner/repo/issues/99"
    assert state_data.get("issue_num") == "99"


# ---------------------------------------------------------------------------
# boss_main smoke test: --plan <issue-ref> end-to-end snapshot
# ---------------------------------------------------------------------------

def test_boss_main_plan_issue_ref_snapshots_spec(tmp_path, monkeypatch):
    """boss_main with --plan <issue-ref> fetches the issue and snapshots spec.md."""
    gr_id = "test-boss-issue-aabb12"
    # Arrange XDG_STATE_HOME so boss_mod.STATE_ROOT and patch_state's
    # XDG-derived state file path agree, otherwise the description fill
    # would silently no-op and the assertion below would always pass.
    xdg_home = tmp_path / "xdg"
    state_root = xdg_home / "claude-gremlins"
    state_root.mkdir(parents=True)
    monkeypatch.setenv("XDG_STATE_HOME", str(xdg_home))

    state_dir, project_root, workdir = _make_gremlin_state(state_root, gr_id)
    _common_boss_patches(monkeypatch, state_root, gr_id)

    monkeypatch.setattr(boss_mod, "get_repo", lambda: "owner/repo")
    monkeypatch.setattr(
        boss_mod, "view_issue",
        lambda ref, repo: {
            "number": 80,
            "url": "https://github.com/owner/repo/issues/80",
            "body": "# Plan from issue\nDo a thing.\n",
        },
    )
    import shutil as _shutil
    monkeypatch.setattr(_shutil, "which", lambda n: "/fake/gh" if n == "gh" else None)

    # Short-circuit handoff at the very first step so we don't have to mock
    # an entire chain — we just want to verify spec.md and boss_state.json.
    def fake_run_handoff(gr_id, state_dir, boss_state, project_root, boss_workdir, model):
        n = boss_state["handoff_count"] + 1
        out_path = os.path.join(state_dir, f"handoff-{n:03d}.md")
        pathlib.Path(out_path).write_text("# Handoff\n")
        boss_state["handoff_count"] = n
        boss_state["current_plan"] = out_path
        boss_state["operator_followups"] = []
        boss_state["handoff_records"].append({
            "timestamp": "2026-01-01T00:00:00Z", "n": n,
            "plan_in": boss_state["spec_path"], "plan_out": out_path,
            "signal_file": "", "exit_state": "chain-done",
            "child_plan": None, "bail_reason": None, "operator_followups": [],
        })
        return "chain-done", {"exit_state": "chain-done", "operator_followups": []}

    monkeypatch.setattr(boss_mod, "run_handoff", fake_run_handoff)

    result = boss_main(["--plan", "80", "--chain-kind", "local"])
    assert result == 0

    snapshot = state_dir / "spec.md"
    assert snapshot.exists()
    body = snapshot.read_text()
    assert "Plan from issue" in body
    assert "Do a thing." in body

    final_state = load_boss_state(str(state_dir))
    assert final_state["spec_path"] == str(snapshot)
    assert final_state["issue_url"] == "https://github.com/owner/repo/issues/80"
    assert final_state["issue_num"] == "80"

    # _maybe_set_description_from_spec should have filled state.json's
    # description from the snapshot's first H1 — this confirms the H1 read
    # works for short specs (regression coverage for the StopIteration bug).
    state_data = json.loads((state_dir / "state.json").read_text())
    assert state_data.get("description") == "Plan from issue"


# ---------------------------------------------------------------------------
# land_child: boss worktree as landing target for local chains
# ---------------------------------------------------------------------------

def test_land_child_uses_boss_workdir_for_local_chain(tmp_path, monkeypatch):
    """For local chains, land_child is called with into_dir=boss_workdir so children
    land into the boss's isolated worktree rather than the user's project_root."""
    gr_id = "test-boss-into-aabb12"
    state_dir, project_root, workdir = _make_gremlin_state(tmp_path, gr_id)
    _common_boss_patches(monkeypatch, tmp_path, gr_id)

    spec = tmp_path / "spec.md"
    spec.write_text("# Spec\n")
    child_plan = tmp_path / "child-plan.md"
    child_plan.write_text("# Child plan\n")

    land_calls = []

    handoff_results = iter([
        ("next-plan", {"exit_state": "next-plan", "child_plan": str(child_plan), "operator_followups": []}),
        ("chain-done", {"exit_state": "chain-done", "operator_followups": []}),
    ])

    def fake_run_handoff(gr_id, state_dir, boss_state, project_root, boss_workdir, model):
        exit_state, sig = next(handoff_results)
        n = boss_state["handoff_count"] + 1
        out_path = os.path.join(state_dir, f"handoff-{n:03d}.md")
        pathlib.Path(out_path).write_text(f"# Handoff {n}\n")
        boss_state["handoff_count"] = n
        boss_state["current_plan"] = out_path
        boss_state["operator_followups"] = sig.get("operator_followups", [])
        boss_state["handoff_records"].append({
            "timestamp": "2026-01-01T00:00:00Z", "n": n,
            "plan_in": boss_state["spec_path"], "plan_out": out_path,
            "signal_file": "", "exit_state": exit_state,
            "child_plan": sig.get("child_plan"), "bail_reason": None,
            "operator_followups": sig.get("operator_followups", []),
        })
        return exit_state, sig

    def fake_launch_child(gr_id, launch_kind, child_plan_path):
        child_id = "child-into-test-bb3344"
        child_dir = tmp_path / child_id
        child_dir.mkdir(exist_ok=True)
        (child_dir / "state.json").write_text(json.dumps({"exit_code": 0}))
        (child_dir / "finished").write_text("")
        return child_id

    def fake_land_child(child_id, into_dir=""):
        land_calls.append((child_id, into_dir))
        return True

    monkeypatch.setattr(boss_mod, "run_handoff", fake_run_handoff)
    monkeypatch.setattr(boss_mod, "launch_child", fake_launch_child)
    monkeypatch.setattr(boss_mod, "land_child", fake_land_child)

    result = boss_main(["--plan", str(spec), "--chain-kind", "local"])
    assert result == 0

    assert len(land_calls) == 1
    landed_child_id, landed_into = land_calls[0]
    assert landed_child_id == "child-into-test-bb3344"
    # Must land into the boss worktree, not project_root
    assert landed_into == str(workdir)
    assert landed_into != str(project_root)


def test_land_child_no_into_dir_for_gh_chain(tmp_path, monkeypatch):
    """For gh chains, land_child is called without into_dir (gh landing goes via PR merge)."""
    gr_id = "test-boss-gh-into-cc5566"
    state_dir, project_root, workdir = _make_gremlin_state(tmp_path, gr_id)
    _common_boss_patches(monkeypatch, tmp_path, gr_id)
    monkeypatch.setattr(boss_mod, "get_default_branch", lambda p: "main")
    monkeypatch.setattr(boss_mod, "get_remote_branch_sha", lambda p, b: "deadbeef12345678")

    spec = tmp_path / "spec.md"
    spec.write_text("# Spec\n")
    child_plan = tmp_path / "child-plan.md"
    child_plan.write_text("# Child plan\n")

    land_calls = []

    handoff_results = iter([
        ("next-plan", {"exit_state": "next-plan", "child_plan": str(child_plan), "operator_followups": []}),
        ("chain-done", {"exit_state": "chain-done", "operator_followups": []}),
    ])

    def fake_run_handoff(gr_id, state_dir, boss_state, project_root, boss_workdir, model):
        exit_state, sig = next(handoff_results)
        n = boss_state["handoff_count"] + 1
        out_path = os.path.join(state_dir, f"handoff-{n:03d}.md")
        pathlib.Path(out_path).write_text(f"# Handoff {n}\n")
        boss_state["handoff_count"] = n
        boss_state["current_plan"] = out_path
        boss_state["operator_followups"] = sig.get("operator_followups", [])
        boss_state["handoff_records"].append({
            "timestamp": "2026-01-01T00:00:00Z", "n": n,
            "plan_in": boss_state["spec_path"], "plan_out": out_path,
            "signal_file": "", "exit_state": exit_state,
            "child_plan": sig.get("child_plan"), "bail_reason": None,
            "operator_followups": sig.get("operator_followups", []),
        })
        return exit_state, sig

    def fake_launch_child(gr_id, launch_kind, child_plan_path):
        child_id = "child-gh-into-test-dd7788"
        child_dir = tmp_path / child_id
        child_dir.mkdir(exist_ok=True)
        (child_dir / "state.json").write_text(json.dumps({"exit_code": 0}))
        (child_dir / "finished").write_text("")
        return child_id

    def fake_land_child(child_id, into_dir=""):
        land_calls.append((child_id, into_dir))
        return True

    monkeypatch.setattr(boss_mod, "run_handoff", fake_run_handoff)
    monkeypatch.setattr(boss_mod, "launch_child", fake_launch_child)
    monkeypatch.setattr(boss_mod, "land_child", fake_land_child)

    result = boss_main(["--plan", str(spec), "--chain-kind", "gh"])
    assert result == 0

    assert len(land_calls) == 1
    _, landed_into = land_calls[0]
    assert landed_into == ""


# ---------------------------------------------------------------------------
# Boss base-ref: current_head tracking and child launch
# ---------------------------------------------------------------------------

def test_boss_launches_child_against_current_head(tmp_path, monkeypatch):
    """launch_child passes current_head from state.json as base_ref to launcher.launch."""
    import gremlins.launcher as launcher_mod

    gr_id = "test-boss-basref-aa1122"
    state_dir = tmp_path / gr_id
    state_dir.mkdir()
    expected_sha = "deadbeef12345678deadbeef12345678deadbeef"
    project_root_path = str(tmp_path / "repo")
    (state_dir / "state.json").write_text(json.dumps({
        "id": gr_id,
        "project_root": project_root_path,
        "current_head": expected_sha,
    }))

    captured = {}

    def fake_launch(kind, *, plan=None, parent_id=None, project_root=None,
                    base_ref="HEAD", pipeline_args=(), **kw):
        captured["base_ref"] = base_ref
        captured["project_root"] = project_root
        return "child-abc-123456"

    # boss_state.json required by launch_child to load spec_path
    (state_dir / "boss_state.json").write_text(json.dumps({
        "spec_path": "/some/spec.md",
        "chain_kind": "local",
        "chain_base_ref": expected_sha,
        "target_branch": "main",
        "current_plan": "/some/spec.md",
        "handoff_count": 0,
        "current_child_id": None,
        "children": [],
        "handoff_records": [],
        "operator_followups": [],
    }))

    monkeypatch.setattr(boss_mod, "STATE_ROOT", str(tmp_path))
    monkeypatch.setattr(launcher_mod, "launch", fake_launch)

    result = boss_mod.launch_child(gr_id, "localgremlin", "/tmp/child-plan.md")

    assert result == "child-abc-123456"
    assert captured["base_ref"] == expected_sha
    assert captured["project_root"] == project_root_path


def test_boss_records_current_head_after_land(tmp_path, monkeypatch):
    """After land_child returns True, patch_state is called with the boss worktree's new HEAD."""
    gr_id = "test-boss-head-bb2233"
    state_dir, project_root, workdir = _make_gremlin_state(tmp_path, gr_id)
    _common_boss_patches(monkeypatch, tmp_path, gr_id)

    spec = tmp_path / "spec.md"
    spec.write_text("# Spec\n")
    child_plan = tmp_path / "child-plan.md"
    child_plan.write_text("# Child plan\n")

    initial_head = "inithead1234567890123456789012345678ab"
    expected_new_head = "newhead12345678901234567890abcdef123456"
    head_sequence = iter([initial_head, expected_new_head])
    monkeypatch.setattr(git_mod, "git_head_of_workdir", lambda w: next(head_sequence))

    patch_calls = []
    monkeypatch.setattr(boss_mod, "patch_state", lambda **kw: patch_calls.append(kw))

    handoff_results = iter([
        ("next-plan", {"exit_state": "next-plan", "child_plan": str(child_plan), "operator_followups": []}),
        ("chain-done", {"exit_state": "chain-done", "operator_followups": []}),
    ])

    def fake_run_handoff(gr_id, state_dir, boss_state, project_root, boss_workdir, model):
        exit_state, sig = next(handoff_results)
        n = boss_state["handoff_count"] + 1
        out_path = os.path.join(state_dir, f"handoff-{n:03d}.md")
        pathlib.Path(out_path).write_text(f"# Handoff {n}\n")
        boss_state["handoff_count"] = n
        boss_state["current_plan"] = out_path
        boss_state["operator_followups"] = sig.get("operator_followups", [])
        boss_state["handoff_records"].append({
            "timestamp": "2026-01-01T00:00:00Z", "n": n,
            "plan_in": boss_state["spec_path"], "plan_out": out_path,
            "signal_file": "", "exit_state": exit_state,
            "child_plan": sig.get("child_plan"), "bail_reason": None,
            "operator_followups": sig.get("operator_followups", []),
        })
        return exit_state, sig

    def fake_launch_child(gr_id, launch_kind, child_plan_path):
        child_id = "child-land-test-cc4455"
        child_dir = tmp_path / child_id
        child_dir.mkdir(exist_ok=True)
        (child_dir / "state.json").write_text(json.dumps({"exit_code": 0}))
        (child_dir / "finished").write_text("")
        return child_id

    monkeypatch.setattr(boss_mod, "run_handoff", fake_run_handoff)
    monkeypatch.setattr(boss_mod, "launch_child", fake_launch_child)
    monkeypatch.setattr(boss_mod, "land_child", lambda cid, into_dir="": True)

    result = boss_main(["--plan", str(spec), "--chain-kind", "local"])
    assert result == 0

    current_head_calls = [c for c in patch_calls if "current_head" in c]
    assert len(current_head_calls) == 2, (
        f"expected exactly 2 patch_state(current_head=...) calls "
        f"(chain-start + post-land), got: {current_head_calls}"
    )
    assert current_head_calls[0]["current_head"] == initial_head, (
        f"chain-start current_head should be {initial_head!r}, got {current_head_calls[0]!r}"
    )
    assert current_head_calls[1]["current_head"] == expected_new_head, (
        f"post-land current_head should be {expected_new_head!r}, got {current_head_calls[1]!r}"
    )


# ---------------------------------------------------------------------------
# launch_child spec_path passthrough
# ---------------------------------------------------------------------------

def test_launch_child_forwards_spec_path(tmp_path, monkeypatch):
    """When boss_state has spec_path set, launch_child passes it as spec_path= to launcher.launch."""
    import gremlins.launcher as launcher_mod

    gr_id = "test-boss-spec-dd9900"
    state_dir = tmp_path / gr_id
    state_dir.mkdir()
    spec_path = "/path/to/boss/spec.md"
    (state_dir / "state.json").write_text(json.dumps({
        "id": gr_id,
        "project_root": str(tmp_path / "repo"),
        "current_head": "abc123def456abc1",
    }))
    (state_dir / "boss_state.json").write_text(json.dumps({
        "spec_path": spec_path,
        "chain_kind": "local",
        "chain_base_ref": "abc123def456abc1",
        "target_branch": "main",
        "current_plan": spec_path,
        "handoff_count": 0,
        "current_child_id": None,
        "children": [],
        "handoff_records": [],
        "operator_followups": [],
    }))

    captured = {}

    def fake_launch(kind, *, plan=None, parent_id=None, project_root=None,
                    base_ref="HEAD", pipeline_args=(), **kw):
        captured.update(kw)
        captured["plan"] = plan
        return "child-spec-ee1122"

    monkeypatch.setattr(boss_mod, "STATE_ROOT", str(tmp_path))
    monkeypatch.setattr(launcher_mod, "launch", fake_launch)

    result = boss_mod.launch_child(gr_id, "localgremlin", "/tmp/child-plan.md")

    assert result == "child-spec-ee1122"
    assert captured.get("spec_path") == spec_path


def test_launch_child_no_spec_path_when_absent(tmp_path, monkeypatch):
    """When boss_state has no spec_path, launch_child passes spec_path=None."""
    import gremlins.launcher as launcher_mod

    gr_id = "test-boss-nospec-ff2233"
    state_dir = tmp_path / gr_id
    state_dir.mkdir()
    (state_dir / "state.json").write_text(json.dumps({
        "id": gr_id,
        "project_root": str(tmp_path / "repo"),
        "current_head": "abc123def456abc1",
    }))
    (state_dir / "boss_state.json").write_text(json.dumps({
        "spec_path": "",
        "chain_kind": "local",
        "chain_base_ref": "abc123def456abc1",
        "target_branch": "main",
        "current_plan": "",
        "handoff_count": 0,
        "current_child_id": None,
        "children": [],
        "handoff_records": [],
        "operator_followups": [],
    }))

    captured = {}

    def fake_launch(kind, *, plan=None, parent_id=None, project_root=None,
                    base_ref="HEAD", pipeline_args=(), **kw):
        captured.update(kw)
        return "child-nospec-gg3344"

    monkeypatch.setattr(boss_mod, "STATE_ROOT", str(tmp_path))
    monkeypatch.setattr(launcher_mod, "launch", fake_launch)

    boss_mod.launch_child(gr_id, "localgremlin", "/tmp/child-plan.md")

    assert captured.get("spec_path") is None
