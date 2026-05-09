"""Tests for gremlins/handoff.py."""

import json
import pathlib

import pytest
from conftest import MINIMAL_EVENTS

from gremlins import handoff
from gremlins.clients import ClientSpec
from gremlins.clients.fake import FakeClaudeClient

FIXTURES = pathlib.Path(__file__).parent / "fixtures"


class WritingHandoffClient(FakeClaudeClient):
    def __init__(
        self,
        *,
        out_path: pathlib.Path,
        signal_path: pathlib.Path,
        signal_payload: dict[str, object],
        child_path: pathlib.Path | None = None,
        child_plan_text: str | None = None,
        sanitize_text: str | None = None,
        handoff_error: Exception | None = None,
        sanitize_error: Exception | None = None,
    ) -> None:
        super().__init__(
            fixtures={
                "handoff": MINIMAL_EVENTS,
                "handoff:sanitize": MINIMAL_EVENTS,
            }
        )
        self.out_path = out_path
        self.signal_path = signal_path
        self.signal_payload = signal_payload
        self.child_path = child_path
        self.child_plan_text = child_plan_text
        self.sanitize_text = sanitize_text
        self.handoff_error = handoff_error
        self.sanitize_error = sanitize_error

    def run(self, prompt, *, label, **kwargs):
        if label == "handoff":
            if self.handoff_error is not None:
                raise self.handoff_error
            self.signal_path.write_text(json.dumps(self.signal_payload))
            self.out_path.write_text("# Rolling plan (stub)\n")
            if self.child_path is not None and self.child_plan_text is not None:
                self.child_path.write_text(self.child_plan_text)
        elif label == "handoff:sanitize":
            if self.sanitize_error is not None:
                raise self.sanitize_error
            if self.sanitize_text is not None:
                self.out_path.write_text(self.sanitize_text)
        return super().run(prompt, label=label, **kwargs)


# ---------------------------------------------------------------------------
# auto_name_out — naming convention
# ---------------------------------------------------------------------------


def test_auto_name_out_default_naming(tmp_path):
    out = handoff.auto_name_out(tmp_path / "plan.md")
    assert out == tmp_path / "plan-001.md"


def test_auto_name_out_increments_when_taken(tmp_path):
    (tmp_path / "plan-001.md").touch()
    (tmp_path / "plan-002.md").touch()
    out = handoff.auto_name_out(tmp_path / "plan.md")
    assert out == tmp_path / "plan-003.md"


def test_auto_name_out_strips_existing_numeric_suffix(tmp_path):
    """plan-001.md → plan-002.md (not plan-001-001.md)."""
    out = handoff.auto_name_out(tmp_path / "plan-001.md")
    assert out == tmp_path / "plan-001.md"  # next free is 001 since none exist
    (tmp_path / "plan-001.md").touch()
    out = handoff.auto_name_out(tmp_path / "plan-001.md")
    assert out == tmp_path / "plan-002.md"


# ---------------------------------------------------------------------------
# parse_args
# ---------------------------------------------------------------------------


def test_parse_args_requires_plan():
    with pytest.raises(SystemExit):
        handoff.parse_args([])


def test_parse_args_defaults():
    args = handoff.parse_args(["--plan", "/tmp/plan.md"])
    assert args.plan == "/tmp/plan.md"
    assert args.spec is None
    assert args.out is None
    assert args.base is None
    assert args.client == "claude:sonnet"
    assert args.rev is None


def test_parse_args_full():
    args = handoff.parse_args(
        [
            "--plan",
            "/p.md",
            "--spec",
            "/s.md",
            "--out",
            "/o.md",
            "--base",
            "develop",
            "--client",
            "claude:haiku",
            "--rev",
            "feature-branch",
        ]
    )
    assert args.plan == "/p.md"
    assert args.spec == "/s.md"
    assert args.out == "/o.md"
    assert args.base == "develop"
    assert args.client == "claude:haiku"
    assert args.rev == "feature-branch"


# ---------------------------------------------------------------------------
# build_prompt — string templating
# ---------------------------------------------------------------------------


def _prompt_paths(tmp_path):
    return (
        tmp_path / "plan-001.md",
        tmp_path / "plan-001-child.md",
        tmp_path / "plan-001.state.json",
    )


def test_build_prompt_includes_plan_log_diff_paths(tmp_path):
    out_p, child_p, sig_p = _prompt_paths(tmp_path)
    p = handoff.build_prompt(
        plan_text="# Plan\nDo X.\n",
        branch="main",
        git_log="abc msg",
        git_diff="diff body",
        out_path=out_p,
        child_plan_path=child_p,
        signal_path=sig_p,
    )
    assert "Do X." in p
    assert "abc msg" in p
    assert "diff body" in p
    assert str(out_p) in p
    assert str(child_p) in p
    assert str(sig_p) in p
    assert "Branch: main" in p


def test_build_prompt_truncates_huge_diff(tmp_path):
    out_p, child_p, sig_p = _prompt_paths(tmp_path)
    p = handoff.build_prompt(
        plan_text="plan",
        branch="b",
        git_log="",
        git_diff="x" * 100000,
        out_path=out_p,
        child_plan_path=child_p,
        signal_path=sig_p,
    )
    assert "diff truncated to 50000 chars" in p


def test_build_prompt_handles_empty_diff_and_log(tmp_path):
    out_p, child_p, sig_p = _prompt_paths(tmp_path)
    p = handoff.build_prompt(
        plan_text="plan",
        branch="b",
        git_log="",
        git_diff="",
        out_path=out_p,
        child_plan_path=child_p,
        signal_path=sig_p,
    )
    assert "(empty — no changes yet)" in p
    assert "(no commits yet — branch just started)" in p


def test_build_prompt_includes_spec_when_given(tmp_path):
    out_p, child_p, sig_p = _prompt_paths(tmp_path)
    p = handoff.build_prompt(
        plan_text="plan",
        branch="b",
        git_log="",
        git_diff="",
        out_path=out_p,
        child_plan_path=child_p,
        signal_path=sig_p,
        spec_text="Overall north-star context",
    )
    assert "Overall north-star context" in p
    assert "Overarching goal" in p


def test_build_prompt_omits_spec_section_by_default(tmp_path):
    out_p, child_p, sig_p = _prompt_paths(tmp_path)
    p = handoff.build_prompt(
        plan_text="plan",
        branch="b",
        git_log="",
        git_diff="",
        out_path=out_p,
        child_plan_path=child_p,
        signal_path=sig_p,
    )
    assert "Overarching goal" not in p


# ---------------------------------------------------------------------------
# main — refusal cases
# ---------------------------------------------------------------------------


def test_main_missing_plan_arg_exits():
    with pytest.raises(SystemExit):
        handoff.main([])


def test_main_claude_not_in_path(monkeypatch, tmp_path, capsys):
    monkeypatch.setattr(handoff.shutil, "which", lambda n: None)
    with pytest.raises(SystemExit) as exc:
        handoff.main(["--plan", str(tmp_path / "p.md")])
    assert exc.value.code == 1
    err = capsys.readouterr().err
    assert "claude CLI not found" in err


def test_main_plan_does_not_exist(monkeypatch, tmp_path, capsys):
    monkeypatch.setattr(handoff.shutil, "which", lambda n: "/fake/claude")
    with pytest.raises(SystemExit):
        handoff.main(["--plan", str(tmp_path / "missing.md")])
    err = capsys.readouterr().err
    assert "does not exist" in err


def test_main_plan_is_directory(monkeypatch, tmp_path, capsys):
    monkeypatch.setattr(handoff.shutil, "which", lambda n: "/fake/claude")
    d = tmp_path / "plan-dir"
    d.mkdir()
    with pytest.raises(SystemExit):
        handoff.main(["--plan", str(d)])
    err = capsys.readouterr().err
    assert "not a file" in err


def test_main_plan_is_empty(monkeypatch, tmp_path, capsys):
    monkeypatch.setattr(handoff.shutil, "which", lambda n: "/fake/claude")
    p = tmp_path / "empty.md"
    p.touch()
    with pytest.raises(SystemExit):
        handoff.main(["--plan", str(p)])
    err = capsys.readouterr().err
    assert "is empty" in err


def test_main_out_parent_does_not_exist(monkeypatch, tmp_path, capsys):
    monkeypatch.setattr(handoff.shutil, "which", lambda n: "/fake/claude")
    p = tmp_path / "plan.md"
    p.write_text("# Plan\n")
    with pytest.raises(SystemExit):
        handoff.main(
            [
                "--plan",
                str(p),
                "--out",
                str(tmp_path / "missing-dir" / "out.md"),
            ]
        )
    err = capsys.readouterr().err
    assert "parent directory does not exist" in err


# ---------------------------------------------------------------------------
# run — signal parsing (next-plan / chain-done / bail)
# ---------------------------------------------------------------------------


def _stub_happy_run(
    monkeypatch,
    tmp_path,
    signal_payload,
    *,
    child_plan_text=None,
    handoff_error=None,
    sanitize_error=None,
    sanitize_text=None,
):
    """Set up a happy-path handoff.run() call with a fake client."""
    plan_path = tmp_path / "plan.md"
    plan_path.write_text("# Plan\nTasks\n- [ ] thing\n")

    monkeypatch.setattr(
        handoff,
        "collect_git_context",
        lambda base_ref, rev=None: ("test-branch", "log line", "diff body"),
    )
    monkeypatch.setattr(handoff, "_load_handoff_style", lambda: "Keep it simple.")

    out_path = handoff.auto_name_out(plan_path)
    sig_path = out_path.parent / (out_path.stem + ".state.json")
    child_path = out_path.parent / (out_path.stem + "-child" + out_path.suffix)
    client = WritingHandoffClient(
        out_path=out_path,
        signal_path=sig_path,
        signal_payload=signal_payload,
        child_path=child_path,
        child_plan_text=child_plan_text,
        sanitize_text=sanitize_text,
        handoff_error=handoff_error,
        sanitize_error=sanitize_error,
    )
    args = handoff.parse_args(["--plan", str(plan_path)])
    return args, client, sig_path, child_path, out_path


def test_run_chain_done_signal(monkeypatch, tmp_path):
    args, client, sig_path, _, _ = _stub_happy_run(
        monkeypatch,
        tmp_path,
        {
            "exit_state": "chain-done",
            "child_plan": None,
            "reason": None,
            "operator_followups": [],
        },
    )
    rc = handoff.run(client, args)
    assert rc == 0
    assert sig_path.exists()
    assert json.loads(sig_path.read_text())["exit_state"] == "chain-done"
    assert [call.label for call in client.calls] == ["handoff", "handoff:sanitize"]


def test_run_next_plan_signal(monkeypatch, tmp_path):
    args, client, sig_path, child_path, _ = _stub_happy_run(
        monkeypatch,
        tmp_path,
        {
            "exit_state": "next-plan",
            "child_plan": None,
            "reason": None,
            "operator_followups": [],
        },
        child_plan_text="# Next step\n",
    )
    client.signal_payload = {
        "exit_state": "next-plan",
        "child_plan": str(child_path),
        "reason": None,
        "operator_followups": [],
    }
    rc = handoff.run(client, args)
    assert rc == 0
    assert child_path.exists()
    assert json.loads(sig_path.read_text())["child_plan"] == str(child_path)
    assert [call.label for call in client.calls] == ["handoff", "handoff:sanitize"]


def test_run_bail_signal(monkeypatch, tmp_path):
    args, client, _, _, _ = _stub_happy_run(
        monkeypatch,
        tmp_path,
        {
            "exit_state": "bail",
            "child_plan": None,
            "reason": "incoherent state — see plan",
            "operator_followups": [],
        },
    )
    rc = handoff.run(client, args)
    assert rc == 0
    assert (
        json.loads(client.signal_path.read_text())["reason"]
        == "incoherent state — see plan"
    )


def test_run_signal_file_not_written(monkeypatch, tmp_path, capsys):
    plan_path = tmp_path / "plan.md"
    plan_path.write_text("# Plan\n")
    monkeypatch.setattr(
        handoff, "collect_git_context", lambda base_ref, rev=None: ("b", "", "")
    )
    monkeypatch.setattr(handoff, "_load_handoff_style", lambda: "Keep it simple.")
    args = handoff.parse_args(["--plan", str(plan_path)])
    client = FakeClaudeClient(fixtures={"handoff": MINIMAL_EVENTS})
    rc = handoff.run(client, args)
    assert rc == 1
    err = capsys.readouterr().err
    assert "signal file not written" in err


def test_run_signal_file_invalid_json(monkeypatch, tmp_path, capsys):
    args, client, sig_path, _, _ = _stub_happy_run(monkeypatch, tmp_path, {})
    client.signal_payload = {}

    def write_bad_json(prompt, *, label, **kwargs):
        if label == "handoff":
            sig_path.write_text("not json")
            client.out_path.write_text("# Rolling plan\n")
        return FakeClaudeClient.run(client, prompt, label=label, **kwargs)

    monkeypatch.setattr(
        client,
        "run",
        write_bad_json,
    )
    rc = handoff.run(client, args)
    assert rc == 1
    err = capsys.readouterr().err
    assert "could not parse signal file" in err


def test_run_signal_unknown_exit_state(monkeypatch, tmp_path, capsys):
    args, client, _, _, _ = _stub_happy_run(
        monkeypatch,
        tmp_path,
        {
            "exit_state": "bogus",
        },
    )
    rc = handoff.run(client, args)
    assert rc == 1
    err = capsys.readouterr().err
    assert "unrecognized exit_state" in err


def test_run_next_plan_missing_child_plan(monkeypatch, tmp_path, capsys):
    args, client, _, _, _ = _stub_happy_run(
        monkeypatch,
        tmp_path,
        {
            "exit_state": "next-plan",
            "child_plan": None,
            "reason": None,
            "operator_followups": [],
        },
    )
    rc = handoff.run(client, args)
    assert rc == 1
    err = capsys.readouterr().err
    assert "child_plan is null" in err


def test_run_next_plan_child_plan_path_does_not_exist(monkeypatch, tmp_path, capsys):
    args, client, _, child_path, _ = _stub_happy_run(
        monkeypatch,
        tmp_path,
        {
            "exit_state": "next-plan",
            "child_plan": str(tmp_path / "no-such-child.md"),
            "reason": None,
            "operator_followups": [],
        },
    )
    rc = handoff.run(client, args)
    assert rc == 1
    err = capsys.readouterr().err
    assert "child plan path in signal file does not exist" in err


def test_run_client_error(monkeypatch, tmp_path, capsys):
    args, client, _, _, _ = _stub_happy_run(
        monkeypatch,
        tmp_path,
        {},
        handoff_error=RuntimeError("boom"),
    )
    rc = handoff.run(client, args)
    assert rc == 1
    err = capsys.readouterr().err
    assert "handoff agent failed: boom" in err


def test_run_claude_sanitizes_with_haiku(monkeypatch, tmp_path):
    args, client, _, _, _ = _stub_happy_run(
        monkeypatch,
        tmp_path,
        {
            "exit_state": "chain-done",
            "child_plan": None,
            "reason": None,
            "operator_followups": [],
        },
        sanitize_text="# Sanitized rolling plan\n",
    )
    args.client = "claude:opus"

    rc = handoff.run(client, args)

    assert rc == 0
    assert [(c.label, c.model) for c in client.calls] == [
        ("handoff", "opus"),
        ("handoff:sanitize", handoff.CLAUDE_SANITIZE_MODEL),
    ]


def test_run_non_claude_sanitizes_with_main_model(monkeypatch, tmp_path):
    args, client, _, _, _ = _stub_happy_run(
        monkeypatch,
        tmp_path,
        {
            "exit_state": "chain-done",
            "child_plan": None,
            "reason": None,
            "operator_followups": [],
        },
        sanitize_text="# Sanitized rolling plan\n",
    )
    args.client = "copilot:gpt-5.4"

    rc = handoff.run(client, args)

    assert rc == 0
    assert [(c.label, c.model) for c in client.calls] == [
        ("handoff", "gpt-5.4"),
        ("handoff:sanitize", "gpt-5.4"),
    ]


def test_run_operator_followups_preserved_in_signal(monkeypatch, tmp_path):
    args, client, sig_path, _, _ = _stub_happy_run(
        monkeypatch,
        tmp_path,
        {
            "exit_state": "chain-done",
            "child_plan": None,
            "reason": None,
            "operator_followups": ["Sync ~/.claude/", "Run smoke test manually"],
        },
    )
    rc = handoff.run(client, args)
    assert rc == 0
    assert json.loads(sig_path.read_text())["operator_followups"] == [
        "Sync ~/.claude/",
        "Run smoke test manually",
    ]


# ---------------------------------------------------------------------------
# main — spec is best-effort
# ---------------------------------------------------------------------------


def test_main_missing_spec_warns_and_continues(monkeypatch, tmp_path, capsys):
    args, client, _, _, _ = _stub_happy_run(
        monkeypatch,
        tmp_path,
        {
            "exit_state": "chain-done",
            "child_plan": None,
            "reason": None,
            "operator_followups": [],
        },
    )
    args.spec = str(tmp_path / "no-such-spec.md")
    rc = handoff.run(client, args)
    assert rc == 0
    err = capsys.readouterr().err
    assert "warning" in err.lower()
    assert "spec" in err.lower()


def test_main_builds_client_and_runs(monkeypatch, tmp_path):
    plan_path = tmp_path / "plan.md"
    plan_path.write_text("# Plan\n- [ ] thing\n")
    out_path = handoff.auto_name_out(plan_path)
    sig_path = out_path.parent / (out_path.stem + ".state.json")
    client = WritingHandoffClient(
        out_path=out_path,
        signal_path=sig_path,
        signal_payload={
            "exit_state": "chain-done",
            "child_plan": None,
            "reason": None,
            "operator_followups": [],
        },
    )
    monkeypatch.setattr(handoff.shutil, "which", lambda n: "/fake/claude")
    monkeypatch.setattr(
        handoff,
        "to_client",
        lambda spec: setattr(client, "_gremlins_client_spec", str(spec)) or client,
    )
    monkeypatch.setattr(
        handoff,
        "collect_git_context",
        lambda base_ref, rev=None: ("test-branch", "log line", "diff body"),
    )
    monkeypatch.setattr(handoff, "_load_handoff_style", lambda: "Keep it simple.")

    rc = handoff.main(["--plan", str(plan_path), "--client", "claude:haiku"])
    assert rc == 0
    assert [call.label for call in client.calls] == ["handoff", "handoff:sanitize"]
    assert client.calls[0].model == "haiku"


# ---------------------------------------------------------------------------
# build_sanitize_prompt — rule coverage
# ---------------------------------------------------------------------------


def test_build_sanitize_prompt_rules(tmp_path):
    out_path = tmp_path / "rolling.md"
    prompt = handoff.build_sanitize_prompt("# Plan\n- [ ] do thing\n", out_path)
    # Each prohibited pattern must be explicitly named
    assert "[x]" in prompt
    assert "~~" in prompt or "struck-through" in prompt.lower()
    assert "H1" in prompt or "# ..." in prompt
    assert str(out_path) in prompt
    # Prose-about-landing and bullet-of-completed-items
    assert any(
        word in prompt.lower() for word in ("landed", "shipped", "merged", "completed")
    )
    assert "bullet" in prompt.lower()


# ---------------------------------------------------------------------------
# sanitize_rolling_plan — behaviour
# ---------------------------------------------------------------------------


def test_sanitize_rolling_plan_rewrites_file(monkeypatch, tmp_path):
    out_path = tmp_path / "rolling.md"
    out_path.write_text("# Bad Plan\nPhases 0–3 have landed.\n- [ ] remaining task\n")
    cleaned = "# Remaining Work\n- [ ] remaining task\n"
    client = WritingHandoffClient(
        out_path=out_path,
        signal_path=tmp_path / "unused.state.json",
        signal_payload={},
        sanitize_text=cleaned,
    )
    handoff.sanitize_rolling_plan(client, out_path, ClientSpec.parse("claude:sonnet"))
    assert out_path.read_text() == cleaned


def test_sanitize_rolling_plan_nonzero_is_nonfatal(monkeypatch, tmp_path, capsys):
    out_path = tmp_path / "rolling.md"
    original = "# Rolling plan\n- [ ] task\n"
    out_path.write_text(original)
    client = WritingHandoffClient(
        out_path=out_path,
        signal_path=tmp_path / "unused.state.json",
        signal_payload={},
        sanitize_error=RuntimeError("sanitize failed"),
    )
    handoff.sanitize_rolling_plan(client, out_path, ClientSpec.parse("claude:sonnet"))
    err = capsys.readouterr().err
    assert "warning" in err.lower()
    assert out_path.read_text() == original


# ---------------------------------------------------------------------------
# sanitize regression — fixture violations
# ---------------------------------------------------------------------------


def test_sanitize_prompt_rejects_phases_preamble():
    bad = (FIXTURES / "handoff_bad_next_plan.md").read_text()
    prompt = handoff.build_sanitize_prompt(bad, pathlib.Path("/tmp/out.md"))
    # Prompt must contain a rule covering "Phases 0–3 have landed"-style prose
    assert any(
        word in prompt.lower() for word in ("landed", "shipped", "merged", "completed")
    )
    # The bad content is present for the agent to rewrite
    assert bad in prompt


def test_sanitize_prompt_rejects_chain_complete_enumeration():
    bad = (FIXTURES / "handoff_bad_chain_done.md").read_text()
    prompt = handoff.build_sanitize_prompt(bad, pathlib.Path("/tmp/out.md"))
    # Prompt must contain a rule covering bullet enumerations of completed items
    assert "bullet" in prompt.lower()
    assert bad in prompt
