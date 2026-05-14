"""Tests for resolve_gremlin exact-match behaviour."""

import json
import pathlib

import gremlins.fleet.constants as _constants
from gremlins.fleet.resolve import collect_gremlin_matches, resolve_gremlin


def _make_gremlin(state_root: pathlib.Path, gremlin_id: str) -> None:
    d = state_root / gremlin_id
    d.mkdir(parents=True)
    (d / "state.json").write_text(json.dumps({"id": gremlin_id}))


def test_exact_match_wins_over_prefix_ambiguity(tmp_path, monkeypatch, capsys):
    state_root = tmp_path / "state"
    state_root.mkdir()
    _make_gremlin(state_root, "update-assistant-prompt")
    _make_gremlin(state_root, "update-assistant-prompt-with-queue-docs-14f2fd")
    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))

    result = resolve_gremlin("update-assistant-prompt")

    assert result is not None
    assert result[0] == "update-assistant-prompt"
    assert capsys.readouterr().out == ""


def test_genuine_ambiguity_prints_error(tmp_path, monkeypatch, capsys):
    state_root = tmp_path / "state"
    state_root.mkdir()
    _make_gremlin(state_root, "fix-bug-abc")
    _make_gremlin(state_root, "fix-bug-def")
    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))

    result = resolve_gremlin("fix-bug")

    assert result is None
    out = capsys.readouterr().out
    assert "ambiguous" in out
    assert "fix-bug-abc" in out
    assert "fix-bug-def" in out


def test_no_match_prints_error(tmp_path, monkeypatch, capsys):
    state_root = tmp_path / "state"
    state_root.mkdir()
    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))

    result = resolve_gremlin("nonexistent")

    assert result is None
    assert "no gremlin matched" in capsys.readouterr().out


def test_collect_gremlin_matches_exact(tmp_path, monkeypatch):
    state_root = tmp_path / "state"
    state_root.mkdir()
    _make_gremlin(state_root, "foo")
    _make_gremlin(state_root, "foobar")
    monkeypatch.setattr(_constants, "STATE_ROOT", str(state_root))

    matches, exact = collect_gremlin_matches("foo")

    assert len(matches) == 2
    assert exact is not None
    assert exact[0] == "foo"
