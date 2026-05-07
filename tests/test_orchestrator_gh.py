"""Tests for gremlins.orchestrators.gh and supporting git helpers.

Uses FakeClaudeClient throughout — no real claude subprocess or gh CLI calls
(gh calls are monkeypatched at the subprocess.run level).
"""

import dataclasses
import json
import pathlib
import shutil
import subprocess

import pytest

import gremlins.orchestrators.gh as _gh_mod
import gremlins.orchestrators.pipeline as _pipeline_mod
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
from gremlins.pipeline import load_pipeline, resolve_pipeline_path

# ---------------------------------------------------------------------------
# Helper: minimal stream-json event list containing a PR URL in a tool_result
# ---------------------------------------------------------------------------


def _issue_events(issue_url: str = "https://github.com/owner/repo/issues/42") -> list:
    return [
        {"type": "system", "subtype": "init"},
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
        {"type": "system", "subtype": "init"},
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
    {"type": "system", "subtype": "init"},
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
        "gremlins.orchestrators.pipeline.install_signal_handlers", lambda *c: None
    )
    monkeypatch.setattr(
        "gremlins.orchestrators.gh.install_signal_handlers", lambda *c: None
    )
    monkeypatch.setattr("gremlins.orchestrators.gh.get_repo", lambda: "owner/repo")

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
    monkeypatch.setattr(
        "gremlins.clients.resolve.resolve_state_file", lambda gr_id=None: state_file
    )

    # Stub out patch_state so tests don't write to real state files.
    monkeypatch.setattr(
        "gremlins.orchestrators.gh.patch_state", lambda gr_id=None, **kw: None
    )

    # Strip pipeline client keys so the injected client is used for every stage.
    _real_load_pipeline = _gh_mod.load_pipeline

    def _load_pipeline_no_clients(path):
        pipeline = _real_load_pipeline(path)
        stripped_stages = [dataclasses.replace(s, client=None) for s in pipeline.stages]
        return dataclasses.replace(
            pipeline, default_client=None, stages=stripped_stages
        )

    monkeypatch.setattr(
        "gremlins.orchestrators.gh.load_pipeline", _load_pipeline_no_clients
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
    assert args.ref == ""


def test_parse_plan_source():
    args = _parse_gh_args(["--plan", "42"])
    assert args.plan_source == "42"
    assert args.instructions == []


def test_parse_resume_from_commit(capsys):
    args = _parse_gh_args(["--plan", "42", "--resume-from", "commit"])
    assert args.resume_from == "commit"
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


def test_gh_pipeline_stage_names(tmp_path):
    pipeline = load_pipeline(resolve_pipeline_path("gh", tmp_path))
    names = [s.name for s in pipeline.stages]
    assert names == [
        "plan",
        "implement",
        "handoff-branch",
        "verify",
        "commit",
        "open-pr",
        "request-copilot",
        "ghreview",
        "wait-copilot",
        "ghaddress",
        "ci-gate",
    ]


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
        "gremlins.stages.ghreview.GHReview.run",
        lambda self, pipe: None,
    )
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.WaitCopilot.run",
        lambda self, pipe: "APPROVED",
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.RequestCopilot.run",
        lambda self, pipe: None,
    )
    monkeypatch.setattr(
        "gremlins.stages.ghaddress.GHAddress.run",
        lambda self, pipe: None,
    )
    monkeypatch.setattr("gremlins.stages.verify.Verify.run", lambda self, pipe: None)
    monkeypatch.setattr("gremlins.stages.wait_ci.WaitCI.run", lambda self, pipe: None)

    client = _CommittingClient(
        git_dir=tmp_path,
        fixtures={
            "implement": IMPL_EVENTS,
            "commit": IMPL_EVENTS,
            "open-github-pr": _pr_events(),
        },
    )

    result = gh_main(["--plan", "42"], client=client)
    assert result == 0

    labels = [c.label for c in client.calls]
    # plan stage must NOT have been called
    assert "plan" not in labels
    assert "implement" in labels
    assert "commit" in labels


def test_plan_stage_uses_bundled_prompt_not_slash_command(tmp_path, monkeypatch):
    """Plan stage builds a real prompt from the bundled ghplan.md, not /ghplan."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    session_dir, state_file = _patch_common(monkeypatch, tmp_path)

    monkeypatch.setattr(
        subprocess,
        "run",
        _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n"),
    )
    monkeypatch.setattr(
        "gremlins.stages.ghreview.GHReview.run", lambda self, pipe: None
    )
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.WaitCopilot.run", lambda self, pipe: "APPROVED"
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.RequestCopilot.run",
        lambda self, pipe: None,
    )
    monkeypatch.setattr(
        "gremlins.stages.ghaddress.GHAddress.run", lambda self, pipe: None
    )
    monkeypatch.setattr("gremlins.stages.verify.Verify.run", lambda self, pipe: None)
    monkeypatch.setattr("gremlins.stages.wait_ci.WaitCI.run", lambda self, pipe: None)

    client = _CommittingClient(
        git_dir=tmp_path,
        fixtures={
            "plan": _issue_events(),
            "implement": IMPL_EVENTS,
            "commit": IMPL_EVENTS,
            "open-github-pr": _pr_events(),
        },
    )

    result = gh_main(["add foo feature"], client=client)
    assert result == 0

    plan_call = next(c for c in client.calls if c.label == "plan")
    assert "add foo feature" in plan_call.prompt
    assert not plan_call.prompt.startswith("/ghplan")
    assert "/ghplan" not in plan_call.prompt


def test_model_forwarded_to_all_stages(tmp_path, monkeypatch):
    """--client provider:model is forwarded to every client.run call."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    session_dir, state_file = _patch_common(monkeypatch, tmp_path)

    monkeypatch.setattr(
        subprocess,
        "run",
        _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n"),
    )
    monkeypatch.setattr(
        "gremlins.stages.ghreview.GHReview.run", lambda self, pipe: None
    )
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.WaitCopilot.run", lambda self, pipe: "APPROVED"
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.RequestCopilot.run",
        lambda self, pipe: None,
    )
    monkeypatch.setattr(
        "gremlins.stages.ghaddress.GHAddress.run", lambda self, pipe: None
    )
    monkeypatch.setattr("gremlins.stages.verify.Verify.run", lambda self, pipe: None)
    monkeypatch.setattr("gremlins.stages.wait_ci.WaitCI.run", lambda self, pipe: None)

    client = _CommittingClient(
        git_dir=tmp_path,
        fixtures={
            "implement": IMPL_EVENTS,
            "commit": IMPL_EVENTS,
            "open-github-pr": _pr_events(),
        },
    )

    result = gh_main(
        ["--plan", "42", "--client", "claude:claude-opus-4-7"], client=client
    )
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
    monkeypatch.setattr(
        "gremlins.stages.ghreview.GHReview.run", lambda self, pipe: None
    )
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.WaitCopilot.run", lambda self, pipe: "APPROVED"
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.RequestCopilot.run",
        lambda self, pipe: None,
    )
    monkeypatch.setattr(
        "gremlins.stages.ghaddress.GHAddress.run", lambda self, pipe: None
    )
    monkeypatch.setattr("gremlins.stages.verify.Verify.run", lambda self, pipe: None)
    monkeypatch.setattr("gremlins.stages.wait_ci.WaitCI.run", lambda self, pipe: None)

    client = _CommittingClient(
        git_dir=tmp_path,
        fixtures={
            "implement": IMPL_EVENTS,
            "commit": IMPL_EVENTS,
            "open-github-pr": _pr_events(),
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


def test_gh_main_client_specifier_model(tmp_path, monkeypatch):
    """Model from --client provider:model flows into all stage run() calls."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    session_dir, state_file = _patch_common(monkeypatch, tmp_path)

    monkeypatch.setattr(
        subprocess,
        "run",
        _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n"),
    )
    monkeypatch.setattr(
        "gremlins.stages.ghreview.GHReview.run", lambda self, pipe: None
    )
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.WaitCopilot.run", lambda self, pipe: "APPROVED"
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.RequestCopilot.run",
        lambda self, pipe: None,
    )
    monkeypatch.setattr(
        "gremlins.stages.ghaddress.GHAddress.run", lambda self, pipe: None
    )
    monkeypatch.setattr("gremlins.stages.verify.Verify.run", lambda self, pipe: None)
    monkeypatch.setattr("gremlins.stages.wait_ci.WaitCI.run", lambda self, pipe: None)

    client = _CommittingClient(
        git_dir=tmp_path,
        fixtures={
            "implement": IMPL_EVENTS,
            "commit": IMPL_EVENTS,
            "open-github-pr": _pr_events(),
        },
    )

    result = gh_main(["--plan", "42", "--client", "copilot:gpt-4o"], client=client)
    assert result == 0

    assert client.calls, "expected at least one client call"
    bad = [c for c in client.calls if c.model != "gpt-4o"]
    assert not bad, (
        f"{len(bad)} stage(s) ran on a non-gpt-4o model: "
        f"{[(c.label, c.model) for c in bad]}"
    )


def test_gh_main_resume_prefers_persisted_stage_clients_over_edited_pipeline(
    tmp_path, monkeypatch
):
    """Resume must keep using persisted stage_clients after the pipeline changes."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    gr_id = "resume-test-gr-id"

    stage_defs = [
        ("plan", "plan"),
        ("implement", "implement"),
        ("handoff-branch", "handoff-branch"),
        ("verify", "verify"),
        ("commit", "commit"),
        ("open-pr", "open-github-pr"),
        ("request-copilot", "request-copilot"),
        ("ghreview", "ghreview"),
        ("wait-copilot", "wait-copilot"),
        ("ghaddress", "ghaddress"),
        ("ci-gate", "wait-ci"),
    ]
    original_stage_clients = {
        "plan": "claude:claude-sonnet-4-6",
        "implement": "claude:claude-haiku-4-5-20251001",
        "handoff-branch": "claude:claude-sonnet-4-6",
        "verify": "claude:claude-opus-4-1",
        "commit": "copilot:gpt-4o",
        "open-pr": "copilot:gpt-4o",
        "request-copilot": "claude:claude-sonnet-4-6",
        "ghreview": "claude:claude-haiku-4-5-20251001",
        "wait-copilot": "copilot:gpt-5",
        "ghaddress": "claude:claude-sonnet-4-6",
        "ci-gate": "claude:claude-opus-4-1",
    }
    mutated_stage_clients = {
        stage_name: "claude:claude-opus-4-7" for stage_name, _ in stage_defs
    }

    pipeline_dir = tmp_path / ".gremlins" / "pipelines"
    pipeline_dir.mkdir(parents=True)
    pipeline_path = pipeline_dir / "gh.yaml"
    prompt_dir = tmp_path / ".gremlins" / "prompts"
    prompt_dir.mkdir(parents=True)
    ghreview_prompt = prompt_dir / "ghreview.md"
    ghreview_prompt.write_text("Review.\n", encoding="utf-8")
    ghaddress_prompt = prompt_dir / "ghaddress.md"
    ghaddress_prompt.write_text("Address.\n", encoding="utf-8")
    implement_prompt = prompt_dir / "implement.md"
    implement_prompt.write_text("Implement.\n", encoding="utf-8")

    def write_pipeline(stage_clients: dict[str, str]) -> None:
        lines = ["name: gh", "prompt_dir: ../prompts", "", "stages:"]
        for stage_name, stage_type in stage_defs:
            fields = [
                f"name: {stage_name}",
                f"type: {stage_type}",
                f"client: {json.dumps(stage_clients[stage_name])}",
            ]
            if stage_type == "ghreview":
                fields.append(f"prompt: {json.dumps('ghreview.md')}")
            elif stage_type == "ghaddress":
                fields.append(f"prompt: {json.dumps('ghaddress.md')}")
            elif stage_type == "implement":
                fields.append(f"prompt: {json.dumps('implement.md')}")
            lines.append("  - { " + ", ".join(fields) + " }")
        pipeline_path.write_text("\n".join(lines) + "\n", encoding="utf-8")

    write_pipeline(original_stage_clients)

    _, state_file = _patch_common(
        monkeypatch,
        tmp_path,
        state_data={
            "issue_url": "https://github.com/owner/repo/issues/42",
            "issue_num": "42",
        },
    )
    monkeypatch.setattr("gremlins.orchestrators.gh.load_pipeline", load_pipeline)

    def writing_patch_state(gr_id=None, _delete=(), **kw):
        data = json.loads(state_file.read_text(encoding="utf-8"))
        for key in _delete:
            data.pop(key, None)
        data.update(kw)
        state_file.write_text(json.dumps(data), encoding="utf-8")

    monkeypatch.setattr("gremlins.orchestrators.gh.patch_state", writing_patch_state)

    monkeypatch.setattr(
        subprocess,
        "run",
        _make_gh_subprocess(issue_body="# Resumed Plan\nDo more stuff.\n"),
    )
    verify_models: list[str] = []
    ghreview_models: list[str] = []
    ghaddress_models: list[str] = []
    ci_models: list[str] = []
    monkeypatch.setattr(
        "gremlins.stages.ghreview.GHReview.run",
        lambda self, pipe: ghreview_models.append(self.model),
    )
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.WaitCopilot.run", lambda self, pipe: "APPROVED"
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.RequestCopilot.run",
        lambda self, pipe: None,
    )
    monkeypatch.setattr(
        "gremlins.stages.ghaddress.GHAddress.run",
        lambda self, pipe: ghaddress_models.append(self.model),
    )
    monkeypatch.setattr(
        "gremlins.stages.verify.Verify.run",
        lambda self, pipe: verify_models.append(self.model),
    )
    monkeypatch.setattr(
        "gremlins.stages.wait_ci.WaitCI.run",
        lambda self, pipe: ci_models.append(self.model),
    )

    launch_client = _CommittingClient(
        git_dir=tmp_path,
        fixtures={
            "implement": IMPL_EVENTS,
            "commit": IMPL_EVENTS,
            "open-github-pr": _pr_events(),
        },
    )

    result = gh_main(["--plan", "42"], client=launch_client, gr_id=gr_id)
    assert result == 0

    launch_state = json.loads(state_file.read_text(encoding="utf-8"))
    assert launch_state.get("stage_clients") == original_stage_clients
    assert verify_models == ["claude-opus-4-1"]
    assert ghreview_models == ["claude-haiku-4-5-20251001"]
    assert ghaddress_models == ["claude-sonnet-4-6"]
    assert ci_models == ["claude-opus-4-1"]

    write_pipeline(mutated_stage_clients)
    verify_models.clear()
    ghreview_models.clear()
    ghaddress_models.clear()
    ci_models.clear()
    subprocess.run(
        ["git", "switch", "main"],
        cwd=tmp_path,
        check=True,
        capture_output=True,
    )
    branch_list = subprocess.run(
        ["git", "branch", "--list", "ghgremlin-impl-handoff-*"],
        cwd=tmp_path,
        check=True,
        capture_output=True,
        text=True,
    )
    for branch in branch_list.stdout.splitlines():
        branch_name = branch.replace("*", "").strip()
        if not branch_name:
            continue
        subprocess.run(
            ["git", "branch", "-D", branch_name],
            cwd=tmp_path,
            check=True,
            capture_output=True,
        )
    (tmp_path / "impl.txt").write_text("resume seed\n", encoding="utf-8")
    subprocess.run(
        ["git", "add", "impl.txt"],
        cwd=tmp_path,
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "commit", "-m", "prep resume"],
        cwd=tmp_path,
        check=True,
        capture_output=True,
    )

    resume_client = _CommittingClient(
        git_dir=tmp_path,
        fixtures={
            "implement": IMPL_EVENTS,
            "commit": IMPL_EVENTS,
            "open-github-pr": _pr_events(),
        },
    )

    result = gh_main(
        ["--plan", "42", "--resume-from", "implement"],
        client=resume_client,
        gr_id=gr_id,
    )
    assert result == 0

    called_models = {call.label: call.model for call in resume_client.calls}
    assert called_models == {
        "implement": "claude-haiku-4-5-20251001",
        "commit": "gpt-4o",
        "open-github-pr": "gpt-4o",
    }
    assert verify_models == ["claude-opus-4-1"]
    assert ghreview_models == ["claude-haiku-4-5-20251001"]
    assert ghaddress_models == ["claude-sonnet-4-6"]
    assert ci_models == ["claude-opus-4-1"]


def test_gh_main_resume_requires_persisted_stage_clients(tmp_path, monkeypatch, capsys):
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    _patch_common(
        monkeypatch,
        tmp_path,
        state_data={
            "issue_url": "https://github.com/owner/repo/issues/42",
            "issue_num": "42",
        },
    )
    monkeypatch.setattr(subprocess, "run", _make_gh_subprocess())

    with pytest.raises(SystemExit):
        gh_main(
            ["--plan", "42", "--resume-from", "implement"],
            client=FakeClaudeClient(fixtures={}),
            gr_id="resume-test-gr-id",
        )

    assert "stage_clients not found" in capsys.readouterr().err


def test_gh_main_resume_requires_each_persisted_stage_client(
    tmp_path, monkeypatch, capsys
):
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    _, state_file = _patch_common(
        monkeypatch,
        tmp_path,
        state_data={
            "issue_url": "https://github.com/owner/repo/issues/42",
            "issue_num": "42",
            "stage_clients": {
                "plan": "claude:sonnet",
                "implement": "claude:sonnet",
                "handoff-branch": "claude:sonnet",
                "verify": "claude:sonnet",
                "commit": "claude:sonnet",
                "open-pr": "claude:sonnet",
                "request-copilot": "claude:sonnet",
                "wait-copilot": "claude:sonnet",
                "ghaddress": "claude:sonnet",
                "ci-gate": "claude:sonnet",
            },
        },
    )
    monkeypatch.setattr(subprocess, "run", _make_gh_subprocess())

    with pytest.raises(SystemExit):
        gh_main(
            ["--plan", "42", "--resume-from", "implement"],
            client=FakeClaudeClient(fixtures={}),
            gr_id="resume-test-gr-id",
        )

    assert "stage_clients missing stage: 'ghreview'" in capsys.readouterr().err


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
    monkeypatch.setattr(
        "gremlins.stages.ghreview.GHReview.run", lambda self, pipe: None
    )
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.WaitCopilot.run", lambda self, pipe: "APPROVED"
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.RequestCopilot.run",
        lambda self, pipe: None,
    )
    monkeypatch.setattr(
        "gremlins.stages.ghaddress.GHAddress.run", lambda self, pipe: None
    )
    monkeypatch.setattr("gremlins.stages.verify.Verify.run", lambda self, pipe: None)
    monkeypatch.setattr("gremlins.stages.wait_ci.WaitCI.run", lambda self, pipe: None)

    client = _CommittingClient(
        git_dir=tmp_path,
        fixtures={
            "implement": IMPL_EVENTS,
            "commit": IMPL_EVENTS,
            "open-github-pr": _pr_events(),
        },
    )

    # Simulate that the state.json has issue_url so read_state_field can return it.
    # We need to reach into the patched resolve_state_file to make the pre-loop read work.
    def _fake_read(sf, field):
        if field == "issue_url":
            return "https://github.com/owner/repo/issues/99"
        if field == "issue_num":
            return "99"
        return ""

    monkeypatch.setattr(_gh_mod, "read_state_field", _fake_read)
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

    monkeypatch.setattr(_gh_mod, "read_state_field", _fake_read)
    monkeypatch.setattr(_pipeline_mod, "read_state_field", _fake_read)
    monkeypatch.setattr(
        _gh_mod, "_fetch_issue_body", lambda num, repo: "# Plan\nContent.\n"
    )

    ghreview_called = []
    monkeypatch.setattr(
        "gremlins.stages.ghreview.GHReview.run",
        lambda self, pipe: ghreview_called.append(self.pr_url),
    )
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.WaitCopilot.run", lambda self, pipe: "APPROVED"
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.RequestCopilot.run",
        lambda self, pipe: None,
    )
    monkeypatch.setattr(
        "gremlins.stages.ghaddress.GHAddress.run", lambda self, pipe: None
    )
    monkeypatch.setattr("gremlins.stages.wait_ci.WaitCI.run", lambda self, pipe: None)
    monkeypatch.setattr(subprocess, "run", _make_gh_subprocess())

    client = FakeClaudeClient(fixtures={})

    result = gh_main(["--plan", "5", "--resume-from", "ghreview"], client=client)
    assert result == 0

    # No client.run calls (plan/implement/commit/open-pr all skipped)
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

    monkeypatch.setattr(
        "gremlins.stages.ghreview.GHReview.run", lambda self, pipe: None
    )
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.WaitCopilot.run", lambda self, pipe: "APPROVED"
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.RequestCopilot.run",
        lambda self, pipe: None,
    )
    monkeypatch.setattr(
        "gremlins.stages.ghaddress.GHAddress.run", lambda self, pipe: None
    )
    monkeypatch.setattr("gremlins.stages.verify.Verify.run", lambda self, pipe: None)
    monkeypatch.setattr("gremlins.stages.wait_ci.WaitCI.run", lambda self, pipe: None)

    # Each fixture carries a distinct non-zero cost so a regression that drops
    # any one stage shows up as the total being short by exactly that amount.
    fixtures = {
        "plan-title": [
            {"type": "system", "subtype": "init"},
            {
                "type": "result",
                "subtype": "success",
                "result": "Feature: Do the thing",
                "total_cost_usd": 0.13,
            },
        ],
        "implement": [
            {"type": "system", "subtype": "init"},
            {"type": "result", "subtype": "success", "total_cost_usd": 0.07},
        ],
        "commit": [
            {"type": "system", "subtype": "init"},
            {"type": "result", "subtype": "success", "total_cost_usd": 0.03},
        ],
        "open-github-pr": [
            {"type": "system", "subtype": "init"},
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
            {"type": "result", "subtype": "success", "total_cost_usd": 0.02},
        ],
    }

    client = _CommittingClient(
        git_dir=tmp_path,
        fixtures=fixtures,
    )
    result = gh_main(["--plan", str(plan_file)], client=client)
    assert result == 0

    labels = [c.label for c in client.calls]
    assert "plan-title" in labels
    assert "implement" in labels
    assert "commit" in labels
    assert "open-github-pr" in labels

    # Read on-disk state.json — verifies both the accumulation and the persistence step.
    state = json.loads(state_file.read_text())
    assert "total_cost_usd" in state, "total_cost_usd was not persisted to state.json"

    total = state["total_cost_usd"]
    expected = 0.13 + 0.07 + 0.03 + 0.02
    assert total == pytest.approx(expected), (
        f"expected total {expected:.2f}, got {total:.4f}; "
        f"a regression dropping plan-title cost (0.13) would show total ≈ {expected - 0.13:.2f}"
    )


# ---------------------------------------------------------------------------
# Regression: --resume-from commit must not re-run implement
# ---------------------------------------------------------------------------


def test_resume_from_commit_skips_implement(tmp_path, monkeypatch):
    """--resume-from commit picks up at commit without re-running implement.

    Regression guard for the bug where the orchestrator silently rewound
    commit → implement, which caused an EmptyImpl loop when the
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

    monkeypatch.setattr(_gh_mod, "read_state_field", _fake_read)
    monkeypatch.setattr(_pipeline_mod, "read_state_field", _fake_read)
    monkeypatch.setattr(
        _gh_mod, "_fetch_issue_body", lambda num, repo: "# Plan\nDo stuff.\n"
    )
    monkeypatch.setattr(subprocess, "run", _make_gh_subprocess())
    monkeypatch.setattr(
        "gremlins.stages.ghreview.GHReview.run", lambda self, pipe: None
    )
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.WaitCopilot.run", lambda self, pipe: "APPROVED"
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.RequestCopilot.run",
        lambda self, pipe: None,
    )
    monkeypatch.setattr(
        "gremlins.stages.ghaddress.GHAddress.run", lambda self, pipe: None
    )
    monkeypatch.setattr("gremlins.stages.wait_ci.WaitCI.run", lambda self, pipe: None)

    client = FakeClaudeClient(
        fixtures={"commit": IMPL_EVENTS, "open-github-pr": _pr_events()}
    )

    result = gh_main(["--plan", "42", "--resume-from", "commit"], client=client)
    assert result == 0

    labels = [c.label for c in client.calls]
    assert "implement" not in labels, "implement must not run on commit resume"
    assert "commit" in labels

    # The commit prompt must contain content from the impl diff.
    commit_call = next(c for c in client.calls if c.label == "commit")
    assert "impl content" in commit_call.prompt or "impl.txt" in commit_call.prompt


def test_parse_resume_from_open_pr(capsys):
    args = _parse_gh_args(["--plan", "42", "--resume-from", "open-pr"])
    assert args.resume_from == "open-pr"


def test_resume_from_open_pr(tmp_path, monkeypatch):
    """--resume-from open-pr skips plan/implement/commit and runs open-github-pr onward."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    # Simulate a completed implement + commit: one commit above init.
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

    handoff_branch = "ghgremlin-impl-handoff-open-pr-test"
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

    monkeypatch.setattr(_gh_mod, "read_state_field", _fake_read)
    monkeypatch.setattr(_pipeline_mod, "read_state_field", _fake_read)
    monkeypatch.setattr(
        _gh_mod, "_fetch_issue_body", lambda num, repo: "# Plan\nDo stuff.\n"
    )
    monkeypatch.setattr(subprocess, "run", _make_gh_subprocess())
    ghreview_called = []
    monkeypatch.setattr(
        "gremlins.stages.ghreview.GHReview.run",
        lambda self, pipe: ghreview_called.append(self.pr_url),
    )
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.WaitCopilot.run", lambda self, pipe: "APPROVED"
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.RequestCopilot.run",
        lambda self, pipe: None,
    )
    monkeypatch.setattr(
        "gremlins.stages.ghaddress.GHAddress.run", lambda self, pipe: None
    )
    monkeypatch.setattr("gremlins.stages.wait_ci.WaitCI.run", lambda self, pipe: None)

    client = FakeClaudeClient(fixtures={"open-github-pr": _pr_events()})

    result = gh_main(["--plan", "42", "--resume-from", "open-pr"], client=client)
    assert result == 0

    labels = [c.label for c in client.calls]
    assert "implement" not in labels, "implement must not run on open-pr resume"
    assert "commit" not in labels, "commit must not run on open-pr resume"
    assert "open-github-pr" in labels

    # pr_url was extracted and threaded into downstream stages
    assert ghreview_called == ["https://github.com/owner/repo/pull/101"]


# ---------------------------------------------------------------------------
# wait-copilot stage: argument wiring
# ---------------------------------------------------------------------------


def test_wait_copilot_stage_argument_wiring(tmp_path, monkeypatch):
    """WaitCopilot receives repo and pr_num as instance attrs, session_dir via state."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    session_dir, _state_file = _patch_common(monkeypatch, tmp_path)

    monkeypatch.setattr(
        subprocess,
        "run",
        _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n"),
    )
    monkeypatch.setattr(
        "gremlins.stages.ghreview.GHReview.run", lambda self, pipe: None
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.RequestCopilot.run",
        lambda self, pipe: None,
    )
    monkeypatch.setattr(
        "gremlins.stages.ghaddress.GHAddress.run", lambda self, pipe: None
    )
    monkeypatch.setattr("gremlins.stages.verify.Verify.run", lambda self, pipe: None)
    monkeypatch.setattr("gremlins.stages.wait_ci.WaitCI.run", lambda self, pipe: None)

    captured_stage = {}

    def record_wait_copilot(self, pipe):
        captured_stage["stage"] = self
        return "APPROVED"

    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.WaitCopilot.run", record_wait_copilot
    )

    client = _CommittingClient(
        git_dir=tmp_path,
        fixtures={
            "implement": IMPL_EVENTS,
            "commit": IMPL_EVENTS,
            "open-github-pr": _pr_events("https://github.com/owner/repo/pull/77"),
        },
    )

    result = gh_main(
        ["--plan", "42", "--client", "claude:claude-opus-4-7"], client=client
    )
    assert result == 0

    stage = captured_stage["stage"]
    assert stage.repo == "owner/repo"
    assert stage.pr_num == "77"
    assert stage.state.session_dir == session_dir


# ---------------------------------------------------------------------------
# ci-gate stage: argument wiring, ordering, and resume behavior
# ---------------------------------------------------------------------------


def test_wait_ci_stage_argument_wiring(tmp_path, monkeypatch):
    """WaitCI receives pr_url, model as instance attrs, session_dir via state."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    session_dir, _state_file = _patch_common(monkeypatch, tmp_path)

    monkeypatch.setattr(
        subprocess,
        "run",
        _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n"),
    )
    monkeypatch.setattr(
        "gremlins.stages.ghreview.GHReview.run", lambda self, pipe: None
    )
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.WaitCopilot.run", lambda self, pipe: "APPROVED"
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.RequestCopilot.run",
        lambda self, pipe: None,
    )
    monkeypatch.setattr(
        "gremlins.stages.ghaddress.GHAddress.run", lambda self, pipe: None
    )
    monkeypatch.setattr("gremlins.stages.verify.Verify.run", lambda self, pipe: None)

    captured_stage = {}

    def record_wait_ci(self, pipe):
        captured_stage["stage"] = self

    monkeypatch.setattr("gremlins.stages.wait_ci.WaitCI.run", record_wait_ci)

    client = _CommittingClient(
        git_dir=tmp_path,
        fixtures={
            "implement": IMPL_EVENTS,
            "commit": IMPL_EVENTS,
            "open-github-pr": _pr_events("https://github.com/owner/repo/pull/77"),
        },
    )

    result = gh_main(
        ["--plan", "42", "--client", "claude:claude-opus-4-7"], client=client
    )
    assert result == 0

    stage = captured_stage["stage"]
    assert stage.pr_url == "https://github.com/owner/repo/pull/77"
    assert stage.model == "claude-opus-4-7"
    assert stage.state.session_dir == session_dir


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
        "gremlins.stages.verify.Verify.run",
        lambda self, pipe: order.append("verify"),
    )
    monkeypatch.setattr(
        "gremlins.stages.ghreview.GHReview.run",
        lambda self, pipe: order.append("ghreview"),
    )
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.WaitCopilot.run",
        lambda self, pipe: order.append("wait-copilot") or "APPROVED",
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.RequestCopilot.run",
        lambda self, pipe: order.append("request-copilot"),
    )
    monkeypatch.setattr(
        "gremlins.stages.ghaddress.GHAddress.run",
        lambda self, pipe: order.append("ghaddress"),
    )
    monkeypatch.setattr(
        "gremlins.stages.wait_ci.WaitCI.run",
        lambda self, pipe: order.append("ci-gate"),
    )

    client = _CommittingClient(
        git_dir=tmp_path,
        fixtures={
            "implement": IMPL_EVENTS,
            "commit": IMPL_EVENTS,
            "open-github-pr": _pr_events(),
        },
    )

    result = gh_main(["--plan", "42"], client=client)
    assert result == 0

    assert order[0] == "verify", "verify must run before other tracked stages"
    assert order[-2:] == ["ghaddress", "ci-gate"]
    assert order.count("verify") == 1
    assert order.count("ci-gate") == 1


def test_resume_from_ci_gate(tmp_path, monkeypatch):
    """--resume-from ci-gate skips all earlier stages and calls only WaitCI.run."""
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

    monkeypatch.setattr(_gh_mod, "read_state_field", _fake_read)
    monkeypatch.setattr(_pipeline_mod, "read_state_field", _fake_read)
    monkeypatch.setattr(
        _gh_mod, "_fetch_issue_body", lambda num, repo: "# Plan\nContent.\n"
    )

    earlier_called: list[str] = []
    ci_stages = []

    monkeypatch.setattr(
        "gremlins.stages.ghreview.GHReview.run",
        lambda self, pipe: earlier_called.append("ghreview"),
    )
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.WaitCopilot.run",
        lambda self, pipe: earlier_called.append("wait-copilot") or "APPROVED",
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.RequestCopilot.run",
        lambda self, pipe: earlier_called.append("request-copilot"),
    )
    monkeypatch.setattr(
        "gremlins.stages.ghaddress.GHAddress.run",
        lambda self, pipe: earlier_called.append("ghaddress"),
    )
    monkeypatch.setattr(
        "gremlins.stages.wait_ci.WaitCI.run",
        lambda self, pipe: ci_stages.append(self),
    )
    monkeypatch.setattr(subprocess, "run", _make_gh_subprocess())

    client = FakeClaudeClient(fixtures={})

    result = gh_main(["--plan", "5", "--resume-from", "ci-gate"], client=client)
    assert result == 0

    assert client.calls == [], "no client stages should run on ci-gate resume"
    assert earlier_called == [], "earlier stages must be skipped"
    assert len(ci_stages) == 1
    assert ci_stages[0].pr_url == "https://github.com/owner/repo/pull/200"


# ---------------------------------------------------------------------------
# verify stage: argument wiring and resume behavior
# ---------------------------------------------------------------------------


def test_verify_stage_argument_wiring(tmp_path, monkeypatch):
    """verify.run receives fix_model, cwd via options, session_dir via ctx."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    session_dir, _state_file = _patch_common(monkeypatch, tmp_path)

    monkeypatch.setattr(
        subprocess,
        "run",
        _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n"),
    )
    monkeypatch.setattr(
        "gremlins.stages.ghreview.GHReview.run", lambda self, pipe: None
    )
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.WaitCopilot.run", lambda self, pipe: "APPROVED"
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.RequestCopilot.run",
        lambda self, pipe: None,
    )
    monkeypatch.setattr(
        "gremlins.stages.ghaddress.GHAddress.run", lambda self, pipe: None
    )
    monkeypatch.setattr("gremlins.stages.wait_ci.WaitCI.run", lambda self, pipe: None)

    captured_stage = {}

    def record_verify(self, pipe):
        captured_stage["stage"] = self

    monkeypatch.setattr("gremlins.stages.verify.Verify.run", record_verify)

    client = _CommittingClient(
        git_dir=tmp_path,
        fixtures={
            "implement": IMPL_EVENTS,
            "commit": IMPL_EVENTS,
            "open-github-pr": _pr_events("https://github.com/owner/repo/pull/77"),
        },
    )

    result = gh_main(
        ["--plan", "42", "--client", "claude:claude-opus-4-7"], client=client
    )
    assert result == 0

    stage = captured_stage["stage"]
    assert stage.model == "claude-opus-4-7"
    assert stage.options.get("cmds") == ["make check", "make test"]
    assert stage.state.session_dir == session_dir


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

    monkeypatch.setattr(_gh_mod, "read_state_field", _fake_read)
    monkeypatch.setattr(_pipeline_mod, "read_state_field", _fake_read)
    monkeypatch.setattr(
        _gh_mod, "_fetch_issue_body", lambda num, repo: "# Plan\nContent.\n"
    )

    earlier_called: list[str] = []
    verify_calls = []

    monkeypatch.setattr(
        "gremlins.stages.verify.Verify.run",
        lambda self, pipe: verify_calls.append(self),
    )
    monkeypatch.setattr(
        "gremlins.stages.ghreview.GHReview.run",
        lambda self, pipe: earlier_called.append("ghreview"),
    )
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.WaitCopilot.run",
        lambda self, pipe: "APPROVED",
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.RequestCopilot.run",
        lambda self, pipe: None,
    )
    monkeypatch.setattr(
        "gremlins.stages.ghaddress.GHAddress.run", lambda self, pipe: None
    )
    monkeypatch.setattr("gremlins.stages.wait_ci.WaitCI.run", lambda self, pipe: None)
    monkeypatch.setattr(subprocess, "run", _make_gh_subprocess())

    client = FakeClaudeClient(
        fixtures={"commit": IMPL_EVENTS, "open-github-pr": _pr_events()}
    )

    result = gh_main(["--plan", "5", "--resume-from", "verify"], client=client)
    assert result == 0

    labels = [c.label for c in client.calls]
    assert "implement" not in labels, "implement must not run on verify resume"
    assert len(verify_calls) == 1


def test_gh_main_writes_stage_to_state(tmp_path, monkeypatch, make_state_dir):
    """gr_id threads into set_stage and writes to the isolated state root."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    gr_id = "test-gr-id"
    state_dir = make_state_dir(gr_id)

    _patch_common(monkeypatch, tmp_path)

    monkeypatch.setattr(
        subprocess, "run", _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n")
    )
    monkeypatch.setattr(
        "gremlins.stages.ghreview.GHReview.run", lambda self, pipe: None
    )
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.WaitCopilot.run", lambda self, pipe: "APPROVED"
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.RequestCopilot.run", lambda self, pipe: None
    )
    monkeypatch.setattr(
        "gremlins.stages.ghaddress.GHAddress.run", lambda self, pipe: None
    )
    monkeypatch.setattr("gremlins.stages.verify.Verify.run", lambda self, pipe: None)
    monkeypatch.setattr("gremlins.stages.wait_ci.WaitCI.run", lambda self, pipe: None)

    client = _CommittingClient(
        git_dir=tmp_path,
        fixtures={
            "implement": IMPL_EVENTS,
            "commit": IMPL_EVENTS,
            "open-github-pr": _pr_events(),
        },
    )

    result = gh_main(["--plan", "42"], gr_id=gr_id, client=client)
    assert result == 0

    data = json.loads((state_dir / "state.json").read_text())
    assert data.get("stage") == "ci-gate"


def test_gh_main_state_client_tracks_effective_model(
    tmp_path, monkeypatch, make_state_dir
):
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    gr_id = "test-gr-id"
    state_dir = make_state_dir(gr_id)

    _patch_common(monkeypatch, tmp_path)

    # Restore the real patch_state so stage_clients is actually written
    import gremlins.state as _state_mod

    monkeypatch.setattr("gremlins.orchestrators.gh.patch_state", _state_mod.patch_state)

    monkeypatch.setattr(
        subprocess, "run", _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n")
    )
    monkeypatch.setattr(
        "gremlins.stages.ghreview.GHReview.run", lambda self, pipe: None
    )
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.WaitCopilot.run", lambda self, pipe: "APPROVED"
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.RequestCopilot.run", lambda self, pipe: None
    )
    monkeypatch.setattr(
        "gremlins.stages.ghaddress.GHAddress.run", lambda self, pipe: None
    )
    monkeypatch.setattr("gremlins.stages.verify.Verify.run", lambda self, pipe: None)
    monkeypatch.setattr("gremlins.stages.wait_ci.WaitCI.run", lambda self, pipe: None)

    client = _CommittingClient(
        git_dir=tmp_path,
        fixtures={
            "implement": IMPL_EVENTS,
            "commit": IMPL_EVENTS,
            "open-github-pr": _pr_events(),
        },
    )

    result = gh_main(
        ["--plan", "42", "--client", "copilot:gpt-5.4"],
        gr_id=gr_id,
        client=client,
    )
    assert result == 0

    data = json.loads((state_dir / "state.json").read_text())
    assert "model" not in data
    assert data.get("stage_clients", {}).get("implement") == "copilot:gpt-5.4"


def test_gh_main_pipeline_default_client_model(tmp_path, monkeypatch):
    """pipeline.default_client model used when --client is absent.

    Regression: the model was extracted only from --model / --client, not from
    the pipeline's default_client. A pipeline with default_client: copilot:gpt-5.4
    produced model=sonnet, causing the Copilot client to fail immediately.
    """
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    session_dir, state_file = _patch_common(monkeypatch, tmp_path)

    # Override load_pipeline (wins over _patch_common's version) to inject
    # default_client without a live client instance.
    from gremlins.clients import ClientSpec as _ClientSpec

    _real_load_pipeline = _gh_mod.load_pipeline

    def _load_pipeline_copilot_default(path):
        pipeline = _real_load_pipeline(path)
        stripped_stages = [dataclasses.replace(s, client=None) for s in pipeline.stages]
        return dataclasses.replace(
            pipeline,
            default_client=_ClientSpec("copilot", "gpt-5.4"),
            stages=stripped_stages,
        )

    monkeypatch.setattr(
        "gremlins.orchestrators.gh.load_pipeline", _load_pipeline_copilot_default
    )

    monkeypatch.setattr(
        subprocess, "run", _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n")
    )
    monkeypatch.setattr(
        "gremlins.stages.ghreview.GHReview.run", lambda self, pipe: None
    )
    monkeypatch.setattr(
        "gremlins.stages.wait_copilot.WaitCopilot.run", lambda self, pipe: "APPROVED"
    )
    monkeypatch.setattr(
        "gremlins.stages.request_copilot.RequestCopilot.run", lambda self, pipe: None
    )
    monkeypatch.setattr(
        "gremlins.stages.ghaddress.GHAddress.run", lambda self, pipe: None
    )
    monkeypatch.setattr("gremlins.stages.verify.Verify.run", lambda self, pipe: None)
    monkeypatch.setattr("gremlins.stages.wait_ci.WaitCI.run", lambda self, pipe: None)

    client = _CommittingClient(
        git_dir=tmp_path,
        fixtures={
            "implement": IMPL_EVENTS,
            "commit": IMPL_EVENTS,
            "open-github-pr": _pr_events(),
        },
    )

    result = gh_main(["--plan", "42"], client=client)
    assert result == 0

    assert client.calls, "expected at least one client call"
    bad = [c for c in client.calls if c.model != "gpt-5.4"]
    assert not bad, (
        f"{len(bad)} stage(s) used wrong model: {[(c.label, c.model) for c in bad]}"
    )
