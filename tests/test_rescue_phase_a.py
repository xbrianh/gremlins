"""Shell integration tests for `/gremlins rescue` Phase A (the diagnosis step).

The diagnosis step spawns ``claude -p`` with the rescue prompt and reads a
verdict marker from a known path inside the gremlin's artifacts dir. The
contract this layer protects:

- The diagnosis agent runs in a *scratch* directory, not the gremlin's
  worktree. The worktree path is named in the prompt for read access; it
  must not become the agent's cwd.
- The agent's verdicts (``fixed`` / ``transient`` / ``structural`` /
  ``unsalvageable``) drive whether the wrapper writes a bail reason or
  proceeds to relaunch.
- A missing / malformed marker results in a wrapper-level bail
  (``diagnosis_no_marker`` / ``diagnosis_bad_marker``), never silent
  success.

Out of scope for this layer — writability restriction:
  The plan target "only ``state.json`` is writable, ``unsalvageable``
  declared for anything outside that" is **not implemented or testable
  here**.  This wrapper does not enforce any file-system restriction.
  The rescue prompt's "permitted edits" text is guidance only, and the
  current prompt wording allows edits to files in the gremlin worktree
  as well as ``state.json``.  In headless mode the wrapper runs with
  ``--permission-mode bypassPermissions``, so Claude's own permission
  system doesn't gate writes either.  A fake binary can write anything
  it likes; nothing in the wrapper observes or reacts to unauthorized
  writes.  Testing this contract would require either running a real
  Claude agent or adding a wrapper-level enforcement mechanism —
  neither of which exists today.
"""

from __future__ import annotations

import datetime
import json
import pathlib
import re
import subprocess

from fixtures.shell_env import (
    install_fake_bin,
    read_fake_claude_log,
    setup_shell_env,
)

import gremlins.fleet.rescue as rescue_mod
from gremlins.fleet.rescue import do_rescue as _do_rescue
from gremlins.launcher import GremlinAlreadyRunning


def _make_failed_gremlin(
    state_root: pathlib.Path, workdir: pathlib.Path, gremlin_id: str = "victim-abcdef"
) -> pathlib.Path:
    """Create the on-disk shape of a gremlin that crashed and is awaiting rescue.

    Returns the state dir path.
    """
    state_dir = state_root / gremlin_id
    state_dir.mkdir(parents=True)
    state = {
        "id": gremlin_id,
        "kind": "localgremlin",
        "stage": "implement",
        "status": "stopped",
        "exit_code": 1,
        "workdir": str(workdir),
        "project_root": str(workdir.parent),
        "description": "test gremlin",
        "started_at": datetime.datetime.now(datetime.UTC).strftime(
            "%Y-%m-%dT%H:%M:%SZ"
        ),
        "rescue_count": 0,
    }
    (state_dir / "state.json").write_text(json.dumps(state), encoding="utf-8")
    (state_dir / "log").write_text("fake log tail\n", encoding="utf-8")
    (state_dir / "finished").touch()
    return state_dir


def test_rescue_diagnosis_runs_in_scratch_dir_not_worktree(
    tmp_path, sandbox, monkeypatch
):
    """The diagnosis agent's cwd is a /tmp scratch dir, not the gremlin's worktree."""
    sh = setup_shell_env(tmp_path)
    _make_failed_gremlin(sandbox.state, sh.repo)

    # HOME + PATH already steered by setup_shell_env. Tell our fake claude
    # to declare unsalvageable so the wrapper bails (no relaunch needed)
    # — this test only cares about cwd, not the relaunch path.
    sh.env["FAKE_CLAUDE_RESCUE_VERDICT"] = "unsalvageable"
    for k, v in sh.env.items():
        monkeypatch.setenv(k, v)

    ok = _do_rescue("victim-abcdef", headless=False)
    # unsalvageable verdict returns False (no relaunch). That's expected.
    assert ok is False

    log = read_fake_claude_log(sh.fake_claude_log)
    rescue_calls = [e for e in log if e["stage"] == "rescue-diagnosis"]
    assert len(rescue_calls) == 1, log
    cwd = rescue_calls[0]["cwd"]
    # cwd must be a scratch dir, not the worktree.
    assert pathlib.Path(cwd).resolve() != sh.repo.resolve(), (
        f"diagnosis must run in scratch, not worktree ({cwd})"
    )
    assert "gremlin-rescue-" in cwd, (
        f"expected scratch dir prefix gremlin-rescue-, got {cwd}"
    )


def test_rescue_unsalvageable_records_bail(tmp_path, sandbox, monkeypatch):
    """An ``unsalvageable`` marker writes ``bail_reason=unsalvageable``."""
    sh = setup_shell_env(tmp_path)
    state_dir = _make_failed_gremlin(sandbox.state, sh.repo)

    sh.env["FAKE_CLAUDE_RESCUE_VERDICT"] = "unsalvageable"
    sh.env["FAKE_CLAUDE_RESCUE_SUMMARY"] = "worktree gone"
    for k, v in sh.env.items():
        monkeypatch.setenv(k, v)

    _do_rescue("victim-abcdef", headless=False)

    state = json.loads((state_dir / "state.json").read_text())
    assert state["bail_reason"] == "unsalvageable"
    assert "worktree gone" in state.get("bail_detail", "")
    assert state["status"] == "bailed"


def test_rescue_structural_records_bail(tmp_path, sandbox, monkeypatch):
    """A ``structural`` marker writes ``bail_reason=structural`` with the agent's summary."""
    sh = setup_shell_env(tmp_path)
    state_dir = _make_failed_gremlin(sandbox.state, sh.repo)

    sh.env["FAKE_CLAUDE_RESCUE_VERDICT"] = "structural"
    sh.env["FAKE_CLAUDE_RESCUE_SUMMARY"] = "pipeline bug in foo.py"
    for k, v in sh.env.items():
        monkeypatch.setenv(k, v)

    _do_rescue("victim-abcdef", headless=False)

    state = json.loads((state_dir / "state.json").read_text())
    assert state["bail_reason"] == "structural"
    assert "pipeline bug in foo.py" in state.get("bail_detail", "")


def test_rescue_no_marker_records_diagnosis_no_marker(tmp_path, sandbox, monkeypatch):
    """Agent that returns 0 without writing the marker → diagnosis_no_marker bail."""
    sh = setup_shell_env(tmp_path)
    # Override the fake claude with a stub that emits no marker but exits 0.
    no_marker_claude = tmp_path / "no_marker_claude.py"
    no_marker_claude.write_text(
        "#!/usr/bin/env python\nimport sys\nsys.exit(0)\n",
        encoding="utf-8",
    )
    install_fake_bin(sh.bin_dir, "claude", no_marker_claude)

    state_dir = _make_failed_gremlin(sandbox.state, sh.repo)

    for k, v in sh.env.items():
        monkeypatch.setenv(k, v)

    _do_rescue("victim-abcdef", headless=False)

    state = json.loads((state_dir / "state.json").read_text())
    assert state["bail_reason"] == "diagnosis_no_marker"


def test_rescue_fixed_verdict_invokes_launcher_resume(tmp_path, sandbox, monkeypatch):
    """A ``fixed`` marker triggers ``launcher.resume(<id>)``.

    Monkeypatches ``gremlins.launcher.resume`` so we don't actually fork a
    background pipeline; the test verifies the wrapper called it with the
    right gremlin id and reported relaunch_outcome=success.
    """
    sh = setup_shell_env(tmp_path)
    _make_failed_gremlin(sandbox.state, sh.repo)

    resume_calls = []

    monkeypatch.setattr(
        rescue_mod, "_resume", lambda gremlin_id: resume_calls.append(gremlin_id)
    )

    sh.env["FAKE_CLAUDE_RESCUE_VERDICT"] = "fixed"
    sh.env["FAKE_CLAUDE_RESCUE_SUMMARY"] = "edited state.json"
    for k, v in sh.env.items():
        monkeypatch.setenv(k, v)

    ok = _do_rescue("victim-abcdef", headless=False)
    assert ok is True

    assert len(resume_calls) == 1, resume_calls
    assert resume_calls[0] == "victim-abcdef"


def test_rescue_already_running_returns_true_no_bail(tmp_path, sandbox, monkeypatch):
    """When _resume raises GremlinAlreadyRunning, do_rescue returns True without bailing."""
    sh = setup_shell_env(tmp_path)
    state_dir = _make_failed_gremlin(sandbox.state, sh.repo)

    monkeypatch.setattr(
        rescue_mod,
        "_run_headless_diagnosis",
        lambda *a, **kw: ("fixed", "state looks good"),
    )

    def _raise_already_running(gremlin_id: str) -> None:
        raise GremlinAlreadyRunning(
            f"gremlin {gremlin_id} is still running (pid 99999) — stop it first"
        )

    monkeypatch.setattr(rescue_mod, "_resume", _raise_already_running)

    ok = rescue_mod.do_rescue("victim-abcdef", headless=True)
    assert ok is True

    state = json.loads((state_dir / "state.json").read_text())
    assert state.get("bail_reason") != "relaunch_failed"
    assert "finished" not in state.get("status", "")


def test_rescue_headless_excluded_class_refused(tmp_path, sandbox, monkeypatch):
    """Headless rescue refuses gremlins whose bail_class is in the exclusion list."""
    sh = setup_shell_env(tmp_path)
    state_dir = _make_failed_gremlin(sandbox.state, sh.repo)
    # Add an excluded bail_class to the existing victim state.
    state = json.loads((state_dir / "state.json").read_text())
    state["bail_class"] = "secrets"
    state["bail_detail"] = "diff touches secrets"
    (state_dir / "state.json").write_text(json.dumps(state), encoding="utf-8")

    for k, v in sh.env.items():
        monkeypatch.setenv(k, v)

    ok = _do_rescue("victim-abcdef", headless=True)
    assert ok is False

    final = json.loads((state_dir / "state.json").read_text())
    assert final["bail_reason"] == "excluded_class:secrets"
    # Fake claude must not have been spawned at all.
    log = read_fake_claude_log(sh.fake_claude_log)
    assert all(e["stage"] != "rescue-diagnosis" for e in log), log


def test_rescue_nonzero_exit_records_diagnosis_claude_error(
    tmp_path, sandbox, monkeypatch
):
    """Agent that exits non-zero → do_rescue returns False with diagnosis_claude_error."""
    sh = setup_shell_env(tmp_path)
    failing_claude = tmp_path / "failing_claude.py"
    failing_claude.write_text(
        "import sys\nsys.exit(42)\n",
        encoding="utf-8",
    )
    install_fake_bin(sh.bin_dir, "claude", failing_claude)

    state_dir = _make_failed_gremlin(sandbox.state, sh.repo)

    for k, v in sh.env.items():
        monkeypatch.setenv(k, v)

    ok = _do_rescue("victim-abcdef", headless=False)
    assert ok is False

    state = json.loads((state_dir / "state.json").read_text())
    assert state["bail_reason"] == "diagnosis_claude_error"


def test_rescue_claude_not_found_records_diagnosis_claude_error(
    tmp_path, sandbox, monkeypatch
):
    """Missing claude binary → do_rescue returns False with diagnosis_claude_error."""
    sh = setup_shell_env(tmp_path)
    state_dir = _make_failed_gremlin(sandbox.state, sh.repo)

    for k, v in sh.env.items():
        monkeypatch.setenv(k, v)

    original_popen = subprocess.Popen

    def popen_raise_if_claude(cmd, *args, **kwargs):
        if cmd and cmd[0] == "claude":
            raise FileNotFoundError("claude not found")
        return original_popen(cmd, *args, **kwargs)

    monkeypatch.setattr(subprocess, "Popen", popen_raise_if_claude)
    # Also patch it in the rescue module's own subprocess reference.
    monkeypatch.setattr(rescue_mod.subprocess, "Popen", popen_raise_if_claude)

    ok = _do_rescue("victim-abcdef", headless=False)
    assert ok is False

    state = json.loads((state_dir / "state.json").read_text())
    assert state["bail_reason"] == "diagnosis_claude_error"


def test_rescue_headless_excluded_class_short_circuits(tmp_path, sandbox, monkeypatch):
    """Plain headless rescue refuses on EXCLUDED_BAIL_CLASSES without --from-boss."""
    sh = setup_shell_env(tmp_path)
    state_dir = _make_failed_gremlin(sandbox.state, sh.repo)
    # Set bail_class to an excluded class
    state = json.loads((state_dir / "state.json").read_text())
    state["bail_class"] = "reviewer_requested_changes"
    (state_dir / "state.json").write_text(json.dumps(state))

    agent_calls = []

    def fake_diagnosis(*a, **kw):
        agent_calls.append(True)
        return "fixed", ""

    monkeypatch.setattr(rescue_mod, "_run_headless_diagnosis", fake_diagnosis)

    ok = rescue_mod.do_rescue("victim-abcdef", headless=True, from_boss=False)
    assert ok is False
    assert agent_calls == [], (
        "diagnosis agent should not run for excluded class without --from-boss"
    )

    updated = json.loads((state_dir / "state.json").read_text())
    assert updated["bail_reason"].startswith("excluded_class:")


def test_rescue_from_boss_bypasses_excluded_class(tmp_path, sandbox, monkeypatch):
    """--from-boss --headless rescue on an excluded-class bail invokes the diagnosis agent."""
    sh = setup_shell_env(tmp_path)
    state_dir = _make_failed_gremlin(sandbox.state, sh.repo)
    # Set bail_class to an excluded class
    state = json.loads((state_dir / "state.json").read_text())
    state["bail_class"] = "reviewer_requested_changes"
    (state_dir / "state.json").write_text(json.dumps(state))

    agent_calls = []

    def fake_diagnosis(workdir, prompt, marker_path):
        agent_calls.append({"workdir": workdir, "prompt": prompt})
        # Return "structural" so do_rescue returns False without trying to relaunch
        return "structural", "test sentinel"

    monkeypatch.setattr(rescue_mod, "_run_headless_diagnosis", fake_diagnosis)

    ok = rescue_mod.do_rescue("victim-abcdef", headless=True, from_boss=True)
    assert ok is False  # structural → no relaunch
    assert len(agent_calls) == 1, "diagnosis agent must run when --from-boss is set"


def test_rescue_diagnosis_streams_events_to_stderr(
    tmp_path, sandbox, monkeypatch, capsys
):
    """Interactive rescue emits [rescue]-prefixed stream-json events to stderr."""
    sh = setup_shell_env(tmp_path)
    _make_failed_gremlin(sandbox.state, sh.repo)

    # Minimal fake claude: emit stream-json events, write unsalvageable marker.
    # Use "unsalvageable" so there's no relaunch step and the test stays self-contained.
    streaming_claude_py = tmp_path / "streaming_claude.py"
    streaming_claude_py.write_text(
        "import json, os, pathlib, sys\n"
        "prompt = sys.argv[-1]\n"
        "for evt in [\n"
        '    {"type": "system", "subtype": "init",\n'
        '     "model": "fake", "cwd": os.getcwd()},\n'
        '    {"type": "assistant", "message": {"content": [\n'
        '        {"type": "text", "text": "Diagnosing..."}]}},\n'
        '    {"type": "result", "subtype": "success", "num_turns": 1, "total_cost_usd": 0},\n'
        "]:\n"
        '    sys.stdout.write(json.dumps(evt) + "\\n")\n'
        "    sys.stdout.flush()\n"
        "for word in prompt.split():\n"
        "    word = word.strip('`\"\\'')\n"
        "    if word.endswith('.done') and word.startswith('/'):\n"
        "        pathlib.Path(word).parent.mkdir(parents=True, exist_ok=True)\n"
        '        pathlib.Path(word).write_text(json.dumps({"status": "unsalvageable", "summary": "streaming test"}))\n'
        "        break\n",
        encoding="utf-8",
    )
    install_fake_bin(sh.bin_dir, "claude", streaming_claude_py)

    for k, v in sh.env.items():
        monkeypatch.setenv(k, v)

    _do_rescue("victim-abcdef", headless=False)

    captured = capsys.readouterr()
    _TS = r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z"
    rescue_lines = [
        ln for ln in captured.err.splitlines() if re.match(rf"{_TS} \[rescue\]", ln)
    ]
    assert len(rescue_lines) >= 2, (
        f"Expected [rescue]-prefixed events on stderr; got: {captured.err!r}"
    )
    assert any("init" in ln for ln in rescue_lines), (
        f"Expected init event line: {rescue_lines}"
    )
    assert any("text:" in ln for ln in rescue_lines), (
        f"Expected text event line: {rescue_lines}"
    )


def test_write_rescue_report_uses_client_label_without_model(tmp_path):
    """Rescue reports use the failed stage's persisted client label."""
    state_dir = tmp_path / "state"
    state_dir.mkdir()

    rescue_mod.write_rescue_report(
        str(state_dir),
        {
            "state": {
                "id": "victim-abcdef",
                "kind": "localgremlin",
                "stage": "implement",
                "client": "claude:opus",
            },
            "attempt_number": 1,
            "headless": False,
            "verdict": "structural",
            "summary": "test summary",
            "relaunch_outcome": "skipped",
        },
    )

    reports = list(state_dir.glob("rescue-*.md"))
    assert len(reports) == 1
    text = reports[0].read_text(encoding="utf-8")
    assert "- Client: claude:opus" in text
    assert "- Model:" not in text
