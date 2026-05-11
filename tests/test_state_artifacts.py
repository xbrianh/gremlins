"""Unit tests for artifact-related state helpers."""

import json
import pathlib

import gremlins.executor.state as state_mod


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
