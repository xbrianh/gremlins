"""Unit tests for artifact-related state helpers."""

import json
import pathlib

import gremlins.executor.state as state_mod
from gremlins.executor.state import StateData

# ---------------------------------------------------------------------------
# State.setup_dirs
# ---------------------------------------------------------------------------


def test_setup_dirs_creates_directories(tmp_path):
    state_dir = tmp_path / "state" / "gr-1"
    session_dir = state_dir / "artifacts"
    state_mod.State.setup_dirs(state_dir, session_dir, gremlin_id=None)
    assert state_dir.is_dir()
    assert session_dir.is_dir()


def test_setup_dirs_writes_instructions(tmp_path):
    state_dir = tmp_path / "state" / "gr-1"
    session_dir = state_dir / "artifacts"
    state_mod.State.setup_dirs(
        state_dir, session_dir, gremlin_id=None, instructions="do x"
    )
    assert (state_dir / "instructions.txt").read_text() == "do x"


def test_setup_dirs_no_gremlin_id_skips_state_json(tmp_path):
    state_dir = tmp_path / "state" / "gr-1"
    session_dir = state_dir / "artifacts"
    state_mod.State.setup_dirs(state_dir, session_dir, gremlin_id=None)
    assert not (state_dir / "state.json").exists()


def test_setup_dirs_with_gremlin_id_bootstraps_state_json(tmp_path):
    state_dir = tmp_path / "state" / "gr-2"
    session_dir = state_dir / "artifacts"
    state_mod.State.setup_dirs(state_dir, session_dir, gremlin_id="gr-2")
    data = json.loads((state_dir / "state.json").read_text())
    assert data["id"] == "gr-2"


def test_setup_dirs_with_gremlin_id_does_not_overwrite_existing_state_json(tmp_path):
    state_dir = tmp_path / "state" / "gr-3"
    session_dir = state_dir / "artifacts"
    state_dir.mkdir(parents=True)
    existing = {"id": "gr-3", "stage": "implement", "extra": True}
    (state_dir / "state.json").write_text(json.dumps(existing))
    state_mod.State.setup_dirs(state_dir, session_dir, gremlin_id="gr-3")
    assert json.loads((state_dir / "state.json").read_text()) == existing


def _make_state_dir(
    tmp_path: pathlib.Path, gremlin_id: str, *, attempt: str = ""
) -> tuple[pathlib.Path, pathlib.Path]:
    state_root = tmp_path / "state"
    state_dir = state_root / gremlin_id
    state_dir.mkdir(parents=True)
    sf = state_dir / "state.json"
    initial: dict = {"id": gremlin_id}
    if attempt:
        initial["attempt"] = attempt
    sf.write_text(json.dumps(initial))
    return state_root, sf


def test_append_artifact_appends_in_order(tmp_path, monkeypatch):
    gremlin_id = "gr-artifact-test"
    state_root, sf = _make_state_dir(tmp_path, gremlin_id)
    monkeypatch.setattr("gremlins.paths.state_root", lambda: state_root)

    StateData.load(gremlin_id).append_artifact({"type": "branch", "name": "feat-1"})
    StateData.load(gremlin_id).append_artifact(
        {"type": "pr", "url": "https://github.com/o/r/pull/1", "branch": "feat-1"},
    )

    data = json.loads(sf.read_text())
    assert data["artifacts"] == [
        {"type": "branch", "name": "feat-1"},
        {"type": "pr", "url": "https://github.com/o/r/pull/1", "branch": "feat-1"},
    ]


def test_append_artifact_noop_when_no_gremlin_id(tmp_path, monkeypatch):
    gremlin_id = "gr-noop-test"
    state_root, sf = _make_state_dir(tmp_path, gremlin_id)
    monkeypatch.setattr("gremlins.paths.state_root", lambda: state_root)
    StateData.load(None).append_artifact({"type": "branch", "name": "x"})
    data = json.loads(sf.read_text())
    assert "artifacts" not in data


def test_read_pr_url_returns_last_pr_url(tmp_path, monkeypatch):
    gremlin_id = "gr-pr-url-test"
    state_root, sf = _make_state_dir(tmp_path, gremlin_id)
    monkeypatch.setattr("gremlins.paths.state_root", lambda: state_root)

    StateData.load(gremlin_id).append_artifact({"type": "branch", "name": "feat-1"})
    StateData.load(gremlin_id).append_artifact(
        {"type": "pr", "url": "https://github.com/o/r/pull/1", "branch": "feat-1"},
    )
    StateData.load(gremlin_id).append_artifact({"type": "branch", "name": "feat-2"})
    StateData.load(gremlin_id).append_artifact(
        {"type": "pr", "url": "https://github.com/o/r/pull/2", "branch": "feat-2"},
    )

    assert StateData.load(gremlin_id).read_pr_url() == "https://github.com/o/r/pull/2"


def test_read_pr_url_empty_when_no_pr(tmp_path, monkeypatch):
    gremlin_id = "gr-no-pr-test"
    state_root, sf = _make_state_dir(tmp_path, gremlin_id)
    monkeypatch.setattr("gremlins.paths.state_root", lambda: state_root)

    assert StateData.load(gremlin_id).read_pr_url() == ""


def test_last_artifact_branch_from_branch_entry(tmp_path, monkeypatch):
    gremlin_id = "gr-lab-test"
    state_root, _ = _make_state_dir(tmp_path, gremlin_id)
    monkeypatch.setattr("gremlins.paths.state_root", lambda: state_root)

    StateData.load(gremlin_id).append_artifact({"type": "branch", "name": "feat-1"})
    assert StateData.load(gremlin_id).last_artifact_branch() == "feat-1"


def test_last_artifact_branch_from_pr_entry(tmp_path, monkeypatch):
    gremlin_id = "gr-lab-pr-test"
    state_root, _ = _make_state_dir(tmp_path, gremlin_id)
    monkeypatch.setattr("gremlins.paths.state_root", lambda: state_root)

    StateData.load(gremlin_id).append_artifact({"type": "branch", "name": "feat-1"})
    StateData.load(gremlin_id).append_artifact(
        {"type": "pr", "url": "https://github.com/o/r/pull/1", "branch": "feat-1"},
    )
    assert StateData.load(gremlin_id).last_artifact_branch() == "feat-1"


def test_last_artifact_branch_empty_when_no_artifacts(tmp_path, monkeypatch):
    gremlin_id = "gr-lab-empty-test"
    state_root, _ = _make_state_dir(tmp_path, gremlin_id)
    monkeypatch.setattr("gremlins.paths.state_root", lambda: state_root)

    assert StateData.load(gremlin_id).last_artifact_branch() == ""


def test_last_artifact_branch_multiple_prs(tmp_path, monkeypatch):
    gremlin_id = "gr-lab-multi-test"
    state_root, _ = _make_state_dir(tmp_path, gremlin_id)
    monkeypatch.setattr("gremlins.paths.state_root", lambda: state_root)

    StateData.load(gremlin_id).append_artifact({"type": "branch", "name": "feat-1"})
    StateData.load(gremlin_id).append_artifact(
        {"type": "pr", "url": "https://github.com/o/r/pull/1", "branch": "feat-1"},
    )
    StateData.load(gremlin_id).append_artifact({"type": "branch", "name": "feat-2"})
    StateData.load(gremlin_id).append_artifact(
        {"type": "pr", "url": "https://github.com/o/r/pull/2", "branch": "feat-2"},
    )

    assert StateData.load(gremlin_id).last_artifact_branch() == "feat-2"


# ---------------------------------------------------------------------------
# attempt stamping on append_artifact
# ---------------------------------------------------------------------------


def test_append_artifact_stamps_attempt_when_set(tmp_path, monkeypatch):
    gremlin_id = "gr-stamp-test"
    state_root, sf = _make_state_dir(tmp_path, gremlin_id, attempt="implement-aabb")
    monkeypatch.setattr("gremlins.paths.state_root", lambda: state_root)

    sd = StateData.load(gremlin_id)
    sd.append_artifact({"type": "branch", "name": "feat-stamp"})

    data = json.loads(sf.read_text())
    assert data["artifacts"] == [
        {"type": "branch", "name": "feat-stamp", "attempt": "implement-aabb"}
    ]


def test_append_artifact_no_stamp_when_attempt_empty(tmp_path, monkeypatch):
    gremlin_id = "gr-nostamp-test"
    state_root, sf = _make_state_dir(tmp_path, gremlin_id)
    monkeypatch.setattr("gremlins.paths.state_root", lambda: state_root)

    StateData.load(gremlin_id).append_artifact({"type": "branch", "name": "feat-ns"})

    data = json.loads(sf.read_text())
    assert data["artifacts"] == [{"type": "branch", "name": "feat-ns"}]


# ---------------------------------------------------------------------------
# read_artifacts_for_attempt / read_artifacts_for_stage
# ---------------------------------------------------------------------------


def test_read_artifacts_for_attempt_exact_match(tmp_path, monkeypatch):
    gremlin_id = "gr-rafa-test"
    state_root, sf = _make_state_dir(tmp_path, gremlin_id)
    monkeypatch.setattr("gremlins.paths.state_root", lambda: state_root)

    arts = [
        {"type": "branch", "name": "a", "attempt": "implement-1111"},
        {"type": "pr", "url": "u1", "branch": "a", "attempt": "implement-1111"},
        {"type": "branch", "name": "b", "attempt": "implement-2222"},
    ]
    sf.write_text(json.dumps({"id": gremlin_id, "artifacts": arts}))

    sd = StateData.load(gremlin_id)
    result = sd.read_artifacts_for_attempt("implement-1111")
    assert result == [
        {"type": "branch", "name": "a", "attempt": "implement-1111"},
        {"type": "pr", "url": "u1", "branch": "a", "attempt": "implement-1111"},
    ]


def test_read_artifacts_for_attempt_no_match(tmp_path, monkeypatch):
    gremlin_id = "gr-rafa-none"
    state_root, sf = _make_state_dir(tmp_path, gremlin_id)
    monkeypatch.setattr("gremlins.paths.state_root", lambda: state_root)

    sf.write_text(
        json.dumps(
            {
                "id": gremlin_id,
                "artifacts": [
                    {"type": "branch", "name": "x", "attempt": "review-aaaa"}
                ],
            }
        )
    )

    assert StateData.load(gremlin_id).read_artifacts_for_attempt("implement-aaaa") == []


def test_read_artifacts_for_stage_prefix_match(tmp_path, monkeypatch):
    gremlin_id = "gr-rafs-test"
    state_root, sf = _make_state_dir(tmp_path, gremlin_id)
    monkeypatch.setattr("gremlins.paths.state_root", lambda: state_root)

    arts = [
        {"type": "branch", "name": "a", "attempt": "implement-1111"},
        {"type": "pr", "url": "u1", "branch": "a", "attempt": "implement-2222"},
        {"type": "branch", "name": "b", "attempt": "review-3333"},
    ]
    sf.write_text(json.dumps({"id": gremlin_id, "artifacts": arts}))

    sd = StateData.load(gremlin_id)
    result = sd.read_artifacts_for_stage("implement")
    assert result == [
        {"type": "branch", "name": "a", "attempt": "implement-1111"},
        {"type": "pr", "url": "u1", "branch": "a", "attempt": "implement-2222"},
    ]


def test_read_artifacts_for_stage_excludes_unstamped(tmp_path, monkeypatch):
    gremlin_id = "gr-rafs-unstamped"
    state_root, sf = _make_state_dir(tmp_path, gremlin_id)
    monkeypatch.setattr("gremlins.paths.state_root", lambda: state_root)

    arts = [
        {"type": "branch", "name": "old"},
        {"type": "branch", "name": "new", "attempt": "implement-9999"},
    ]
    sf.write_text(json.dumps({"id": gremlin_id, "artifacts": arts}))

    result = StateData.load(gremlin_id).read_artifacts_for_stage("implement")
    assert result == [{"type": "branch", "name": "new", "attempt": "implement-9999"}]
