"""Tests for gremlins/handoff.py."""

import json
import pathlib
import subprocess

import pytest

from gremlins import handoff

FIXTURES = pathlib.Path(__file__).parent / "fixtures"


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
    assert args.model == "sonnet"
    assert args.timeout is None
    assert args.rev is None


def test_parse_args_full():
    args = handoff.parse_args([
        "--plan", "/p.md", "--spec", "/s.md", "--out", "/o.md",
        "--base", "develop", "--model", "haiku", "--timeout", "300",
        "--rev", "feature-branch",
    ])
    assert args.plan == "/p.md"
    assert args.spec == "/s.md"
    assert args.out == "/o.md"
    assert args.base == "develop"
    assert args.model == "haiku"
    assert args.timeout == 300
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
        plan_text="plan", branch="b", git_log="", git_diff="x" * 100000,
        out_path=out_p, child_plan_path=child_p, signal_path=sig_p,
    )
    assert "diff truncated to 50000 chars" in p


def test_build_prompt_handles_empty_diff_and_log(tmp_path):
    out_p, child_p, sig_p = _prompt_paths(tmp_path)
    p = handoff.build_prompt(
        plan_text="plan", branch="b", git_log="", git_diff="",
        out_path=out_p, child_plan_path=child_p, signal_path=sig_p,
    )
    assert "(empty — no changes yet)" in p
    assert "(no commits yet — branch just started)" in p


def test_build_prompt_includes_spec_when_given(tmp_path):
    out_p, child_p, sig_p = _prompt_paths(tmp_path)
    p = handoff.build_prompt(
        plan_text="plan", branch="b", git_log="", git_diff="",
        out_path=out_p, child_plan_path=child_p, signal_path=sig_p,
        spec_text="Overall north-star context",
    )
    assert "Overall north-star context" in p
    assert "Overarching goal" in p


def test_build_prompt_omits_spec_section_by_default(tmp_path):
    out_p, child_p, sig_p = _prompt_paths(tmp_path)
    p = handoff.build_prompt(
        plan_text="plan", branch="b", git_log="", git_diff="",
        out_path=out_p, child_plan_path=child_p, signal_path=sig_p,
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
        handoff.main([
            "--plan", str(p),
            "--out", str(tmp_path / "missing-dir" / "out.md"),
        ])
    err = capsys.readouterr().err
    assert "parent directory does not exist" in err


# ---------------------------------------------------------------------------
# main — signal parsing (next-plan / chain-done / bail)
# ---------------------------------------------------------------------------

def _stub_happy_main(monkeypatch, tmp_path, signal_payload, *,
                      child_plan_text=None, claude_returncode=0,
                      claude_timeout=False):
    """Set up a happy-path main() that writes the given signal payload.

    If `child_plan_text` is given, the fake claude run also writes the
    child plan file to whatever path the prompt says it should go to.
    Returns the plan path so the caller can pass it to handoff.main.

    The first subprocess.run call is the main agent (writes sig_path and
    out_path); the second call is the sanitize pass (treated as a no-op).
    """
    plan_path = tmp_path / "plan.md"
    plan_path.write_text("# Plan\nTasks\n- [ ] thing\n")

    monkeypatch.setattr(handoff.shutil, "which", lambda n: "/fake/claude")
    monkeypatch.setattr(handoff, "collect_git_context",
                        lambda base_ref, rev=None: ("test-branch", "log line", "diff body"))

    out_path = handoff.auto_name_out(plan_path)
    sig_path = out_path.parent / (out_path.stem + ".state.json")
    child_path = out_path.parent / (out_path.stem + "-child" + out_path.suffix)

    call_count = [0]

    def fake_run(cmd, **kwargs):
        if claude_timeout:
            raise subprocess.TimeoutExpired(cmd=cmd, timeout=kwargs.get("timeout") or 1)
        call_count[0] += 1
        if call_count[0] == 1:
            # Main agent call
            if claude_returncode == 0:
                sig_path.write_text(json.dumps(signal_payload))
                out_path.write_text("# Rolling plan (stub)\n")
                if child_plan_text is not None:
                    child_path.write_text(child_plan_text)
            return subprocess.CompletedProcess(args=cmd, returncode=claude_returncode)
        # Second call is the sanitize pass — no-op
        return subprocess.CompletedProcess(args=cmd, returncode=0)

    monkeypatch.setattr(handoff.subprocess, "run", fake_run)
    return plan_path, sig_path, child_path


def test_main_chain_done_signal(monkeypatch, tmp_path, capsys):
    plan_path, sig_path, _ = _stub_happy_main(monkeypatch, tmp_path, {
        "exit_state": "chain-done",
        "child_plan": None,
        "reason": None,
        "operator_followups": [],
    })
    rc = handoff.main(["--plan", str(plan_path)])
    assert rc == 0
    out = capsys.readouterr().out
    assert "handoff complete: chain-done" in out
    assert sig_path.exists()


def test_main_next_plan_signal(monkeypatch, tmp_path, capsys):
    plan_path, sig_path, child_path = _stub_happy_main(
        monkeypatch, tmp_path,
        {
            "exit_state": "next-plan",
            "child_plan": None,  # filled in by the fake_run write below
            "reason": None,
            "operator_followups": [],
        },
        child_plan_text="# Next step\n",
    )
    # Patch the signal payload to point at the child plan path the wrapper expects.
    payload = {
        "exit_state": "next-plan",
        "child_plan": str(child_path),
        "reason": None,
        "operator_followups": [],
    }
    out_path = handoff.auto_name_out(plan_path)
    call_count2 = [0]

    def fake_run2(cmd, **kw):
        call_count2[0] += 1
        if call_count2[0] == 1:
            sig_path.write_text(json.dumps(payload))
            child_path.write_text("# Child\n")
            out_path.write_text("# Rolling plan\n")
        return subprocess.CompletedProcess(args=cmd, returncode=0)

    monkeypatch.setattr(handoff, "collect_git_context",
                        lambda base_ref, rev=None: ("b", "", ""))
    monkeypatch.setattr(handoff.subprocess, "run", fake_run2)

    rc = handoff.main(["--plan", str(plan_path)])
    assert rc == 0
    out = capsys.readouterr().out
    assert "handoff complete: next-plan" in out
    assert "child plan:" in out
    assert child_path.exists()


def test_main_bail_signal(monkeypatch, tmp_path, capsys):
    plan_path, _, _ = _stub_happy_main(monkeypatch, tmp_path, {
        "exit_state": "bail",
        "child_plan": None,
        "reason": "incoherent state — see plan",
        "operator_followups": [],
    })
    rc = handoff.main(["--plan", str(plan_path)])
    assert rc == 0
    out = capsys.readouterr().out
    assert "handoff complete: bail" in out
    assert "incoherent state" in out


def test_main_signal_file_not_written(monkeypatch, tmp_path, capsys):
    plan_path = tmp_path / "plan.md"
    plan_path.write_text("# Plan\n")
    monkeypatch.setattr(handoff.shutil, "which", lambda n: "/fake/claude")
    monkeypatch.setattr(handoff, "collect_git_context",
                        lambda base_ref, rev=None: ("b", "", ""))
    # claude returns success but writes no signal file
    monkeypatch.setattr(handoff.subprocess, "run",
                        lambda cmd, **kw: subprocess.CompletedProcess(args=cmd, returncode=0))
    rc = handoff.main(["--plan", str(plan_path)])
    assert rc == 1
    err = capsys.readouterr().err
    assert "signal file not written" in err


def test_main_signal_file_invalid_json(monkeypatch, tmp_path, capsys):
    plan_path, sig_path, _ = _stub_happy_main(monkeypatch, tmp_path, {})
    # Overwrite with garbage
    monkeypatch.setattr(handoff.subprocess, "run", lambda cmd, **kw: (
        sig_path.write_text("not json"),
        subprocess.CompletedProcess(args=cmd, returncode=0),
    )[-1])
    rc = handoff.main(["--plan", str(plan_path)])
    assert rc == 1
    err = capsys.readouterr().err
    assert "could not parse signal file" in err


def test_main_signal_unknown_exit_state(monkeypatch, tmp_path, capsys):
    plan_path, _, _ = _stub_happy_main(monkeypatch, tmp_path, {
        "exit_state": "bogus",
    })
    rc = handoff.main(["--plan", str(plan_path)])
    assert rc == 1
    err = capsys.readouterr().err
    assert "unrecognized exit_state" in err


def test_main_next_plan_missing_child_plan(monkeypatch, tmp_path, capsys):
    plan_path, _, _ = _stub_happy_main(monkeypatch, tmp_path, {
        "exit_state": "next-plan",
        "child_plan": None,
        "reason": None,
        "operator_followups": [],
    })
    rc = handoff.main(["--plan", str(plan_path)])
    assert rc == 1
    err = capsys.readouterr().err
    assert "child_plan is null" in err


def test_main_next_plan_child_plan_path_does_not_exist(monkeypatch, tmp_path, capsys):
    plan_path, _, child_path = _stub_happy_main(monkeypatch, tmp_path, {
        "exit_state": "next-plan",
        "child_plan": str(tmp_path / "no-such-child.md"),
        "reason": None,
        "operator_followups": [],
    })
    rc = handoff.main(["--plan", str(plan_path)])
    assert rc == 1
    err = capsys.readouterr().err
    assert "child plan path in signal file does not exist" in err


def test_main_claude_nonzero_exit(monkeypatch, tmp_path, capsys):
    plan_path, _, _ = _stub_happy_main(monkeypatch, tmp_path, {},
                                         claude_returncode=2)
    rc = handoff.main(["--plan", str(plan_path)])
    assert rc == 1
    err = capsys.readouterr().err
    assert "claude -p exited 2" in err


def test_main_claude_timeout(monkeypatch, tmp_path, capsys):
    plan_path, _, _ = _stub_happy_main(monkeypatch, tmp_path, {},
                                         claude_timeout=True)
    rc = handoff.main(["--plan", str(plan_path), "--timeout", "5"])
    assert rc == 1
    err = capsys.readouterr().err
    assert "timed out" in err


def test_main_operator_followups_printed(monkeypatch, tmp_path, capsys):
    plan_path, _, _ = _stub_happy_main(monkeypatch, tmp_path, {
        "exit_state": "chain-done",
        "child_plan": None,
        "reason": None,
        "operator_followups": ["Sync ~/.claude/", "Run smoke test manually"],
    })
    rc = handoff.main(["--plan", str(plan_path)])
    assert rc == 0
    out = capsys.readouterr().out
    assert "operator follow-ups (2):" in out
    assert "Sync ~/.claude/" in out
    assert "Run smoke test manually" in out


# ---------------------------------------------------------------------------
# main — spec is best-effort
# ---------------------------------------------------------------------------

def test_main_missing_spec_warns_and_continues(monkeypatch, tmp_path, capsys):
    plan_path, _, _ = _stub_happy_main(monkeypatch, tmp_path, {
        "exit_state": "chain-done",
        "child_plan": None,
        "reason": None,
        "operator_followups": [],
    })
    rc = handoff.main([
        "--plan", str(plan_path),
        "--spec", str(tmp_path / "no-such-spec.md"),
    ])
    assert rc == 0
    err = capsys.readouterr().err
    assert "warning" in err.lower()
    assert "spec" in err.lower()


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
    assert any(word in prompt.lower() for word in ("landed", "shipped", "merged", "completed"))
    assert "bullet" in prompt.lower()


# ---------------------------------------------------------------------------
# sanitize_rolling_plan — behaviour
# ---------------------------------------------------------------------------

def test_sanitize_rolling_plan_rewrites_file(monkeypatch, tmp_path):
    out_path = tmp_path / "rolling.md"
    out_path.write_text("# Bad Plan\nPhases 0–3 have landed.\n- [ ] remaining task\n")
    cleaned = "# Remaining Work\n- [ ] remaining task\n"

    def fake_run(cmd, **kwargs):
        out_path.write_text(cleaned)
        return subprocess.CompletedProcess(args=cmd, returncode=0)

    monkeypatch.setattr(handoff.subprocess, "run", fake_run)
    handoff.sanitize_rolling_plan(out_path, timeout=None)
    assert out_path.read_text() == cleaned


def test_sanitize_rolling_plan_nonzero_is_nonfatal(monkeypatch, tmp_path, capsys):
    plan_path = tmp_path / "plan.md"
    plan_path.write_text("# Plan\n- [ ] task\n")
    monkeypatch.setattr(handoff.shutil, "which", lambda n: "/fake/claude")
    monkeypatch.setattr(handoff, "collect_git_context",
                        lambda base_ref, rev=None: ("b", "", ""))

    out_path = handoff.auto_name_out(plan_path)
    sig_path = out_path.parent / (out_path.stem + ".state.json")
    payload = {
        "exit_state": "chain-done",
        "child_plan": None,
        "reason": None,
        "operator_followups": [],
    }

    call_count = [0]

    def fake_run(cmd, **kwargs):
        call_count[0] += 1
        if call_count[0] == 1:
            sig_path.write_text(json.dumps(payload))
            out_path.write_text("# Rolling plan\n")
            return subprocess.CompletedProcess(args=cmd, returncode=0)
        # Sanitize pass — return non-zero
        return subprocess.CompletedProcess(args=cmd, returncode=1)

    monkeypatch.setattr(handoff.subprocess, "run", fake_run)
    rc = handoff.main(["--plan", str(plan_path)])
    assert rc == 0
    err = capsys.readouterr().err
    assert "warning" in err.lower()


# ---------------------------------------------------------------------------
# sanitize regression — fixture violations
# ---------------------------------------------------------------------------

def test_sanitize_prompt_rejects_phases_preamble():
    bad = (FIXTURES / "handoff_bad_next_plan.md").read_text()
    prompt = handoff.build_sanitize_prompt(bad, pathlib.Path("/tmp/out.md"))
    # Prompt must contain a rule covering "Phases 0–3 have landed"-style prose
    assert any(word in prompt.lower() for word in ("landed", "shipped", "merged", "completed"))
    # The bad content is present for the agent to rewrite
    assert bad in prompt


def test_sanitize_prompt_rejects_chain_complete_enumeration():
    bad = (FIXTURES / "handoff_bad_chain_done.md").read_text()
    prompt = handoff.build_sanitize_prompt(bad, pathlib.Path("/tmp/out.md"))
    # Prompt must contain a rule covering bullet enumerations of completed items
    assert "bullet" in prompt.lower()
    assert bad in prompt
