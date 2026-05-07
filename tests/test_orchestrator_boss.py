"""Tests for gremlins.orchestrators.boss (thin wrapper over local_main)."""

from __future__ import annotations

from gremlins.orchestrators.boss import _strip_arg, boss_main

# ---------------------------------------------------------------------------
# _strip_arg
# ---------------------------------------------------------------------------


def test_strip_arg_removes_flag_and_value():
    result = _strip_arg(
        ["--plan", "foo.md", "--chain-kind", "local", "--client", "c:m"], "--chain-kind"
    )
    assert result == ["--plan", "foo.md", "--client", "c:m"]


def test_strip_arg_removes_equals_form():
    result = _strip_arg(["--chain-kind=local", "--plan", "foo.md"], "--chain-kind")
    assert result == ["--plan", "foo.md"]


def test_strip_arg_noop_when_absent():
    args = ["--plan", "foo.md"]
    assert _strip_arg(args, "--chain-kind") == args


def test_strip_arg_dangling_flag():
    # Flag at end with no following value
    result = _strip_arg(["--plan", "foo.md", "--chain-kind"], "--chain-kind")
    assert result == ["--plan", "foo.md"]


# ---------------------------------------------------------------------------
# boss_main delegates to local_main
# ---------------------------------------------------------------------------


def test_boss_main_delegates_to_local_main(monkeypatch, tmp_path):
    captured: list[tuple[list[str], str | None]] = []

    def fake_local_main(argv: list[str], *, gr_id: str | None = None) -> int:
        captured.append((list(argv), gr_id))
        return 0

    monkeypatch.setattr("gremlins.orchestrators.local.local_main", fake_local_main)

    plan = tmp_path / "plan.md"
    plan.write_text("# Plan\n", encoding="utf-8")

    rc = boss_main(["--plan", str(plan), "--chain-kind", "local"], gr_id="test-gr")

    assert rc == 0
    assert len(captured) == 1
    argv, gr_id_arg = captured[0]
    assert argv[0] == "--pipeline"
    assert argv[1] == "boss"
    # --chain-kind should be stripped
    assert "--chain-kind" not in argv
    assert str(plan) in argv
    assert gr_id_arg == "test-gr"


def test_boss_main_without_chain_kind(monkeypatch, tmp_path):
    captured: list[list[str]] = []

    def fake_local_main(argv: list[str], *, gr_id: str | None = None) -> int:
        captured.append(list(argv))
        return 0

    monkeypatch.setattr("gremlins.orchestrators.local.local_main", fake_local_main)

    plan = tmp_path / "plan.md"
    plan.write_text("# Plan\n", encoding="utf-8")

    rc = boss_main(["--plan", str(plan)], gr_id=None)

    assert rc == 0
    assert captured[0][:2] == ["--pipeline", "boss"]


def test_boss_main_passes_gr_id(monkeypatch):
    received_gr_id: list[str | None] = []

    def fake_local_main(argv: list[str], *, gr_id: str | None = None) -> int:
        received_gr_id.append(gr_id)
        return 42

    monkeypatch.setattr("gremlins.orchestrators.local.local_main", fake_local_main)

    rc = boss_main([], gr_id="my-gr-id")
    assert rc == 42
    assert received_gr_id == ["my-gr-id"]
