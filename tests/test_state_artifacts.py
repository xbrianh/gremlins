"""Unit tests for artifact-related state helpers."""

import json
import pathlib

import gremlins.executor.state as state_mod

# ---------------------------------------------------------------------------
# State.setup_dirs
# ---------------------------------------------------------------------------


def test_setup_dirs_creates_directories(tmp_path):
    state_dir = tmp_path / "state" / "gr-1"
    session_dir = state_dir / "artifacts"
    state_mod.State.setup_dirs(state_dir, session_dir, gr_id=None)
    assert state_dir.is_dir()
    assert session_dir.is_dir()


def test_setup_dirs_writes_instructions(tmp_path):
    state_dir = tmp_path / "state" / "gr-1"
    session_dir = state_dir / "artifacts"
    state_mod.State.setup_dirs(state_dir, session_dir, gr_id=None, instructions="do x")
    assert (state_dir / "instructions.txt").read_text() == "do x"


def test_setup_dirs_no_gr_id_skips_state_json(tmp_path):
    state_dir = tmp_path / "state" / "gr-1"
    session_dir = state_dir / "artifacts"
    state_mod.State.setup_dirs(state_dir, session_dir, gr_id=None)
    assert not (state_dir / "state.json").exists()


def test_setup_dirs_with_gr_id_bootstraps_state_json(tmp_path):
    state_dir = tmp_path / "state" / "gr-2"
    session_dir = state_dir / "artifacts"
    state_mod.State.setup_dirs(state_dir, session_dir, gr_id="gr-2")
    data = json.loads((state_dir / "state.json").read_text())
    assert data["id"] == "gr-2"


def test_setup_dirs_with_gr_id_does_not_overwrite_existing_state_json(tmp_path):
    state_dir = tmp_path / "state" / "gr-3"
    session_dir = state_dir / "artifacts"
    state_dir.mkdir(parents=True)
    existing = {"id": "gr-3", "stage": "implement", "extra": True}
    (state_dir / "state.json").write_text(json.dumps(existing))
    state_mod.State.setup_dirs(state_dir, session_dir, gr_id="gr-3")
    assert json.loads((state_dir / "state.json").read_text()) == existing


def _make_state_dir(
    tmp_path: pathlib.Path, gr_id: str
) -> tuple[pathlib.Path, pathlib.Path]:
    state_root = tmp_path / "state"
    state_dir = state_root / gr_id
    state_dir.mkdir(parents=True)
    sf = state_dir / "state.json"
    sf.write_text(json.dumps({"id": gr_id}))
    return state_root, sf


def test_append_artifact_appends_in_order(tmp_path, monkeypatch):
    gr_id = "gr-artifact-test"
    state_root, sf = _make_state_dir(tmp_path, gr_id)
    monkeypatch.setattr("gremlins.paths.state_root", lambda: state_root)

    state_mod.append_artifact(gr_id, {"type": "branch", "name": "feat-1"})
    state_mod.append_artifact(
        gr_id,
        {"type": "pr", "url": "https://github.com/o/r/pull/1", "branch": "feat-1"},
    )

    data = json.loads(sf.read_text())
    assert data["artifacts"] == [
        {"type": "branch", "name": "feat-1"},
        {"type": "pr", "url": "https://github.com/o/r/pull/1", "branch": "feat-1"},
    ]


def test_append_artifact_noop_when_no_gr_id(tmp_path, monkeypatch):
    gr_id = "gr-noop-test"
    state_root, sf = _make_state_dir(tmp_path, gr_id)
    monkeypatch.setattr("gremlins.paths.state_root", lambda: state_root)
    state_mod.append_artifact(None, {"type": "branch", "name": "x"})
    data = json.loads(sf.read_text())
    assert "artifacts" not in data


def test_read_pr_url_returns_last_pr_url(tmp_path, monkeypatch):
    gr_id = "gr-pr-url-test"
    state_root, sf = _make_state_dir(tmp_path, gr_id)
    monkeypatch.setattr("gremlins.paths.state_root", lambda: state_root)

    state_mod.append_artifact(gr_id, {"type": "branch", "name": "feat-1"})
    state_mod.append_artifact(
        gr_id,
        {"type": "pr", "url": "https://github.com/o/r/pull/1", "branch": "feat-1"},
    )
    state_mod.append_artifact(gr_id, {"type": "branch", "name": "feat-2"})
    state_mod.append_artifact(
        gr_id,
        {"type": "pr", "url": "https://github.com/o/r/pull/2", "branch": "feat-2"},
    )

    assert state_mod.read_pr_url(gr_id) == "https://github.com/o/r/pull/2"


def test_read_pr_url_empty_when_no_pr(tmp_path, monkeypatch):
    gr_id = "gr-no-pr-test"
    state_root, sf = _make_state_dir(tmp_path, gr_id)
    monkeypatch.setattr("gremlins.paths.state_root", lambda: state_root)

    assert state_mod.read_pr_url(gr_id) == ""


def test_last_artifact_branch_from_branch_entry(tmp_path, monkeypatch):
    gr_id = "gr-lab-test"
    state_root, _ = _make_state_dir(tmp_path, gr_id)
    monkeypatch.setattr("gremlins.paths.state_root", lambda: state_root)

    state_mod.append_artifact(gr_id, {"type": "branch", "name": "feat-1"})
    assert state_mod.last_artifact_branch(gr_id) == "feat-1"


def test_last_artifact_branch_from_pr_entry(tmp_path, monkeypatch):
    gr_id = "gr-lab-pr-test"
    state_root, _ = _make_state_dir(tmp_path, gr_id)
    monkeypatch.setattr("gremlins.paths.state_root", lambda: state_root)

    state_mod.append_artifact(gr_id, {"type": "branch", "name": "feat-1"})
    state_mod.append_artifact(
        gr_id,
        {"type": "pr", "url": "https://github.com/o/r/pull/1", "branch": "feat-1"},
    )
    assert state_mod.last_artifact_branch(gr_id) == "feat-1"


def test_last_artifact_branch_empty_when_no_artifacts(tmp_path, monkeypatch):
    gr_id = "gr-lab-empty-test"
    state_root, _ = _make_state_dir(tmp_path, gr_id)
    monkeypatch.setattr("gremlins.paths.state_root", lambda: state_root)

    assert state_mod.last_artifact_branch(gr_id) == ""


def test_last_artifact_branch_multiple_prs(tmp_path, monkeypatch):
    gr_id = "gr-lab-multi-test"
    state_root, _ = _make_state_dir(tmp_path, gr_id)
    monkeypatch.setattr("gremlins.paths.state_root", lambda: state_root)

    state_mod.append_artifact(gr_id, {"type": "branch", "name": "feat-1"})
    state_mod.append_artifact(
        gr_id,
        {"type": "pr", "url": "https://github.com/o/r/pull/1", "branch": "feat-1"},
    )
    state_mod.append_artifact(gr_id, {"type": "branch", "name": "feat-2"})
    state_mod.append_artifact(
        gr_id,
        {"type": "pr", "url": "https://github.com/o/r/pull/2", "branch": "feat-2"},
    )

    assert state_mod.last_artifact_branch(gr_id) == "feat-2"
