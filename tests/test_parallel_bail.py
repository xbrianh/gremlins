"""Unit tests for gremlins.utils.parallel_bail: collect_bails and decide."""

from __future__ import annotations

import json
import pathlib

from gremlins.utils.parallel_bail import BailedChild, BailDecision, collect_bails, decide


# ---------------------------------------------------------------------------
# collect_bails
# ---------------------------------------------------------------------------


def _write_bail(state_dir: pathlib.Path, attempt: str, payload: dict[str, str]) -> None:
    (state_dir / f"bail_{attempt}.json").write_text(
        json.dumps(payload), encoding="utf-8"
    )


def test_collect_bails_none_bailed(tmp_path: pathlib.Path) -> None:
    attempts = {"a": "attempt-a", "b": "attempt-b"}
    result = collect_bails(tmp_path, ["a", "b"], attempts)
    assert result == []


def test_collect_bails_some_bailed(tmp_path: pathlib.Path) -> None:
    attempts = {"a": "attempt-a", "b": "attempt-b"}
    _write_bail(tmp_path, "attempt-a", {"class": "security", "detail": "bad"})

    result = collect_bails(tmp_path, ["a", "b"], attempts)
    assert len(result) == 1
    assert result[0].key == "a"
    assert result[0].bail == {"class": "security", "detail": "bad"}


def test_collect_bails_all_bailed(tmp_path: pathlib.Path) -> None:
    attempts = {"a": "attempt-a", "b": "attempt-b"}
    _write_bail(tmp_path, "attempt-a", {"class": "other", "detail": "x"})
    _write_bail(tmp_path, "attempt-b", {"class": "other", "detail": "y"})

    result = collect_bails(tmp_path, ["a", "b"], attempts)
    assert [r.key for r in result] == ["a", "b"]


def test_collect_bails_preserves_child_key_order(tmp_path: pathlib.Path) -> None:
    attempts = {"c": "attempt-c", "a": "attempt-a", "b": "attempt-b"}
    _write_bail(tmp_path, "attempt-c", {"class": "other", "detail": ""})
    _write_bail(tmp_path, "attempt-a", {"class": "other", "detail": ""})

    result = collect_bails(tmp_path, ["c", "a", "b"], attempts)
    assert [r.key for r in result] == ["c", "a"]


def test_collect_bails_missing_attempt_key_skipped(tmp_path: pathlib.Path) -> None:
    attempts: dict[str, str] = {}  # no attempt recorded for "a"
    _write_bail(tmp_path, "attempt-a", {"class": "other", "detail": ""})

    result = collect_bails(tmp_path, ["a"], attempts)
    assert result == []


def test_collect_bails_corrupt_json_falls_back(tmp_path: pathlib.Path) -> None:
    attempts = {"a": "attempt-a"}
    (tmp_path / "bail_attempt-a.json").write_text("not-json", encoding="utf-8")

    result = collect_bails(tmp_path, ["a"], attempts)
    assert len(result) == 1
    assert result[0].bail == {"class": "other"}


def test_collect_bails_empty_child_keys(tmp_path: pathlib.Path) -> None:
    attempts = {"a": "attempt-a"}
    _write_bail(tmp_path, "attempt-a", {"class": "other", "detail": ""})

    result = collect_bails(tmp_path, [], attempts)
    assert result == []


# ---------------------------------------------------------------------------
# decide — "any" policy
# ---------------------------------------------------------------------------


def _bc(key: str) -> BailedChild:
    return BailedChild(key=key, bail={"class": "other", "detail": key})


def test_decide_any_none_bailed() -> None:
    d = decide([], total=3, policy="any")
    assert d == BailDecision(should_bail=False, first_bail={})


def test_decide_any_some_bailed() -> None:
    d = decide([_bc("a")], total=3, policy="any")
    assert d.should_bail is True
    assert d.first_bail["detail"] == "a"


def test_decide_any_all_bailed() -> None:
    d = decide([_bc("a"), _bc("b"), _bc("c")], total=3, policy="any")
    assert d.should_bail is True


# ---------------------------------------------------------------------------
# decide — "all" policy
# ---------------------------------------------------------------------------


def test_decide_all_none_bailed() -> None:
    d = decide([], total=3, policy="all")
    assert d == BailDecision(should_bail=False, first_bail={})


def test_decide_all_some_bailed() -> None:
    d = decide([_bc("a")], total=3, policy="all")
    assert d.should_bail is False
    assert d.first_bail == {"class": "other", "detail": "a"}


def test_decide_all_all_bailed() -> None:
    d = decide([_bc("a"), _bc("b"), _bc("c")], total=3, policy="all")
    assert d.should_bail is True
    assert d.first_bail["detail"] == "a"


def test_decide_all_single_child_bailed() -> None:
    d = decide([_bc("solo")], total=1, policy="all")
    assert d.should_bail is True


# ---------------------------------------------------------------------------
# decide — first_bail is always the first bailed child's data
# ---------------------------------------------------------------------------


def test_decide_first_bail_is_first_child() -> None:
    bailed = [
        BailedChild(key="a", bail={"class": "x", "detail": "first"}),
        BailedChild(key="b", bail={"class": "y", "detail": "second"}),
    ]
    d = decide(bailed, total=2, policy="any")
    assert d.first_bail["detail"] == "first"
