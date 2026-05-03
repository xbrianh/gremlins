"""Tests for gremlins.orchestrators.gh and supporting git helpers.

Uses FakeClaudeClient throughout — no real claude subprocess or gh CLI calls
(gh calls are monkeypatched at the subprocess.run level).
"""

import json
import pathlib
import shutil
import subprocess

import pytest

import gremlins.orchestrators.gh as _gh_mod
from gremlins.clients.fake import FakeClaudeClient
from gremlins.git import (
    DirtyOnly,
    DivergentHead,
    EmptyImpl,
    HeadAdvanced,
    PreImplState,
    classify_impl_outcome,
    create_handoff_branch,
    record_pre_impl_state,
    sweep_stale_handoff_branches,
)
from gremlins.orchestrators.gh import _parse_gh_args, _parse_issue_ref, gh_main

# ---------------------------------------------------------------------------
# Helper: minimal stream-json event list containing a PR URL in a tool_result
# ---------------------------------------------------------------------------


def _issue_events(issue_url: str = "https://github.com/owner/repo/issues/42") -> list:
    return [
        {"type": "system", "subtype": "init", "session_id": "session-plan-1"},
        {
            "type": "assistant",
            "message": {
                "content": [
                    {
                        "type": "tool_use",
                        "id": "tu-plan-1",
                        "name": "Bash",
                        "input": {"command": "gh issue create --title 'foo'"},
                    }
                ]
            },
        },
        {
            "type": "user",
            "message": {
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "tu-plan-1",
                        "content": issue_url,
                    }
                ]
            },
        },
        {"type": "result", "subtype": "success"},
    ]


def _pr_events(pr_url: str = "https://github.com/owner/repo/pull/101") -> list:
    return [
        {"type": "system", "subtype": "init", "session_id": "session-commit-1"},
        {
            "type": "assistant",
            "message": {
                "content": [
                    {
                        "type": "tool_use",
                        "id": "tu-pr-1",
                        "name": "Bash",
                        "input": {"command": "gh pr create --base main"},
                    }
                ]
            },
        },
        {
            "type": "user",
            "message": {
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "tu-pr-1",
                        "content": pr_url,
                    }
                ]
            },
        },
        {"type": "result", "subtype": "success"},
    ]


IMPL_EVENTS = [
    {"type": "system", "subtype": "init", "session_id": "session-impl-1"},
    {"type": "result", "subtype": "success"},
]


# ---------------------------------------------------------------------------
# Common patches for gh_main smoke tests
# ---------------------------------------------------------------------------


def _patch_common(monkeypatch, tmp_path, *, state_data: dict = None):
    """Apply standard monkeypatches for gh_main smoke tests."""
    monkeypatch.setattr(
        shutil, "which", lambda n: f"/fake/{n}" if n in ("claude", "gh") else None
    )
    monkeypatch.setattr(
        "gremlins.orchestrators.gh.install_signal_handlers", lambda c: None
    )
    monkeypatch.setattr("gremlins.orchestrators.gh.get_repo", lambda: "owner/repo")
    monkeypatch.setattr(
        "gremlins.orchestrators.gh.load_prompts", lambda paths: "Be good."
    )

    session_dir = tmp_path / "session"
    session_dir.mkdir()
    monkeypatch.setattr(
        "gremlins.orchestrators.gh.resolve_session_dir", lambda gr_id=None: session_dir
    )

    state_file = tmp_path / "state.json"
    initial = {
        "id": "gr-test",
        "kind": "ghgremlin",
        "stage": "starting",
        "bail_class": "",
    }
    if state_data:
        initial.update(state_data)
    state_file.write_text(json.dumps(initial))
    monkeypatch.setattr(
        "gremlins.orchestrators.gh.resolve_state_file", lambda gr_id=None: state_file
    )

    # Stub out patch_state so tests don't write to real state files.
    monkeypatch.setattr(
        "gremlins.orchestrators.gh.patch_state", lambda gr_id=None, **kw: None
    )

    # set_stage is a no-op in tests — gr_id is not passed to gh_main.

    return session_dir, state_file


_real_subprocess_run = subprocess.run


def _make_gh_subprocess(
    *,
    issue_body: str = "# Plan\nDo stuff.\n",
    copilot_state: str = "APPROVED",
    pr_diff: str = "diff --git a/f b/f\n",
):
    """Return a subprocess.run replacement that stubs gh CLI calls and delegates
    all other commands (e.g. git) to the real subprocess.run."""

    def fake_run(cmd, *args, **kwargs):
        prog = cmd[0] if cmd else ""
        if prog != "gh":
            # Let git and other real commands through unchanged
            return _real_subprocess_run(cmd, *args, **kwargs)

        sub = cmd[1] if len(cmd) > 1 else ""
        # gh issue view ... --json body --jq .body
        if sub == "issue" and "view" in cmd and "--jq" in cmd:
            return subprocess.CompletedProcess(
                cmd, 0, stdout=issue_body + "\n", stderr=""
            )
        # gh issue view ... --json number,url,body  (for --plan issue-ref resolution)
        if sub == "issue" and "view" in cmd and "--json" in cmd:
            num = cmd[3] if len(cmd) > 3 else "42"
            data = json.dumps(
                {
                    "number": int(num),
                    "url": f"https://github.com/owner/repo/issues/{num}",
                    "body": issue_body,
                }
            )
            return subprocess.CompletedProcess(cmd, 0, stdout=data, stderr="")
        # gh pr edit (request-copilot)
        if sub == "pr" and "edit" in cmd:
            return subprocess.CompletedProcess(cmd, 0, stdout="", stderr="")
        # gh pr diff
        if sub == "pr" and "diff" in cmd:
            return subprocess.CompletedProcess(cmd, 0, stdout=pr_diff, stderr="")
        # gh api (wait-copilot)
        if sub == "api":
            return subprocess.CompletedProcess(
                cmd, 0, stdout=copilot_state + "\n", stderr=""
            )
        return subprocess.CompletedProcess(cmd, 0, stdout="", stderr="")

    return fake_run


# ---------------------------------------------------------------------------
# classify_impl_outcome — all four branches (pure git, real temp repo)
# ---------------------------------------------------------------------------


def _init_git_repo(path: pathlib.Path) -> None:
    subprocess.run(["git", "init"], cwd=path, check=True, capture_output=True)
    subprocess.run(
        ["git", "config", "user.email", "test@test.com"],
        cwd=path,
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "config", "user.name", "Test"],
        cwd=path,
        check=True,
        capture_output=True,
    )
    (path / "README.md").write_text("init\n")
    subprocess.run(
        ["git", "add", "README.md"], cwd=path, check=True, capture_output=True
    )
    subprocess.run(
        ["git", "commit", "-m", "init"], cwd=path, check=True, capture_output=True
    )


def test_classify_empty_impl(tmp_path):
    _init_git_repo(tmp_path)
    pre = record_pre_impl_state(cwd=str(tmp_path))
    outcome = classify_impl_outcome(pre, cwd=str(tmp_path))
    assert isinstance(outcome, EmptyImpl)


def test_classify_dirty_only(tmp_path):
    _init_git_repo(tmp_path)
    pre = record_pre_impl_state(cwd=str(tmp_path))
    (tmp_path / "new.txt").write_text("dirty\n")
    outcome = classify_impl_outcome(pre, cwd=str(tmp_path))
    assert isinstance(outcome, DirtyOnly)


def test_classify_head_advanced(tmp_path):
    _init_git_repo(tmp_path)
    pre = record_pre_impl_state(cwd=str(tmp_path))
    (tmp_path / "feat.txt").write_text("feature\n")
    subprocess.run(
        ["git", "add", "feat.txt"], cwd=tmp_path, check=True, capture_output=True
    )
    subprocess.run(
        ["git", "commit", "-m", "feat"], cwd=tmp_path, check=True, capture_output=True
    )
    outcome = classify_impl_outcome(pre, cwd=str(tmp_path))
    assert isinstance(outcome, HeadAdvanced)
    assert outcome.commit_count == 1


def test_classify_divergent_head(tmp_path):
    _init_git_repo(tmp_path)
    pre = record_pre_impl_state(cwd=str(tmp_path))

    # Create an orphan branch (diverges from the init commit)
    subprocess.run(
        ["git", "checkout", "--orphan", "orphan"],
        cwd=tmp_path,
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "rm", "-rf", "."], cwd=tmp_path, check=True, capture_output=True
    )
    (tmp_path / "orphan.txt").write_text("orphan\n")
    subprocess.run(
        ["git", "add", "orphan.txt"], cwd=tmp_path, check=True, capture_output=True
    )
    subprocess.run(
        ["git", "commit", "-m", "orphan commit"],
        cwd=tmp_path,
        check=True,
        capture_output=True,
    )

    outcome = classify_impl_outcome(pre, cwd=str(tmp_path))
    assert isinstance(outcome, DivergentHead)


# ---------------------------------------------------------------------------
# impl-handoff branch lifecycle (real temp git repo)
# ---------------------------------------------------------------------------


def test_handoff_branch_lifecycle(tmp_path):
    """create_handoff_branch creates a branch at current HEAD; sweep_stale removes merged ones."""
    _init_git_repo(tmp_path)

    # Make an implementation commit
    (tmp_path / "impl.txt").write_text("work\n")
    subprocess.run(
        ["git", "add", "impl.txt"], cwd=tmp_path, check=True, capture_output=True
    )
    subprocess.run(
        ["git", "commit", "-m", "impl"], cwd=tmp_path, check=True, capture_output=True
    )

    pre = PreImplState(
        head=subprocess.run(
            ["git", "rev-parse", "HEAD~1"],
            cwd=tmp_path,
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip(),
        branch="",
    )

    handoff = create_handoff_branch(pre, cwd=str(tmp_path))
    assert handoff.startswith("ghgremlin-impl-handoff-")

    # Verify we're on the handoff branch
    current = subprocess.run(
        ["git", "symbolic-ref", "--short", "HEAD"],
        cwd=tmp_path,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    assert current == handoff

    # Create a "stale" handoff branch pointing to the same HEAD (simulating prior run)
    stale_branch = "ghgremlin-impl-handoff-9999"
    subprocess.run(
        ["git", "branch", stale_branch],
        cwd=tmp_path,
        check=True,
        capture_output=True,
    )

    # sweep_stale should delete the merged stale branch
    sweep_stale_handoff_branches(handoff, cwd=str(tmp_path))

    refs = subprocess.run(
        [
            "git",
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads/ghgremlin-impl-handoff-*",
        ],
        cwd=tmp_path,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.splitlines()
    # Stale merged branch should be gone; current handoff should still exist
    assert handoff in refs
    assert stale_branch not in refs


# ---------------------------------------------------------------------------
# _parse_gh_args — arg parsing unit tests
# ---------------------------------------------------------------------------


def test_parse_instructions():
    args = _parse_gh_args(["add a login page"])
    # A single quoted string arrives as one element in argv
    assert args.instructions == ["add a login page"]
    assert args.plan_source is None
    assert args.resume_from is None
    assert args.model is None
    assert args.ref == ""


def test_parse_plan_source():
    args = _parse_gh_args(["--plan", "42"])
    assert args.plan_source == "42"
    assert args.instructions == []


def test_parse_model():
    args = _parse_gh_args(["--model", "claude-opus-4-7", "do stuff"])
    assert args.model == "claude-opus-4-7"


def test_parse_resume_from_commit_pr(capsys):
    args = _parse_gh_args(["--plan", "42", "--resume-from", "commit-pr"])
    assert args.resume_from == "commit-pr"
    captured = capsys.readouterr()
    assert "rewinding" not in captured.err


def test_parse_plan_and_instructions_mutual_exclusion():
    with pytest.raises(SystemExit):
        _parse_gh_args(["--plan", "42", "also some instructions"])


# ---------------------------------------------------------------------------
# _parse_issue_ref unit tests
# ---------------------------------------------------------------------------


def test_parse_issue_ref_numeric():
    repo, ref = _parse_issue_ref("42", "owner/repo")
    assert repo == "owner/repo"
    assert ref == "42"


def test_parse_issue_ref_hash_prefix():
    repo, ref = _parse_issue_ref("#42", "owner/repo")
    assert repo == "owner/repo"
    assert ref == "42"


def test_parse_issue_ref_cross_repo():
    repo, ref = _parse_issue_ref("other/repo#7", "owner/repo")
    assert repo == "other/repo"
    assert ref == "7"


def test_parse_issue_ref_full_url():
    repo, ref = _parse_issue_ref(
        "https://github.com/owner/repo/issues/123", "owner/repo"
    )
    assert repo == "owner/repo"
    assert ref == "123"


def test_parse_issue_ref_invalid():
    repo, ref = _parse_issue_ref("not-a-ref", "owner/repo")
    assert repo is None
    assert ref is None


# ---------------------------------------------------------------------------
# gh_main — smoke test: --plan issue-ref mode (plan stage skipped)
# ---------------------------------------------------------------------------


class _CommittingClient(FakeClaudeClient):
    """FakeClaudeClient that creates a git commit when the implement label runs."""

    def __init__(self, *args, git_dir: pathlib.Path = None, **kwargs):
        super().__init__(*args, **kwargs)
        self._git_dir = git_dir

    def run(self, prompt, *, label, **kwargs):
        if label == "implement" and self._git_dir is not None:
            # Simulate implement creating a commit
            (self._git_dir / "impl.txt").write_text("impl\n")
            subprocess.run(
                ["git", "add", "impl.txt"],
                cwd=self._git_dir,
                check=True,
                capture_output=True,
            )
            subprocess.run(
                ["git", "commit", "-m", "impl: add impl.txt"],
                cwd=self._git_dir,
                check=True,
                capture_output=True,
            )
        return super().run(prompt, label=label, **kwargs)


def test_plan_mode_skips_plan_stage(tmp_path, monkeypatch):
    """--plan <issue-ref> resolves issue body without running the plan stage."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    session_dir, state_file = _patch_common(monkeypatch, tmp_path)

    monkeypatch.setattr(
        subprocess,
        "run",
        _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n"),
    )

    monkeypatch.setattr(
        "gremlins.stages.ghreview.run",
        lambda ctx, options: None,
    )
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.run",
        lambda ctx, options: "APPROVED",
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.run",
        lambda ctx, options: None,
    )
    monkeypatch.setattr(
        "gremlins.stages.ghaddress.run",
        lambda ctx, options: None,
    )
    monkeypatch.setattr("gremlins.stages.verify.run", lambda ctx, options: None)
    monkeypatch.setattr("gremlins.stages.wait_ci.run", lambda ctx, options: None)

    client = _CommittingClient(
        git_dir=tmp_path,
        fixtures={
            "implement": IMPL_EVENTS,
            "commit-pr": _pr_events(),
        },
    )

    result = gh_main(["--plan", "42"], client=client)
    assert result == 0

    labels = [c.label for c in client.calls]
    # plan stage must NOT have been called
    assert "plan" not in labels
    assert "implement" in labels
    assert "commit-pr" in labels


def test_plan_stage_uses_bundled_prompt_not_slash_command(tmp_path, monkeypatch):
    """Plan stage builds a real prompt from the bundled ghplan.md, not /ghplan."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    session_dir, state_file = _patch_common(monkeypatch, tmp_path)

    # Restore the real load_prompts (patch_common replaces it with a stub).
    from gremlins.prompts import load_prompts as _real_load_prompts

    monkeypatch.setattr(
        "gremlins.orchestrators.gh.load_prompts",
        _real_load_prompts,
    )

    monkeypatch.setattr(
        subprocess,
        "run",
        _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n"),
    )
    monkeypatch.setattr("gremlins.stages.ghreview.run", lambda ctx, options: None)
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.run", lambda ctx, options: "APPROVED"
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.run",
        lambda ctx, options: None,
    )
    monkeypatch.setattr("gremlins.stages.ghaddress.run", lambda ctx, options: None)
    monkeypatch.setattr("gremlins.stages.verify.run", lambda ctx, options: None)
    monkeypatch.setattr("gremlins.stages.wait_ci.run", lambda ctx, options: None)

    client = _CommittingClient(
        git_dir=tmp_path,
        fixtures={
            "plan": _issue_events(),
            "implement": IMPL_EVENTS,
            "commit-pr": _pr_events(),
        },
    )

    result = gh_main(["add foo feature"], client=client)
    assert result == 0

    plan_call = next(c for c in client.calls if c.label == "plan")
    assert "add foo feature" in plan_call.prompt
    assert not plan_call.prompt.startswith("/ghplan")
    assert "/ghplan" not in plan_call.prompt


def test_model_forwarded_to_all_stages(tmp_path, monkeypatch):
    """--model is forwarded to every client.run call."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    session_dir, state_file = _patch_common(monkeypatch, tmp_path)

    monkeypatch.setattr(
        subprocess,
        "run",
        _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n"),
    )
    monkeypatch.setattr("gremlins.stages.ghreview.run", lambda ctx, options: None)
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.run", lambda ctx, options: "APPROVED"
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.run",
        lambda ctx, options: None,
    )
    monkeypatch.setattr("gremlins.stages.ghaddress.run", lambda ctx, options: None)
    monkeypatch.setattr("gremlins.stages.verify.run", lambda ctx, options: None)
    monkeypatch.setattr("gremlins.stages.wait_ci.run", lambda ctx, options: None)

    client = _CommittingClient(
        git_dir=tmp_path,
        fixtures={
            "implement": IMPL_EVENTS,
            "commit-pr": _pr_events(),
        },
    )

    result = gh_main(["--plan", "42", "--model", "claude-opus-4-7"], client=client)
    assert result == 0

    for call in client.calls:
        assert call.model == "claude-opus-4-7", (
            f"stage {call.label!r} got model={call.model!r}"
        )


def test_gh_main_defaults_model_to_sonnet(tmp_path, monkeypatch):
    """Regression: ghgremlin must default --model to sonnet, not fall through
    to claude's runtime default (which inherits the calling session's model
    and silently runs every stage on opus when launched from an opus session).
    """
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    session_dir, state_file = _patch_common(monkeypatch, tmp_path)

    monkeypatch.setattr(
        subprocess,
        "run",
        _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n"),
    )
    monkeypatch.setattr("gremlins.stages.ghreview.run", lambda ctx, options: None)
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.run", lambda ctx, options: "APPROVED"
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.run",
        lambda ctx, options: None,
    )
    monkeypatch.setattr("gremlins.stages.ghaddress.run", lambda ctx, options: None)
    monkeypatch.setattr("gremlins.stages.verify.run", lambda ctx, options: None)
    monkeypatch.setattr("gremlins.stages.wait_ci.run", lambda ctx, options: None)

    client = _CommittingClient(
        git_dir=tmp_path,
        fixtures={
            "implement": IMPL_EVENTS,
            "commit-pr": _pr_events(),
        },
    )

    # Invoke with NO --model.
    result = gh_main(["--plan", "42"], client=client)
    assert result == 0

    # Every recorded run must have model == "sonnet". Asserting on every call
    # (not just calls[0]) catches the case where one stage is fixed but
    # another is overlooked.
    assert client.calls, "expected at least one client call"
    bad = [c for c in client.calls if c.model != "sonnet"]
    assert not bad, (
        f"{len(bad)} stage(s) ran on a non-sonnet model: "
        f"{[(c.label, c.model) for c in bad]}"
    )


def test_gh_main_resume_prefers_persisted_model_over_sonnet_default(
    tmp_path, monkeypatch
):
    """Regression: on resume with no --model, a persisted state.json.model must
    win over the fresh-launch "sonnet" fallback. Locks in the other half of the
    invariant called out in gh.py — a future refactor that switched argparse to
    default="sonnet" would silently break this precedence and only the
    fresh-launch test would still pass.
    """
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    state_data = {
        "issue_url": "https://github.com/owner/repo/issues/99",
        "issue_num": "99",
        "model": "claude-opus-4-7",
    }
    session_dir, state_file = _patch_common(
        monkeypatch, tmp_path, state_data=state_data
    )

    monkeypatch.setattr(
        subprocess,
        "run",
        _make_gh_subprocess(issue_body="# Resumed Plan\nDo more stuff.\n"),
    )
    monkeypatch.setattr("gremlins.stages.ghreview.run", lambda ctx, options: None)
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.run", lambda ctx, options: "APPROVED"
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.run",
        lambda ctx, options: None,
    )
    monkeypatch.setattr("gremlins.stages.ghaddress.run", lambda ctx, options: None)
    monkeypatch.setattr("gremlins.stages.verify.run", lambda ctx, options: None)
    monkeypatch.setattr("gremlins.stages.wait_ci.run", lambda ctx, options: None)

    client = _CommittingClient(
        git_dir=tmp_path,
        fixtures={
            "implement": IMPL_EVENTS,
            "commit-pr": _pr_events(),
        },
    )

    def _fake_read(sf, field):
        if field == "issue_url":
            return "https://github.com/owner/repo/issues/99"
        if field == "issue_num":
            return "99"
        if field == "model":
            return "claude-opus-4-7"
        return ""

    monkeypatch.setattr(_gh_mod, "_read_state_field", _fake_read)
    monkeypatch.setattr(
        _gh_mod, "_fetch_issue_body", lambda num, repo: "# Resumed Plan\nDo stuff.\n"
    )

    # Invoke with NO --model — resume path should restore "claude-opus-4-7"
    # from state.json rather than falling through to the "sonnet" default.
    result = gh_main(["--plan", "99", "--resume-from", "implement"], client=client)
    assert result == 0

    assert client.calls, "expected at least one client call"
    bad = [c for c in client.calls if c.model != "claude-opus-4-7"]
    assert not bad, (
        f"{len(bad)} stage(s) ignored persisted state.json model: "
        f"{[(c.label, c.model) for c in bad]}"
    )


def test_resume_from_implement(tmp_path, monkeypatch):
    """--resume-from implement reloads issue_url from state.json and runs implement onward."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    state_data = {
        "issue_url": "https://github.com/owner/repo/issues/99",
        "issue_num": "99",
    }
    session_dir, state_file = _patch_common(
        monkeypatch, tmp_path, state_data=state_data
    )

    monkeypatch.setattr(
        subprocess,
        "run",
        _make_gh_subprocess(issue_body="# Resumed Plan\nDo more stuff.\n"),
    )
    monkeypatch.setattr("gremlins.stages.ghreview.run", lambda ctx, options: None)
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.run", lambda ctx, options: "APPROVED"
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.run",
        lambda ctx, options: None,
    )
    monkeypatch.setattr("gremlins.stages.ghaddress.run", lambda ctx, options: None)
    monkeypatch.setattr("gremlins.stages.verify.run", lambda ctx, options: None)
    monkeypatch.setattr("gremlins.stages.wait_ci.run", lambda ctx, options: None)

    client = _CommittingClient(
        git_dir=tmp_path,
        fixtures={
            "implement": IMPL_EVENTS,
            "commit-pr": _pr_events(),
        },
    )

    # Simulate that the state.json has issue_url so _read_state_field can return it.
    # We need to reach into the patched resolve_state_file to make the pre-loop read work.
    def _fake_read(sf, field):
        if field == "issue_url":
            return "https://github.com/owner/repo/issues/99"
        if field == "issue_num":
            return "99"
        return ""

    monkeypatch.setattr(_gh_mod, "_read_state_field", _fake_read)
    monkeypatch.setattr(
        _gh_mod, "_fetch_issue_body", lambda num, repo: "# Resumed Plan\nDo stuff.\n"
    )

    result = gh_main(["--plan", "99", "--resume-from", "implement"], client=client)
    assert result == 0

    labels = [c.label for c in client.calls]
    assert "plan" not in labels
    assert "implement" in labels


def test_resume_from_ghreview(tmp_path, monkeypatch):
    """--resume-from ghreview reloads pr_url from state.json and skips earlier stages."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    session_dir, state_file = _patch_common(monkeypatch, tmp_path)

    def _fake_read(sf, field):
        if field == "issue_url":
            return "https://github.com/owner/repo/issues/5"
        if field == "issue_num":
            return "5"
        if field == "pr_url":
            return "https://github.com/owner/repo/pull/200"
        if field == "model":
            return ""
        return ""

    monkeypatch.setattr(_gh_mod, "_read_state_field", _fake_read)
    monkeypatch.setattr(
        _gh_mod, "_fetch_issue_body", lambda num, repo: "# Plan\nContent.\n"
    )

    ghreview_called = []
    monkeypatch.setattr(
        "gremlins.stages.ghreview.run",
        lambda ctx, options: ghreview_called.append(options.pr_url),
    )
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.run", lambda ctx, options: "APPROVED"
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.run",
        lambda ctx, options: None,
    )
    monkeypatch.setattr("gremlins.stages.ghaddress.run", lambda ctx, options: None)
    monkeypatch.setattr("gremlins.stages.wait_ci.run", lambda ctx, options: None)
    monkeypatch.setattr(subprocess, "run", _make_gh_subprocess())

    client = FakeClaudeClient(fixtures={})

    result = gh_main(["--plan", "5", "--resume-from", "ghreview"], client=client)
    assert result == 0

    # No client.run calls (plan/implement/commit-pr all skipped)
    assert client.calls == []
    # ghreview was called with the correct PR URL
    assert ghreview_called == ["https://github.com/owner/repo/pull/200"]


def test_plan_file_path_includes_plan_title_cost_in_total(tmp_path, monkeypatch):
    """gh_main with --plan <file> aggregates plan-title's cost into the persisted total_cost_usd.

    Regression guard for #157 (missing plan-title cost) and #164 (plan-title
    moved to stream-json mode for cost capture). Reads total_cost_usd from the
    on-disk state.json to verify the persistence step at gh.py:471-473.
    """
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    plan_file = tmp_path / "my-plan.md"
    plan_file.write_text("# Feature\nDo the thing.\n")

    session_dir, state_file = _patch_common(monkeypatch, tmp_path)

    # Override patch_state so it actually writes fields to state_file instead of no-op.
    def writing_patch_state(gr_id=None, _delete=(), **kw):
        data = json.loads(state_file.read_text())
        for key in _delete:
            data.pop(key, None)
        data.update(kw)
        state_file.write_text(json.dumps(data))

    monkeypatch.setattr("gremlins.orchestrators.gh.patch_state", writing_patch_state)

    def fake_gh_run(cmd, *args, **kwargs):
        prog = cmd[0] if cmd else ""
        if prog != "gh":
            return _real_subprocess_run(cmd, *args, **kwargs)
        sub = cmd[1] if len(cmd) > 1 else ""
        if sub == "issue" and "create" in cmd:
            return subprocess.CompletedProcess(
                cmd, 0, stdout="https://github.com/owner/repo/issues/42\n", stderr=""
            )
        return subprocess.CompletedProcess(cmd, 0, stdout="", stderr="")

    monkeypatch.setattr(subprocess, "run", fake_gh_run)

    monkeypatch.setattr("gremlins.stages.ghreview.run", lambda ctx, options: None)
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.run", lambda ctx, options: "APPROVED"
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.run",
        lambda ctx, options: None,
    )
    monkeypatch.setattr("gremlins.stages.ghaddress.run", lambda ctx, options: None)
    monkeypatch.setattr("gremlins.stages.verify.run", lambda ctx, options: None)
    monkeypatch.setattr("gremlins.stages.wait_ci.run", lambda ctx, options: None)

    # Each fixture carries a distinct non-zero cost so a regression that drops
    # any one stage shows up as the total being short by exactly that amount.
    fixtures = {
        "plan-title": [
            {"type": "system", "subtype": "init", "session_id": "session-title-1"},
            {
                "type": "result",
                "subtype": "success",
                "result": "Feature: Do the thing",
                "total_cost_usd": 0.13,
            },
        ],
        "implement": [
            {"type": "system", "subtype": "init", "session_id": "session-impl-1"},
            {"type": "result", "subtype": "success", "total_cost_usd": 0.07},
        ],
        "commit-pr": [
            {"type": "system", "subtype": "init", "session_id": "session-commit-1"},
            {
                "type": "assistant",
                "message": {
                    "content": [
                        {
                            "type": "tool_use",
                            "id": "tu-pr-1",
                            "name": "Bash",
                            "input": {"command": "gh pr create --base main"},
                        }
                    ]
                },
            },
            {
                "type": "user",
                "message": {
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "tu-pr-1",
                            "content": "https://github.com/owner/repo/pull/101",
                        }
                    ]
                },
            },
            {"type": "result", "subtype": "success", "total_cost_usd": 0.05},
        ],
    }

    # text_results provides a fallback for the plan-title label so that if
    # production regresses to output_format != "stream-json", the pipeline
    # still completes rather than raising KeyError.  The cost assertion then
    # fails for the right reason: total_cost_usd is short by exactly 0.13
    # because the text-mode path never accumulates plan-title's fixture cost.
    client = _CommittingClient(
        git_dir=tmp_path,
        fixtures=fixtures,
        text_results={"plan-title": "Feature: Do the thing"},
    )
    result = gh_main(["--plan", str(plan_file)], client=client)
    assert result == 0

    labels = [c.label for c in client.calls]
    assert "plan-title" in labels
    assert "implement" in labels
    assert "commit-pr" in labels

    # Read on-disk state.json — verifies both the accumulation and the persistence step.
    state = json.loads(state_file.read_text())
    assert "total_cost_usd" in state, "total_cost_usd was not persisted to state.json"

    total = state["total_cost_usd"]
    expected = 0.13 + 0.07 + 0.05
    assert total == pytest.approx(expected), (
        f"expected total {expected:.2f}, got {total:.4f}; "
        f"a regression dropping plan-title cost (0.13) would show total ≈ {expected - 0.13:.2f}"
    )


# ---------------------------------------------------------------------------
# Regression: --resume-from commit-pr must not re-run implement
# ---------------------------------------------------------------------------


def test_code_style_forwarded_to_ghreview_and_ghaddress(tmp_path, monkeypatch):
    """code_style is threaded into ghreview and ghaddress options."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    session_dir, state_file = _patch_common(monkeypatch, tmp_path)

    monkeypatch.setattr(
        subprocess,
        "run",
        _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n"),
    )

    captured = {}

    def record_ghreview(ctx, options):
        captured["ghreview"] = options

    def record_ghaddress(ctx, options):
        captured["ghaddress"] = options

    monkeypatch.setattr("gremlins.stages.ghreview.run", record_ghreview)
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.run", lambda ctx, options: "APPROVED"
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.run",
        lambda ctx, options: None,
    )
    monkeypatch.setattr("gremlins.stages.ghaddress.run", record_ghaddress)
    monkeypatch.setattr("gremlins.stages.verify.run", lambda ctx, options: None)
    monkeypatch.setattr("gremlins.stages.wait_ci.run", lambda ctx, options: None)

    client = _CommittingClient(
        git_dir=tmp_path,
        fixtures={
            "implement": IMPL_EVENTS,
            "commit-pr": _pr_events(),
        },
    )

    result = gh_main(["--plan", "42"], client=client)
    assert result == 0

    assert captured["ghreview"].code_style == "Be good."
    assert captured["ghaddress"].code_style == "Be good."


def test_resume_from_commit_pr_skips_implement(tmp_path, monkeypatch):
    """--resume-from commit-pr picks up at commit-pr without re-running implement.

    Regression guard for the bug where the orchestrator silently rewound
    commit-pr → implement, which caused an EmptyImpl loop when the
    impl-handoff branch was already present.
    """
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    # Simulate a completed implement stage: one commit above the init commit.
    (tmp_path / "impl.txt").write_text("impl content\n")
    subprocess.run(
        ["git", "add", "impl.txt"], cwd=tmp_path, check=True, capture_output=True
    )
    subprocess.run(
        ["git", "commit", "-m", "feat: add impl.txt"],
        cwd=tmp_path,
        check=True,
        capture_output=True,
    )

    base_ref = subprocess.run(
        ["git", "rev-parse", "HEAD~1"],
        cwd=tmp_path,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()

    # Create the impl-handoff branch at the impl commit.
    handoff_branch = "ghgremlin-impl-handoff-9999"
    subprocess.run(
        ["git", "branch", handoff_branch], cwd=tmp_path, check=True, capture_output=True
    )

    session_dir, state_file = _patch_common(monkeypatch, tmp_path)

    def _fake_read(sf, field):
        if field == "issue_url":
            return "https://github.com/owner/repo/issues/42"
        if field == "issue_num":
            return "42"
        if field == "pr_url":
            return ""
        if field == "impl_handoff_branch":
            return handoff_branch
        if field == "impl_base_ref":
            return base_ref
        return ""

    monkeypatch.setattr(_gh_mod, "_read_state_field", _fake_read)
    monkeypatch.setattr(
        _gh_mod, "_fetch_issue_body", lambda num, repo: "# Plan\nDo stuff.\n"
    )
    monkeypatch.setattr(subprocess, "run", _make_gh_subprocess())
    monkeypatch.setattr("gremlins.stages.ghreview.run", lambda ctx, options: None)
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.run", lambda ctx, options: "APPROVED"
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.run",
        lambda ctx, options: None,
    )
    monkeypatch.setattr("gremlins.stages.ghaddress.run", lambda ctx, options: None)
    monkeypatch.setattr("gremlins.stages.wait_ci.run", lambda ctx, options: None)

    client = FakeClaudeClient(fixtures={"commit-pr": _pr_events()})

    result = gh_main(["--plan", "42", "--resume-from", "commit-pr"], client=client)
    assert result == 0

    labels = [c.label for c in client.calls]
    assert "implement" not in labels, "implement must not run on commit-pr resume"
    assert "commit-pr" in labels

    # The commit-pr prompt must contain content from the impl diff.
    commit_pr_call = next(c for c in client.calls if c.label == "commit-pr")
    assert (
        "impl content" in commit_pr_call.prompt or "impl.txt" in commit_pr_call.prompt
    )

    # No resume_session: commit-pr must open a fresh session.
    assert commit_pr_call.resume_session is None


# ---------------------------------------------------------------------------
# ci-gate stage: argument wiring, ordering, and resume behavior
# ---------------------------------------------------------------------------


def test_wait_ci_stage_argument_wiring(tmp_path, monkeypatch):
    """wait_ci.run receives pr_url, model, code_style via options, session_dir via ctx."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    session_dir, _state_file = _patch_common(monkeypatch, tmp_path)

    monkeypatch.setattr(
        subprocess,
        "run",
        _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n"),
    )
    monkeypatch.setattr("gremlins.stages.ghreview.run", lambda ctx, options: None)
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.run", lambda ctx, options: "APPROVED"
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.run",
        lambda ctx, options: None,
    )
    monkeypatch.setattr("gremlins.stages.ghaddress.run", lambda ctx, options: None)
    monkeypatch.setattr("gremlins.stages.verify.run", lambda ctx, options: None)

    captured_ctx = {}
    captured_options = {}

    def record_wait_ci(ctx, options):
        captured_ctx["ctx"] = ctx
        captured_options["options"] = options

    monkeypatch.setattr("gremlins.stages.wait_ci.run", record_wait_ci)

    client = _CommittingClient(
        git_dir=tmp_path,
        fixtures={
            "implement": IMPL_EVENTS,
            "commit-pr": _pr_events("https://github.com/owner/repo/pull/77"),
        },
    )

    result = gh_main(["--plan", "42", "--model", "claude-opus-4-7"], client=client)
    assert result == 0

    opts = captured_options["options"]
    ctx = captured_ctx["ctx"]
    assert opts.pr_url == "https://github.com/owner/repo/pull/77"
    assert opts.model == "claude-opus-4-7"
    assert opts.code_style == "Be good."
    assert ctx.session_dir == session_dir


def test_wait_ci_stage_ordering(tmp_path, monkeypatch):
    """ci-gate runs after ghaddress and exactly once."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    _session_dir, _state_file = _patch_common(monkeypatch, tmp_path)

    monkeypatch.setattr(
        subprocess,
        "run",
        _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n"),
    )

    order: list[str] = []

    monkeypatch.setattr(
        "gremlins.stages.verify.run",
        lambda ctx, options: order.append("verify"),
    )
    monkeypatch.setattr(
        "gremlins.stages.ghreview.run",
        lambda ctx, options: order.append("ghreview"),
    )
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.run",
        lambda ctx, options: order.append("wait-copilot") or "APPROVED",
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.run",
        lambda ctx, options: order.append("request-copilot"),
    )
    monkeypatch.setattr(
        "gremlins.stages.ghaddress.run",
        lambda ctx, options: order.append("ghaddress"),
    )
    monkeypatch.setattr(
        "gremlins.stages.wait_ci.run",
        lambda ctx, options: order.append("ci-gate"),
    )

    client = _CommittingClient(
        git_dir=tmp_path,
        fixtures={
            "implement": IMPL_EVENTS,
            "commit-pr": _pr_events(),
        },
    )

    result = gh_main(["--plan", "42"], client=client)
    assert result == 0

    assert order[0] == "verify", "verify must run before other tracked stages"
    assert order[-2:] == ["ghaddress", "ci-gate"]
    assert order.count("verify") == 1
    assert order.count("ci-gate") == 1


def test_resume_from_ci_gate(tmp_path, monkeypatch):
    """--resume-from ci-gate skips all earlier stages and calls only wait_ci.run."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    _session_dir, _state_file = _patch_common(monkeypatch, tmp_path)

    def _fake_read(sf, field):
        if field == "issue_url":
            return "https://github.com/owner/repo/issues/5"
        if field == "issue_num":
            return "5"
        if field == "pr_url":
            return "https://github.com/owner/repo/pull/200"
        if field == "model":
            return ""
        return ""

    monkeypatch.setattr(_gh_mod, "_read_state_field", _fake_read)
    monkeypatch.setattr(
        _gh_mod, "_fetch_issue_body", lambda num, repo: "# Plan\nContent.\n"
    )

    earlier_called: list[str] = []
    ci_calls = []

    monkeypatch.setattr(
        "gremlins.stages.ghreview.run",
        lambda ctx, options: earlier_called.append("ghreview"),
    )
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.run",
        lambda ctx, options: earlier_called.append("wait-copilot") or "APPROVED",
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.run",
        lambda ctx, options: earlier_called.append("request-copilot"),
    )
    monkeypatch.setattr(
        "gremlins.stages.ghaddress.run",
        lambda ctx, options: earlier_called.append("ghaddress"),
    )
    monkeypatch.setattr(
        "gremlins.stages.wait_ci.run",
        lambda ctx, options: ci_calls.append(options),
    )
    monkeypatch.setattr(subprocess, "run", _make_gh_subprocess())

    client = FakeClaudeClient(fixtures={})

    result = gh_main(["--plan", "5", "--resume-from", "ci-gate"], client=client)
    assert result == 0

    assert client.calls == [], "no client stages should run on ci-gate resume"
    assert earlier_called == [], "earlier stages must be skipped"
    assert len(ci_calls) == 1
    assert ci_calls[0].pr_url == "https://github.com/owner/repo/pull/200"


# ---------------------------------------------------------------------------
# verify stage: argument wiring and resume behavior
# ---------------------------------------------------------------------------


def test_verify_stage_argument_wiring(tmp_path, monkeypatch):
    """verify.run receives fix_model, cwd, code_style via options, session_dir via ctx."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    session_dir, _state_file = _patch_common(monkeypatch, tmp_path)

    monkeypatch.setattr(
        subprocess,
        "run",
        _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n"),
    )
    monkeypatch.setattr("gremlins.stages.ghreview.run", lambda ctx, options: None)
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.run", lambda ctx, options: "APPROVED"
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.run",
        lambda ctx, options: None,
    )
    monkeypatch.setattr("gremlins.stages.ghaddress.run", lambda ctx, options: None)
    monkeypatch.setattr("gremlins.stages.wait_ci.run", lambda ctx, options: None)

    captured_ctx = {}
    captured_options = {}

    def record_verify(ctx, options):
        captured_ctx["ctx"] = ctx
        captured_options["options"] = options

    monkeypatch.setattr("gremlins.stages.verify.run", record_verify)

    client = _CommittingClient(
        git_dir=tmp_path,
        fixtures={
            "implement": IMPL_EVENTS,
            "commit-pr": _pr_events("https://github.com/owner/repo/pull/77"),
        },
    )

    result = gh_main(["--plan", "42", "--model", "claude-opus-4-7"], client=client)
    assert result == 0

    opts = captured_options["options"]
    ctx = captured_ctx["ctx"]
    assert opts.fix_model == "claude-opus-4-7"
    assert opts.code_style == "Be good."
    assert ctx.session_dir == session_dir


def test_resume_from_verify(tmp_path, monkeypatch):
    """--resume-from verify skips plan and implement, runs verify onward."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    # Simulate a completed implement stage: one commit above init.
    (tmp_path / "impl.txt").write_text("impl content\n")
    subprocess.run(
        ["git", "add", "impl.txt"], cwd=tmp_path, check=True, capture_output=True
    )
    subprocess.run(
        ["git", "commit", "-m", "feat: add impl.txt"],
        cwd=tmp_path,
        check=True,
        capture_output=True,
    )
    base_ref = subprocess.run(
        ["git", "rev-parse", "HEAD~1"],
        cwd=tmp_path,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    handoff_branch = "ghgremlin-impl-handoff-verify-test"
    subprocess.run(
        ["git", "branch", handoff_branch],
        cwd=tmp_path,
        check=True,
        capture_output=True,
    )

    _session_dir, _state_file = _patch_common(monkeypatch, tmp_path)

    def _fake_read(sf, field):
        if field == "issue_url":
            return "https://github.com/owner/repo/issues/5"
        if field == "issue_num":
            return "5"
        if field == "pr_url":
            return ""
        if field == "impl_handoff_branch":
            return handoff_branch
        if field == "impl_base_ref":
            return base_ref
        if field == "model":
            return ""
        return ""

    monkeypatch.setattr(_gh_mod, "_read_state_field", _fake_read)
    monkeypatch.setattr(
        _gh_mod, "_fetch_issue_body", lambda num, repo: "# Plan\nContent.\n"
    )

    earlier_called: list[str] = []
    verify_calls = []

    monkeypatch.setattr(
        "gremlins.stages.verify.run",
        lambda ctx, options: verify_calls.append(options),
    )
    monkeypatch.setattr(
        "gremlins.stages.ghreview.run",
        lambda ctx, options: earlier_called.append("ghreview"),
    )
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.run",
        lambda ctx, options: "APPROVED",
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.run",
        lambda ctx, options: None,
    )
    monkeypatch.setattr("gremlins.stages.ghaddress.run", lambda ctx, options: None)
    monkeypatch.setattr("gremlins.stages.wait_ci.run", lambda ctx, options: None)
    monkeypatch.setattr(subprocess, "run", _make_gh_subprocess())

    client = FakeClaudeClient(fixtures={"commit-pr": _pr_events()})

    result = gh_main(["--plan", "5", "--resume-from", "verify"], client=client)
    assert result == 0

    labels = [c.label for c in client.calls]
    assert "implement" not in labels, "implement must not run on verify resume"
    assert len(verify_calls) == 1


def test_gh_main_writes_stage_to_state(tmp_path, monkeypatch):
    """gr_id threads into set_stage and writes to state.json under XDG_STATE_HOME."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    gr_id = "test-gr-id"
    xdg = tmp_path / "xdg"
    state_dir = xdg / "claude-gremlins" / gr_id
    state_dir.mkdir(parents=True)
    (state_dir / "state.json").write_text(json.dumps({"id": gr_id, "stage": ""}))
    monkeypatch.setenv("XDG_STATE_HOME", str(xdg))

    session_dir, _ = _patch_common(monkeypatch, tmp_path)

    monkeypatch.setattr(
        subprocess, "run", _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n")
    )
    monkeypatch.setattr("gremlins.stages.ghreview.run", lambda ctx, options: None)
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.run", lambda ctx, options: "APPROVED"
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.run", lambda ctx, options: None
    )
    monkeypatch.setattr("gremlins.stages.ghaddress.run", lambda ctx, options: None)
    monkeypatch.setattr("gremlins.stages.verify.run", lambda ctx, options: None)
    monkeypatch.setattr("gremlins.stages.wait_ci.run", lambda ctx, options: None)

    client = _CommittingClient(
        git_dir=tmp_path,
        fixtures={"implement": IMPL_EVENTS, "commit-pr": _pr_events()},
    )

    result = gh_main(["--plan", "42"], gr_id=gr_id, client=client)
    assert result == 0

    data = json.loads((state_dir / "state.json").read_text())
    assert data.get("stage")
