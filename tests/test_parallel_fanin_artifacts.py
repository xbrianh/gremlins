"""Tests for artifact gathering at parallel fan-in."""

from __future__ import annotations

import json
import logging
import pathlib

import pytest

from gremlins.artifacts.uri import Uri
from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import State, StateData, build_state
from gremlins.stages.parallel import _ParallelExecutor


def _make_parent(tmp_path: pathlib.Path, gremlin_id: str) -> State:
    artifact_dir = tmp_path / "state" / gremlin_id / "artifacts"
    artifact_dir.mkdir(parents=True, exist_ok=True)
    data = StateData(gremlin_id=gremlin_id)
    return build_state(data=data, client=FakeClaudeClient(), artifact_dir=artifact_dir)


def _make_child_dir(
    tmp_path: pathlib.Path,
    child_id: str,
    bindings: dict[str, str],
    files: dict[str, bytes],
) -> None:
    child_dir = tmp_path / "state" / child_id
    artifacts_dir = child_dir / "artifacts"
    artifacts_dir.mkdir(parents=True, exist_ok=True)
    (child_dir / "registry.json").write_text(json.dumps(bindings), encoding="utf-8")
    for name, content in files.items():
        (artifacts_dir / name).write_bytes(content)


def _executor(
    parent_state: State,
    child_keys: list[str],
    group_name: str = "grp",
) -> _ParallelExecutor:
    child_runners = [(k, parent_state, lambda: None) for k in child_keys]
    return _ParallelExecutor(
        group_name,
        child_runners,  # type: ignore[arg-type]
        max_concurrent=None,
        set_stage_fn=lambda _: None,
        cancel_on_bail=False,
        bail_policy="any",
        parent_data=parent_state.data,
        parent_state=parent_state,
        project_root=pathlib.Path("/nonexistent"),
    )


@pytest.fixture(autouse=True)
def _sandbox(tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("GREMLINS_SANDBOX_ROOT", str(tmp_path))


# ---------------------------------------------------------------------------
# single child, file://session/ artifact
# ---------------------------------------------------------------------------


def test_single_child_file_artifact_copied(tmp_path: pathlib.Path) -> None:
    parent = _make_parent(tmp_path, "p1")
    child_id = "p1--grp--sonnet"
    _make_child_dir(
        tmp_path,
        child_id,
        bindings={"review-code": "file://session/review.md"},
        files={"review.md": b"# review"},
    )

    ex = _executor(parent, ["sonnet"])
    ex._gather_child_artifacts()

    assert parent.artifacts.produced("review-code")
    content = parent.artifacts.read("review-code")
    assert content == "# review"
    dest = parent.artifact_dir / "review.md"
    assert dest.exists()


# ---------------------------------------------------------------------------
# multiple children produce the same key → disambiguate with child_key
# ---------------------------------------------------------------------------


def test_multi_child_same_key_disambiguated(tmp_path: pathlib.Path) -> None:
    parent = _make_parent(tmp_path, "p2")

    _make_child_dir(
        tmp_path,
        "p2--grp--opus",
        bindings={"review-code": "file://session/review.md"},
        files={"review.md": b"opus review"},
    )
    _make_child_dir(
        tmp_path,
        "p2--grp--sonnet",
        bindings={"review-code": "file://session/review.md"},
        files={"review.md": b"sonnet review"},
    )

    ex = _executor(parent, ["opus", "sonnet"])
    ex._gather_child_artifacts()

    assert parent.artifacts.produced("review-code/opus")
    assert parent.artifacts.produced("review-code/sonnet")
    assert parent.artifacts.read("review-code/opus") == "opus review"
    assert parent.artifacts.read("review-code/sonnet") == "sonnet review"
    assert not parent.artifacts.produced("review-code")


# ---------------------------------------------------------------------------
# parent-snapshotted keys are not re-bound
# ---------------------------------------------------------------------------


def test_snapshotted_parent_keys_skipped(tmp_path: pathlib.Path) -> None:
    parent = _make_parent(tmp_path, "p3")
    parent_file = parent.artifact_dir / "existing.txt"
    parent_file.write_bytes(b"existing")
    parent.artifacts.bind("existing-key", Uri.parse("file://session/existing.txt"))

    _make_child_dir(
        tmp_path,
        "p3--grp--child",
        bindings={
            "existing-key": "file:///some/absolute/path.txt",  # snapshotted, rewritten
            "new-key": "file://session/new.txt",
        },
        files={"new.txt": b"new content"},
    )

    ex = _executor(parent, ["child"])
    ex._gather_child_artifacts()

    # existing-key must not be rebound
    assert (
        str(parent.artifacts.resolve("existing-key")) == "file://session/existing.txt"
    )
    # new-key must be gathered
    assert parent.artifacts.produced("new-key")
    assert parent.artifacts.read("new-key") == "new content"


# ---------------------------------------------------------------------------
# non-file:// artifact (e.g. gh://) is bound directly
# ---------------------------------------------------------------------------


def test_non_file_artifact_bound_directly(tmp_path: pathlib.Path) -> None:
    parent = _make_parent(tmp_path, "p4")
    _make_child_dir(
        tmp_path,
        "p4--grp--child",
        bindings={"pr": "gh://pr/42"},
        files={},
    )

    ex = _executor(parent, ["child"])
    ex._gather_child_artifacts()

    assert parent.artifacts.produced("pr")
    assert str(parent.artifacts.resolve("pr")) == "gh://pr/42"


# ---------------------------------------------------------------------------
# missing child artifact file → warning, no crash
# ---------------------------------------------------------------------------


def test_missing_child_artifact_file_skipped(
    tmp_path: pathlib.Path, caplog: pytest.LogCaptureFixture
) -> None:
    parent = _make_parent(tmp_path, "p5")
    _make_child_dir(
        tmp_path,
        "p5--grp--child",
        bindings={"review-code": "file://session/missing.md"},
        files={},  # file not created
    )

    ex = _executor(parent, ["child"])
    with caplog.at_level(logging.WARNING):
        ex._gather_child_artifacts()

    assert not parent.artifacts.produced("review-code")
    assert any("missing" in r.message for r in caplog.records)
