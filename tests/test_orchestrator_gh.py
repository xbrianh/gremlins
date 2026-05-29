"""Tests for gremlins.executor.run and supporting git helpers.

Uses FakeClaudeClient throughout — no real claude subprocess or gh CLI calls
(gh calls are monkeypatched at the subprocess.run level).
"""

import asyncio
import dataclasses
import json
import pathlib
import re
import shutil
import subprocess
from collections.abc import Callable
from typing import Any

import pytest
from conftest import MINIMAL_EVENTS

from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.run import _parse_args as _parse_gh_args
from gremlins.executor.run import run_pipeline
from gremlins.pipeline import Pipeline
from gremlins.pipeline.discovery import resolve_pipeline_path


def _init_git_repo(path: pathlib.Path) -> None:
    path.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        ["git", "init", "-b", "main"], cwd=path, check=True, capture_output=True
    )
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


def _async(fn: Callable[..., Any]) -> Callable[..., Any]:
    async def _w(*a: Any, **kw: Any) -> Any:
        return fn(*a, **kw)

    return _w


def _gh_pipeline_path(cwd):
    return resolve_pipeline_path("gh", cwd)


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


def _patch_common(
    monkeypatch, tmp_path, *, state_data: dict = None, fake_pr_number: str = "101"
):
    """Apply standard monkeypatches for gh_main smoke tests."""
    monkeypatch.setattr(
        shutil,
        "which",
        lambda n: f"/fake/{n}" if n in ("claude", "gh", "git") else None,
    )
    monkeypatch.setattr(
        "gremlins.executor.run._install_signal_handlers", lambda c, gid: None
    )
    monkeypatch.setattr("gremlins.executor.run._get_repo", lambda: "owner/repo")

    session_dir = tmp_path / "session"
    session_dir.mkdir()
    monkeypatch.setattr(
        "gremlins.executor.run.resolve_session_dir", lambda gremlin_id=None: session_dir
    )

    state_file = tmp_path / "state.json"
    head_r = _real_subprocess_run(
        ["git", "rev-parse", "HEAD"], cwd=tmp_path, capture_output=True, text=True
    )
    base_ref_sha = head_r.stdout.strip() if head_r.returncode == 0 else ""
    initial = {
        "id": "gr-test",
        "kind": "ghgremlin",
        "stage": "starting",
    }
    if state_data:
        initial.update(state_data)
    state_file.write_text(json.dumps(initial))
    # base_ref_sha is now stored in registry.json, not state.json
    # spec and plan are always bound at launch; bind them here so the implement
    # agent stage can resolve both even when the plan stage is skipped.
    registry_data: dict = {
        "spec": "file://session/spec.md",
        "plan": "file://session/plan.md",
        "pr-url": "file://session/pr-url.txt",
        "pr-branch": "file://session/pr-branch.txt",
        "pr-number": "file://session/pr-number.txt",
    }
    if base_ref_sha:
        registry_data["base_sha"] = f"git://commit/{base_ref_sha}"
    registry_file = tmp_path / "registry.json"
    registry_file.write_text(json.dumps(registry_data))
    # Create placeholder artifact files so file resolvers find them.
    (session_dir / "spec.md").write_text("", encoding="utf-8")
    (session_dir / "plan.md").write_text("", encoding="utf-8")
    (session_dir / "pr-title.txt").write_text("Fake PR Title\n")
    (session_dir / "pr-body.md").write_text("Fake PR body.\n")
    (session_dir / "pr-url.txt").write_text(
        f"https://github.com/owner/repo/pull/{fake_pr_number}\n"
    )
    (session_dir / "pr-branch.txt").write_text("issue-42-fake-slug\n")
    (session_dir / "pr-number.txt").write_text(f"{fake_pr_number}\n")
    monkeypatch.setattr(
        "gremlins.executor.run.resolve_state_file", lambda gremlin_id=None: state_file
    )

    import subprocess as _subprocess_mod

    from gremlins.utils import proc as _proc_mod

    _orig_shell = _proc_mod.run_shell_async

    async def _noop_gh_shell(cmd, *, cwd=None, env=None):
        if isinstance(cmd, str):
            if cmd.lstrip().startswith("gh "):
                return _subprocess_mod.CompletedProcess(cmd, 0, "", "")
            m = re.search(r'"([^"]+/pr-number\.txt)"', cmd)
            if m:
                p = pathlib.Path(m.group(1))
                p.parent.mkdir(parents=True, exist_ok=True)
                p.write_text(f"{fake_pr_number}\n")
                # Also write pr-url.txt and pr-branch.txt produced by push-and-open.
                url_p = p.parent / "pr-url.txt"
                url_p.write_text(
                    f"https://github.com/owner/repo/pull/{fake_pr_number}\n"
                )
                # pr-branch.txt already written by compose-pr; ensure it's non-empty.
                branch_p = p.parent / "pr-branch.txt"
                if not branch_p.exists() or branch_p.stat().st_size == 0:
                    branch_p.write_text("issue-42-fake-slug\n")
                return _subprocess_mod.CompletedProcess(cmd, 0, "", "")
            m2 = re.search(r'"([^"]+/pr-base-ref\.txt)"', cmd)
            if m2:
                p = pathlib.Path(m2.group(1))
                p.parent.mkdir(parents=True, exist_ok=True)
                p.write_text("main\n")
                return _subprocess_mod.CompletedProcess(cmd, 0, "", "")
            # publish-as-issue script: intercept to write a fake issue number so
            # verify_produced passes without a real gh CLI or git remote.
            m3 = re.search(r'"([^"]+/plan-issue-number\.txt)"', cmd)
            if m3:
                p = pathlib.Path(m3.group(1))
                p.parent.mkdir(parents=True, exist_ok=True)
                p.write_text("42\n")
                return _subprocess_mod.CompletedProcess(cmd, 0, "", "")
        return await _orig_shell(cmd, cwd=cwd, env=env)

    monkeypatch.setattr(_proc_mod, "run_shell_async", _noop_gh_shell)
    monkeypatch.setattr(
        "gremlins.executor.state.resolve_state_file", lambda gremlin_id=None: state_file
    )

    # Use a writing shim so the commit runner can read back artifact values.
    def _append_artifact(self, artifact):
        data = json.loads(state_file.read_text(encoding="utf-8"))
        arts = list(data.get("artifacts") or [])
        arts.append(artifact)
        data["artifacts"] = arts
        state_file.write_text(json.dumps(data), encoding="utf-8")

    monkeypatch.setattr(
        "gremlins.executor.state.StateData.append_artifact", _append_artifact
    )

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
        # gh pr edit (github-request-copilot-review)
        if sub == "pr" and "edit" in cmd:
            return subprocess.CompletedProcess(cmd, 0, stdout="", stderr="")
        # gh pr diff
        if sub == "pr" and "diff" in cmd:
            return subprocess.CompletedProcess(cmd, 0, stdout=pr_diff, stderr="")
        # gh pr view --json url,number,headRefName (GitHubResolver.read for pr/<n>)
        if sub == "pr" and "view" in cmd and "--json" in cmd:
            num = cmd[3] if len(cmd) > 3 else "101"
            data = json.dumps(
                {
                    "url": f"https://github.com/owner/repo/pull/{num}",
                    "number": int(num),
                    "headRefName": "issue-42-impl-slug",
                }
            )
            return subprocess.CompletedProcess(cmd, 0, stdout=data, stderr="")
        # gh api (github-wait-copilot)
        if sub == "api":
            return subprocess.CompletedProcess(
                cmd, 0, stdout=copilot_state + "\n", stderr=""
            )
        return subprocess.CompletedProcess(cmd, 0, stdout="", stderr="")

    return fake_run


# ---------------------------------------------------------------------------
# _parse_gh_args — arg parsing unit tests
# ---------------------------------------------------------------------------


def test_parse_instructions():
    args = _parse_gh_args(["add a login page"])
    # A single quoted string arrives as one element in argv
    assert args.instructions == ["add a login page"]
    assert args.plan is None
    assert args.resume_from is None


def test_parse_plan_source():
    args = _parse_gh_args(["--plan", "#42"])
    assert args.plan == "#42"
    assert args.instructions == []


def test_parse_resume_from_commit(capsys):
    args = _parse_gh_args(["--plan", "#42", "--resume-from", "commit"])
    assert args.resume_from == "commit"
    captured = capsys.readouterr()
    assert "rewinding" not in captured.err


def test_parse_plan_and_instructions_mutual_exclusion():
    with pytest.raises(SystemExit):
        _parse_gh_args(["--plan", "#42", "also some instructions"])


def test_gh_pipeline_stage_names(tmp_path):
    pipeline = Pipeline.from_yaml(resolve_pipeline_path("gh", tmp_path))
    names = [s.name for s in pipeline.stages]
    assert names == [
        "inputs",
        "resolve-plan-input",
        "plan",
        "publish-as-issue",
        "update-description",
        "implement",
        "require-impl-progress",
        "normalize",
        "verify",
        "open-pr",
        "compose-pr",
        "push-and-open",
        "github-request-copilot-review",
        "github-review-pull-request",
        "github-wait-copilot",
        "github-address-pull-request-reviews",
        "ci-gate",
    ]


# ---------------------------------------------------------------------------
# gh_main — smoke test: --plan issue-ref mode (plan stage skipped)
# ---------------------------------------------------------------------------


class _CommittingClient(FakeClaudeClient):
    """FakeClaudeClient that creates a git commit when the implement label runs.

    Also writes plan.md when the plan label runs so verify_produced passes for
    the plan recipe's out: { plan_file: file://session/plan.md } binding.
    """

    def __init__(
        self,
        *args,
        git_dir: pathlib.Path = None,
        session_dir: pathlib.Path = None,
        **kwargs,
    ):
        super().__init__(*args, **kwargs)
        self._git_dir = git_dir
        self._session_dir = session_dir

    def run(self, prompt, *, label, **kwargs):
        if label == "plan" and self._session_dir is not None:
            plan_md = self._session_dir / "plan.md"
            if not plan_md.exists() or plan_md.stat().st_size == 0:
                plan_md.write_text("# Plan\nDo stuff.\n", encoding="utf-8")
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
    """--plan <issue-ref> pre-populates plan.md; plan agent sees existing content and skips."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    session_dir, state_file = _patch_common(monkeypatch, tmp_path)

    # Pre-populate plan.md and plan-issue-number.txt (simulating what
    # resolve-plan-input and publish-as-issue do in production).
    (session_dir / "plan.md").write_text("# Plan\nDo stuff.\n", encoding="utf-8")
    (session_dir / "plan-issue-number.txt").write_text("42", encoding="utf-8")

    monkeypatch.setattr(
        subprocess,
        "run",
        _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n"),
    )

    monkeypatch.setattr(
        "gremlins.stages.loop.LoopStage.run", _async(lambda self, pipe: None)
    )

    client = _CommittingClient(
        git_dir=tmp_path,
        session_dir=session_dir,
        fixtures={
            "plan": MINIMAL_EVENTS,
            "implement": IMPL_EVENTS,
            "compose-pr": MINIMAL_EVENTS,
            "github-review-pull-request": MINIMAL_EVENTS,
            "github-address-pull-request-reviews": MINIMAL_EVENTS,
        },
    )

    result = asyncio.run(
        run_pipeline(_gh_pipeline_path(tmp_path), argv=["--plan", "#42"], client=client)
    )
    assert result == 0

    labels = [c.label for c in client.calls]
    assert "implement" in labels


def test_plan_skip_if_exists_on_resume(tmp_path, monkeypatch):
    """Resume: plan stage skipped when plan artifact is already verified."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    session_dir, state_file = _patch_common(monkeypatch, tmp_path)
    # Overwrite with non-empty content so verified("plan") is True.
    (session_dir / "plan.md").write_text("# Plan\nDo stuff.\n", encoding="utf-8")

    monkeypatch.setattr(
        subprocess,
        "run",
        _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n"),
    )
    monkeypatch.setattr(
        "gremlins.stages.loop.LoopStage.run", _async(lambda self, pipe: None)
    )

    client = _CommittingClient(
        git_dir=tmp_path,
        session_dir=session_dir,
        fixtures={
            "implement": IMPL_EVENTS,
            "compose-pr": MINIMAL_EVENTS,
            "github-review-pull-request": MINIMAL_EVENTS,
            "github-address-pull-request-reviews": MINIMAL_EVENTS,
        },
    )

    result = asyncio.run(
        run_pipeline(
            _gh_pipeline_path(tmp_path), argv=["add foo feature"], client=client
        )
    )
    assert result == 0
    labels = [c.label for c in client.calls]
    assert "plan" not in labels
    assert "implement" in labels


def test_publish_as_issue_skip_if_exists(tmp_path, monkeypatch):
    """publish-as-issue skipped when plan-issue-number artifact is already verified."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    session_dir, state_file = _patch_common(monkeypatch, tmp_path)
    (session_dir / "plan.md").write_text("# Plan\nDo stuff.\n", encoding="utf-8")
    (session_dir / "plan-issue-number.txt").write_text("42", encoding="utf-8")

    # Add plan-issue-number to registry so skip_if_exists fires.
    registry_path = tmp_path / "registry.json"
    reg = json.loads(registry_path.read_text())
    reg["plan-issue-number"] = "file://session/plan-issue-number.txt"
    registry_path.write_text(json.dumps(reg))

    shell_cmds: list[str] = []
    from gremlins.utils import proc as _proc_mod

    _orig_shell = _proc_mod.run_shell_async

    async def _recording_shell(cmd, **kwargs):
        if isinstance(cmd, str):
            shell_cmds.append(cmd)
        return await _orig_shell(cmd, **kwargs)

    monkeypatch.setattr(_proc_mod, "run_shell_async", _recording_shell)

    monkeypatch.setattr(
        subprocess,
        "run",
        _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n"),
    )
    monkeypatch.setattr(
        "gremlins.stages.loop.LoopStage.run", _async(lambda self, pipe: None)
    )

    client = _CommittingClient(
        git_dir=tmp_path,
        session_dir=session_dir,
        fixtures={
            "implement": IMPL_EVENTS,
            "compose-pr": MINIMAL_EVENTS,
            "github-review-pull-request": MINIMAL_EVENTS,
            "github-address-pull-request-reviews": MINIMAL_EVENTS,
        },
    )

    result = asyncio.run(
        run_pipeline(
            _gh_pipeline_path(tmp_path), argv=["add foo feature"], client=client
        )
    )
    assert result == 0
    assert not any("gh issue create" in cmd for cmd in shell_cmds)
    assert "implement" in [c.label for c in client.calls]


def test_plan_no_h1_issue_body(tmp_path, monkeypatch):
    """resolve-plan-input prepends an H1 when the fetched issue body lacks one."""
    import os

    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    # Fake gh binary: dispatches on the --jq selector
    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    gh_bin = fake_bin / "gh"
    gh_bin.write_text(
        "#!/bin/sh\n"
        'for arg in "$@"; do\n'
        '    case "$arg" in\n'
        "        .title) printf 'Issue Title'; exit 0;;\n"
        "        .number) printf '42'; exit 0;;\n"
        "        .body) printf 'No H1 in this body.'; exit 0;;\n"
        "    esac\n"
        "done\n"
    )
    gh_bin.chmod(0o755)
    monkeypatch.setenv("PATH", f"{fake_bin}:{os.environ['PATH']}")

    from gremlins.utils import proc as _proc_mod

    _real_shell = _proc_mod.run_shell_async  # save before _patch_common patches it

    session_dir, state_file = _patch_common(monkeypatch, tmp_path)
    _noop_shell = _proc_mod.run_shell_async  # now points to _noop_gh_shell

    # Wire up plan_arg so resolve-plan-input doesn't exit at the [ -z ] guard
    registry_path = tmp_path / "registry.json"
    reg = json.loads(registry_path.read_text())
    reg["plan_arg"] = "file://session/plan-arg.txt"
    registry_path.write_text(json.dumps(reg))
    (session_dir / "plan-arg.txt").write_text("#42", encoding="utf-8")

    # Let resolve-plan-input run for real (fake gh in PATH handles the gh calls);
    # everything else stays with the noop interceptor
    async def _shell(cmd, *, cwd=None, env=None):
        if isinstance(cmd, str) and "plan.md" in cmd and "gh issue view" in cmd:
            return await _real_shell(cmd, cwd=cwd, env=env)
        return await _noop_shell(cmd, cwd=cwd, env=env)

    monkeypatch.setattr(_proc_mod, "run_shell_async", _shell)

    monkeypatch.setattr(
        subprocess,
        "run",
        _make_gh_subprocess(issue_body="No H1 in this body.\n"),
    )
    monkeypatch.setattr(
        "gremlins.stages.loop.LoopStage.run", _async(lambda self, pipe: None)
    )

    client = _CommittingClient(
        git_dir=tmp_path,
        session_dir=None,
        fixtures={
            "implement": IMPL_EVENTS,
            "compose-pr": MINIMAL_EVENTS,
            "github-review-pull-request": MINIMAL_EVENTS,
            "github-address-pull-request-reviews": MINIMAL_EVENTS,
        },
    )

    result = asyncio.run(
        run_pipeline(_gh_pipeline_path(tmp_path), argv=["--plan", "#42"], client=client)
    )
    assert result == 0
    plan_content = (session_dir / "plan.md").read_text(encoding="utf-8")
    assert plan_content.startswith("# ")
    assert (session_dir / "plan-issue-number.txt").exists()


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
        "gremlins.stages.loop.LoopStage.run", _async(lambda self, pipe: None)
    )

    client = _CommittingClient(
        git_dir=tmp_path,
        session_dir=session_dir,
        fixtures={
            "plan": _issue_events(),
            "implement": IMPL_EVENTS,
            "commit": IMPL_EVENTS,
            "compose-pr": MINIMAL_EVENTS,
            "github-review-pull-request": MINIMAL_EVENTS,
            "github-address-pull-request-reviews": MINIMAL_EVENTS,
        },
    )

    result = asyncio.run(
        run_pipeline(
            _gh_pipeline_path(tmp_path), argv=["add foo feature"], client=client
        )
    )
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
        "gremlins.stages.loop.LoopStage.run", _async(lambda self, pipe: None)
    )

    client = _CommittingClient(
        git_dir=tmp_path,
        session_dir=session_dir,
        fixtures={
            "plan": MINIMAL_EVENTS,
            "implement": IMPL_EVENTS,
            "commit": IMPL_EVENTS,
            "compose-pr": MINIMAL_EVENTS,
            "github-review-pull-request": MINIMAL_EVENTS,
            "github-address-pull-request-reviews": MINIMAL_EVENTS,
        },
    )

    result = asyncio.run(
        run_pipeline(
            _gh_pipeline_path(tmp_path),
            argv=["--plan", "#42", "--client", "claude:claude-opus-4-7"],
            client=client,
        )
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
        "gremlins.stages.loop.LoopStage.run", _async(lambda self, pipe: None)
    )

    client = _CommittingClient(
        git_dir=tmp_path,
        session_dir=session_dir,
        fixtures={
            "plan": MINIMAL_EVENTS,
            "implement": IMPL_EVENTS,
            "commit": IMPL_EVENTS,
            "compose-pr": MINIMAL_EVENTS,
            "github-review-pull-request": MINIMAL_EVENTS,
            "github-address-pull-request-reviews": MINIMAL_EVENTS,
        },
    )

    # Invoke with NO --model.
    result = asyncio.run(
        run_pipeline(_gh_pipeline_path(tmp_path), argv=["--plan", "#42"], client=client)
    )
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
        "gremlins.stages.loop.LoopStage.run", _async(lambda self, pipe: None)
    )

    client = _CommittingClient(
        git_dir=tmp_path,
        session_dir=session_dir,
        fixtures={
            "plan": MINIMAL_EVENTS,
            "implement": IMPL_EVENTS,
            "commit": IMPL_EVENTS,
            "compose-pr": MINIMAL_EVENTS,
            "github-review-pull-request": MINIMAL_EVENTS,
            "github-address-pull-request-reviews": MINIMAL_EVENTS,
        },
    )

    result = asyncio.run(
        run_pipeline(
            _gh_pipeline_path(tmp_path),
            argv=["--plan", "#42", "--client", "copilot:gpt-4o"],
            client=client,
        )
    )
    assert result == 0

    assert client.calls, "expected at least one client call"
    bad = [c for c in client.calls if c.model != "gpt-4o"]
    assert not bad, (
        f"{len(bad)} stage(s) ran on a non-gpt-4o model: "
        f"{[(c.label, c.model) for c in bad]}"
    )


def test_resume_from_implement(tmp_path, monkeypatch):
    """--resume-from implement reads plan.md from session_dir and runs implement onward."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    state_data = {
        "issue_url": "https://github.com/owner/repo/issues/99",
        "issue_num": "99",
    }
    session_dir, state_file = _patch_common(
        monkeypatch, tmp_path, state_data=state_data
    )
    (session_dir / "plan.md").write_text(
        "# Resumed Plan\nDo stuff.\n", encoding="utf-8"
    )

    monkeypatch.setattr(
        subprocess,
        "run",
        _make_gh_subprocess(issue_body="# Resumed Plan\nDo more stuff.\n"),
    )
    monkeypatch.setattr(
        "gremlins.stages.loop.LoopStage.run", _async(lambda self, pipe: None)
    )

    client = _CommittingClient(
        git_dir=tmp_path,
        fixtures={
            "implement": IMPL_EVENTS,
            "commit": IMPL_EVENTS,
            "compose-pr": MINIMAL_EVENTS,
            "github-review-pull-request": MINIMAL_EVENTS,
            "github-address-pull-request-reviews": MINIMAL_EVENTS,
        },
    )

    result = asyncio.run(
        run_pipeline(
            _gh_pipeline_path(tmp_path),
            argv=["--plan", "#99", "--resume-from", "implement"],
            client=client,
        )
    )
    assert result == 0

    labels = [c.label for c in client.calls]
    assert "plan" not in labels
    assert "implement" in labels


def test_resume_from_github_review_pull_request(tmp_path, monkeypatch):
    """--resume-from github-review-pull-request skips earlier stages and calls it."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    session_dir, state_file = _patch_common(monkeypatch, tmp_path, fake_pr_number="200")

    data = json.loads(state_file.read_text())
    data["issue_url"] = "https://github.com/owner/repo/issues/5"
    state_file.write_text(json.dumps(data))

    monkeypatch.setattr(
        "gremlins.stages.loop.LoopStage.run", _async(lambda self, pipe: None)
    )

    monkeypatch.setattr(subprocess, "run", _make_gh_subprocess())

    client = FakeClaudeClient(
        fixtures={
            "github-review-pull-request": MINIMAL_EVENTS,
            "github-address-pull-request-reviews": MINIMAL_EVENTS,
        }
    )

    result = asyncio.run(
        run_pipeline(
            _gh_pipeline_path(tmp_path),
            argv=["--plan", "#5", "--resume-from", "github-review-pull-request"],
            client=client,
        )
    )
    assert result == 0

    review_calls = [c for c in client.calls if c.label == "github-review-pull-request"]
    assert len(review_calls) == 1
    assert "https://github.com/owner/repo/pull/200" in review_calls[0].prompt


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

    # Override State.patch so it actually writes fields to state_file instead of no-op.
    def writing_patch_state(self, _delete=(), **kw):
        data = json.loads(state_file.read_text())
        for key in _delete:
            data.pop(key, None)
        data.update(kw)
        state_file.write_text(json.dumps(data))

    monkeypatch.setattr("gremlins.executor.state.StateData.patch", writing_patch_state)

    def fake_gh_run(cmd, *args, **kwargs):
        prog = cmd[0] if cmd else ""
        if prog != "gh":
            return _real_subprocess_run(cmd, *args, **kwargs)
        sub = cmd[1] if len(cmd) > 1 else ""
        if sub == "issue" and "create" in cmd:
            return subprocess.CompletedProcess(
                cmd, 0, stdout="https://github.com/owner/repo/issues/42\n", stderr=""
            )
        if sub == "issue" and "view" in cmd and "--json" in cmd:
            data = json.dumps(
                {
                    "number": 42,
                    "url": "https://github.com/owner/repo/issues/42",
                    "body": "# Feature\nDo the thing.\n",
                    "title": "Feature: Do the thing",
                }
            )
            return subprocess.CompletedProcess(cmd, 0, stdout=data, stderr="")
        if sub == "pr" and "view" in cmd and "--json" in cmd:
            num = cmd[3] if len(cmd) > 3 else "101"
            data = json.dumps(
                {
                    "url": f"https://github.com/owner/repo/pull/{num}",
                    "number": int(num),
                    "headRefName": "issue-42-impl-slug",
                }
            )
            return subprocess.CompletedProcess(cmd, 0, stdout=data, stderr="")
        return subprocess.CompletedProcess(cmd, 0, stdout="", stderr="")

    async def fake_gh_run_async(cmd, *args, **kwargs):
        return fake_gh_run(cmd, *args, **kwargs)

    monkeypatch.setattr(subprocess, "run", fake_gh_run)

    monkeypatch.setattr(
        "gremlins.stages.loop.LoopStage.run", _async(lambda self, pipe: None)
    )

    # Each fixture carries a distinct non-zero cost so a regression that drops
    # any one stage shows up as the total being short by exactly that amount.
    fixtures = {
        "plan": [
            {"type": "system", "subtype": "init"},
            {
                "type": "result",
                "subtype": "success",
                "total_cost_usd": 0.13,
            },
        ],
        "implement": [
            {"type": "system", "subtype": "init"},
            {"type": "result", "subtype": "success", "total_cost_usd": 0.07},
        ],
        "compose-pr": [
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
        "github-review-pull-request": MINIMAL_EVENTS,
        "github-address-pull-request-reviews": MINIMAL_EVENTS,
    }

    client = _CommittingClient(
        git_dir=tmp_path,
        session_dir=session_dir,
        fixtures=fixtures,
    )
    result = asyncio.run(
        run_pipeline(
            _gh_pipeline_path(tmp_path), argv=["--plan", str(plan_file)], client=client
        )
    )
    assert result == 0

    labels = [c.label for c in client.calls]
    assert "plan" in labels
    assert "implement" in labels
    assert "compose-pr" in labels

    # Read on-disk state.json — verifies both the accumulation and the persistence step.
    state = json.loads(state_file.read_text())
    assert "total_cost_usd" in state, "total_cost_usd was not persisted to state.json"

    total = state["total_cost_usd"]
    expected = 0.13 + 0.07 + 0.02
    assert total == pytest.approx(expected), (
        f"expected total {expected:.2f}, got {total:.4f}; "
        f"a regression dropping plan cost (0.13) would show total ≈ {expected - 0.13:.2f}"
    )


def test_parse_resume_from_open_pr(capsys):
    args = _parse_gh_args(["--plan", "#42", "--resume-from", "open-pr"])
    assert args.resume_from == "open-pr"


def test_resume_from_open_pr(tmp_path, monkeypatch):
    """--resume-from open-pr skips plan/implement and runs open-pr/compose-pr/push-and-open onward."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    # Simulate a completed implement: one commit above init.
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

    session_dir, state_file = _patch_common(monkeypatch, tmp_path)

    data = json.loads(state_file.read_text())
    data["issue_url"] = "https://github.com/owner/repo/issues/42"
    state_file.write_text(json.dumps(data))

    monkeypatch.setattr(subprocess, "run", _make_gh_subprocess())
    monkeypatch.setattr(
        "gremlins.stages.loop.LoopStage.run", _async(lambda self, pipe: None)
    )

    client = FakeClaudeClient(
        fixtures={
            "compose-pr": MINIMAL_EVENTS,
            "github-review-pull-request": MINIMAL_EVENTS,
            "github-address-pull-request-reviews": MINIMAL_EVENTS,
        }
    )

    result = asyncio.run(
        run_pipeline(
            _gh_pipeline_path(tmp_path),
            argv=["--plan", "#42", "--resume-from", "open-pr"],
            client=client,
        )
    )
    assert result == 0

    labels = [c.label for c in client.calls]
    assert "implement" not in labels, "implement must not run on open-pr resume"
    assert "compose-pr" in labels

    compose_pr_call = next(c for c in client.calls if c.label == "compose-pr")
    assert "gh://issue/42" in compose_pr_call.prompt, (
        "compose-pr must receive upgraded gh:// plan URI, not a file:// path"
    )

    review_calls = [c for c in client.calls if c.label == "github-review-pull-request"]
    assert len(review_calls) == 1
    assert "https://github.com/owner/repo/pull/101" in review_calls[0].prompt
    # Verify push-and-open wrote pr to registry.json
    registry_path = tmp_path / "registry.json"
    assert registry_path.exists(), "registry.json should have been written"
    assert json.loads(registry_path.read_text()).get("pr") == "gh://pr/101"


# ---------------------------------------------------------------------------
# github-wait-copilot stage: argument wiring
# ---------------------------------------------------------------------------


def test_github_wait_copilot_stage_argument_wiring(tmp_path, monkeypatch):
    """github-wait-copilot loop receives repo and session_dir; pr_url is written to state by GitHubOpenPullRequest."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    session_dir, state_file = _patch_common(monkeypatch, tmp_path, fake_pr_number="77")

    monkeypatch.setattr(
        subprocess,
        "run",
        _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n"),
    )

    captured_stages: dict = {}

    async def record_loop(self, state):
        captured_stages[self.name] = (self, state)

    monkeypatch.setattr("gremlins.stages.loop.LoopStage.run", record_loop)

    client = _CommittingClient(
        git_dir=tmp_path,
        session_dir=session_dir,
        fixtures={
            "plan": MINIMAL_EVENTS,
            "implement": IMPL_EVENTS,
            "commit": IMPL_EVENTS,
            "compose-pr": MINIMAL_EVENTS,
            "github-review-pull-request": MINIMAL_EVENTS,
            "github-address-pull-request-reviews": MINIMAL_EVENTS,
        },
    )

    result = asyncio.run(
        run_pipeline(
            _gh_pipeline_path(tmp_path),
            argv=["--plan", "#42", "--client", "claude:claude-opus-4-7"],
            client=client,
        )
    )
    assert result == 0

    assert "github-wait-copilot" in captured_stages
    _, copilot_state = captured_stages["github-wait-copilot"]
    assert copilot_state.repo == "owner/repo"
    assert copilot_state.session_dir == session_dir
    # pr is written to registry.json by push-and-open
    registry_path = tmp_path / "registry.json"
    assert registry_path.exists(), "registry.json should have been written"
    assert json.loads(registry_path.read_text()).get("pr") == "gh://pr/77"


# ---------------------------------------------------------------------------
# ci-gate stage: argument wiring, ordering, and resume behavior
# ---------------------------------------------------------------------------


def test_github_wait_ci_stage_argument_wiring(tmp_path, monkeypatch):
    """ci-gate LoopStage receives model and session_dir; pr_url is written to state by GitHubOpenPullRequest."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    session_dir, state_file = _patch_common(monkeypatch, tmp_path, fake_pr_number="77")

    monkeypatch.setattr(
        subprocess,
        "run",
        _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n"),
    )

    captured_stage = {}

    async def record_loops(self, state):
        if self.name == "ci-gate":
            captured_stage["stage"] = self
            captured_stage["state"] = state

    monkeypatch.setattr("gremlins.stages.loop.LoopStage.run", record_loops)

    client = _CommittingClient(
        git_dir=tmp_path,
        session_dir=session_dir,
        fixtures={
            "plan": MINIMAL_EVENTS,
            "implement": IMPL_EVENTS,
            "commit": IMPL_EVENTS,
            "compose-pr": MINIMAL_EVENTS,
            "github-review-pull-request": MINIMAL_EVENTS,
            "github-address-pull-request-reviews": MINIMAL_EVENTS,
        },
    )

    result = asyncio.run(
        run_pipeline(
            _gh_pipeline_path(tmp_path),
            argv=["--plan", "#42", "--client", "claude:claude-opus-4-7"],
            client=client,
        )
    )
    assert result == 0

    stage = captured_stage["stage"]
    assert stage.client.model == "claude-opus-4-7"
    assert captured_stage["state"].session_dir == session_dir
    # pr is written to registry.json by push-and-open
    registry_path = tmp_path / "registry.json"
    assert registry_path.exists(), "registry.json should have been written"
    assert json.loads(registry_path.read_text()).get("pr") == "gh://pr/77"


def test_github_wait_ci_stage_ordering(tmp_path, monkeypatch):
    """ci-gate runs after github-address-pull-request-reviews and exactly once."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    session_dir, _state_file = _patch_common(monkeypatch, tmp_path)

    monkeypatch.setattr(
        subprocess,
        "run",
        _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n"),
    )

    order: list[str] = []

    monkeypatch.setattr(
        "gremlins.stages.loop.LoopStage.run",
        _async(lambda self, pipe: order.append(self.name)),
    )

    client = _CommittingClient(
        git_dir=tmp_path,
        session_dir=session_dir,
        fixtures={
            "plan": MINIMAL_EVENTS,
            "implement": IMPL_EVENTS,
            "commit": IMPL_EVENTS,
            "compose-pr": MINIMAL_EVENTS,
            "github-review-pull-request": MINIMAL_EVENTS,
            "github-address-pull-request-reviews": MINIMAL_EVENTS,
        },
    )

    result = asyncio.run(
        run_pipeline(_gh_pipeline_path(tmp_path), argv=["--plan", "#42"], client=client)
    )
    assert result == 0

    assert order[0] == "verify", "verify must run before other tracked stages"
    assert order[-1] == "ci-gate"
    assert order.count("verify") == 1
    assert order.count("ci-gate") == 1
    labels = [c.label for c in client.calls]
    assert "github-review-pull-request" in labels
    assert "github-address-pull-request-reviews" in labels
    review_idx = labels.index("github-review-pull-request")
    addr_idx = labels.index("github-address-pull-request-reviews")
    assert review_idx < addr_idx


def test_resume_from_ci_gate(tmp_path, monkeypatch):
    """--resume-from ci-gate skips all earlier stages and calls only ci-gate LoopStage.run."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    _session_dir, state_file = _patch_common(monkeypatch, tmp_path)

    data = json.loads(state_file.read_text())
    data["issue_url"] = "https://github.com/owner/repo/issues/5"
    data.setdefault("artifacts", []).append(
        {"type": "pr", "url": "https://github.com/owner/repo/pull/200", "branch": ""}
    )
    state_file.write_text(json.dumps(data))

    loop_calls: list[str] = []

    async def track_loops(self, state):
        loop_calls.append(self.name)

    monkeypatch.setattr("gremlins.stages.loop.LoopStage.run", track_loops)
    monkeypatch.setattr(subprocess, "run", _make_gh_subprocess())

    client = FakeClaudeClient(fixtures={})

    result = asyncio.run(
        run_pipeline(
            _gh_pipeline_path(tmp_path),
            argv=["--plan", "#5", "--resume-from", "ci-gate"],
            client=client,
        )
    )
    assert result == 0

    assert client.calls == [], "no client stages should run on ci-gate resume"
    assert loop_calls == ["ci-gate"], "only ci-gate loop should run"
    # pr artifact is pre-populated in state.json; verify it's there
    state = json.loads(state_file.read_text())
    pr_artifacts = [a for a in state.get("artifacts", []) if a.get("type") == "pr"]
    assert (
        pr_artifacts
        and pr_artifacts[-1].get("url") == "https://github.com/owner/repo/pull/200"
    )


# ---------------------------------------------------------------------------
# verify stage: argument wiring and resume behavior
# ---------------------------------------------------------------------------


def test_verify_stage_argument_wiring(tmp_path, monkeypatch):
    """verify.run receives fix_model, cwd via options, session_dir via ctx."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    session_dir, _state_file = _patch_common(monkeypatch, tmp_path, fake_pr_number="77")

    monkeypatch.setattr(
        subprocess,
        "run",
        _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n"),
    )

    captured_stage = {}

    async def record_verify(self, state):
        if self.name == "verify":
            captured_stage["stage"] = self
            captured_stage["state"] = state

    monkeypatch.setattr("gremlins.stages.loop.LoopStage.run", record_verify)

    client = _CommittingClient(
        git_dir=tmp_path,
        session_dir=session_dir,
        fixtures={
            "plan": MINIMAL_EVENTS,
            "implement": IMPL_EVENTS,
            "commit": IMPL_EVENTS,
            "compose-pr": MINIMAL_EVENTS,
            "github-review-pull-request": MINIMAL_EVENTS,
            "github-address-pull-request-reviews": MINIMAL_EVENTS,
        },
    )

    result = asyncio.run(
        run_pipeline(
            _gh_pipeline_path(tmp_path),
            argv=["--plan", "#42", "--client", "claude:claude-opus-4-7"],
            client=client,
        )
    )
    assert result == 0

    stage = captured_stage["stage"]
    assert stage.client.model == "claude-opus-4-7"
    # cmds are on the cmd exec stage inside the loop body; first cmd is the user cmd
    cmd_stage = stage.body[0]
    assert cmd_stage.options.get("cmds")[0] == "make check && make test"
    assert captured_stage["state"].session_dir == session_dir


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

    _session_dir, state_file = _patch_common(monkeypatch, tmp_path)

    data = json.loads(state_file.read_text())
    data["issue_url"] = "https://github.com/owner/repo/issues/5"
    state_file.write_text(json.dumps(data))

    verify_calls = []

    monkeypatch.setattr(
        "gremlins.stages.loop.LoopStage.run",
        _async(lambda self, pipe: verify_calls.append(self)),
    )

    monkeypatch.setattr(subprocess, "run", _make_gh_subprocess())

    client = FakeClaudeClient(
        fixtures={
            "compose-pr": MINIMAL_EVENTS,
            "github-review-pull-request": MINIMAL_EVENTS,
            "github-address-pull-request-reviews": MINIMAL_EVENTS,
        }
    )

    result = asyncio.run(
        run_pipeline(
            _gh_pipeline_path(tmp_path),
            argv=["--plan", "#5", "--resume-from", "verify"],
            client=client,
        )
    )
    assert result == 0

    labels = [c.label for c in client.calls]
    assert "implement" not in labels, "implement must not run on verify resume"
    assert sum(1 for s in verify_calls if s.name == "verify") == 1


def test_gh_main_writes_stage_to_state(tmp_path, monkeypatch):
    """set_stage writes the stage name to the state file threaded through State."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    gremlin_id = "test-gr-id"
    session_dir, state_file = _patch_common(monkeypatch, tmp_path)

    monkeypatch.setattr(
        subprocess, "run", _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n")
    )
    monkeypatch.setattr(
        "gremlins.stages.loop.LoopStage.run", _async(lambda self, pipe: None)
    )

    client = _CommittingClient(
        git_dir=tmp_path,
        session_dir=session_dir,
        fixtures={
            "plan": MINIMAL_EVENTS,
            "implement": IMPL_EVENTS,
            "commit": IMPL_EVENTS,
            "compose-pr": MINIMAL_EVENTS,
            "github-review-pull-request": MINIMAL_EVENTS,
            "github-address-pull-request-reviews": MINIMAL_EVENTS,
        },
    )

    result = asyncio.run(
        run_pipeline(
            _gh_pipeline_path(tmp_path),
            argv=["--plan", "#42"],
            gremlin_id=gremlin_id,
            client=client,
        )
    )
    assert result == 0

    data = json.loads(state_file.read_text())
    assert data.get("stage") == "ci-gate"


def test_gh_main_state_client_tracks_effective_model(
    tmp_path, monkeypatch, make_state_dir
):
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    gremlin_id = "test-gr-id"
    state_dir = make_state_dir(gremlin_id)

    session_dir, _ = _patch_common(monkeypatch, tmp_path)

    monkeypatch.setattr(
        subprocess, "run", _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n")
    )
    monkeypatch.setattr(
        "gremlins.stages.loop.LoopStage.run", _async(lambda self, pipe: None)
    )

    client = _CommittingClient(
        git_dir=tmp_path,
        session_dir=session_dir,
        fixtures={
            "plan": MINIMAL_EVENTS,
            "implement": IMPL_EVENTS,
            "commit": IMPL_EVENTS,
            "compose-pr": MINIMAL_EVENTS,
            "github-review-pull-request": MINIMAL_EVENTS,
            "github-address-pull-request-reviews": MINIMAL_EVENTS,
        },
    )

    result = asyncio.run(
        run_pipeline(
            _gh_pipeline_path(tmp_path),
            argv=["--plan", "#42", "--client", "copilot:gpt-5.4"],
            gremlin_id=gremlin_id,
            client=client,
        )
    )
    assert result == 0

    data = json.loads((state_dir / "state.json").read_text())
    assert "model" not in data


def test_gh_main_pipeline_default_client_model(tmp_path, monkeypatch):
    """pipeline.default_client model used when --client is absent.

    Regression: the model was extracted only from --model / --client, not from
    the pipeline's default_client. A pipeline with default_client: copilot:gpt-5.4
    produced model=sonnet, causing the Copilot client to fail immediately.
    """
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    session_dir, state_file = _patch_common(monkeypatch, tmp_path)

    # Override Pipeline.from_yaml to inject default_client: copilot:gpt-5.4 and
    # re-fill stage clients so every stage inherits that model.
    from gremlins.clients.client import Client
    from gremlins.pipeline import _fill_stage_clients

    _real_from_yaml = Pipeline.from_yaml

    def _strip_clients_2(stage):
        stage.client = None
        for child in getattr(stage, "body", []):
            _strip_clients_2(child)

    def _from_yaml_copilot_default(path):
        pipeline = _real_from_yaml(path)
        new_default = Client("copilot", "gpt-5.4")
        for s in pipeline.stages:
            _strip_clients_2(s)
        _fill_stage_clients(pipeline.stages, new_default)
        return dataclasses.replace(pipeline, default_client=new_default)

    monkeypatch.setattr(
        "gremlins.pipeline.Pipeline.from_yaml", _from_yaml_copilot_default
    )

    monkeypatch.setattr(
        subprocess, "run", _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n")
    )
    monkeypatch.setattr(
        "gremlins.stages.loop.LoopStage.run", _async(lambda self, pipe: None)
    )

    client = _CommittingClient(
        git_dir=tmp_path,
        session_dir=session_dir,
        fixtures={
            "plan": MINIMAL_EVENTS,
            "implement": IMPL_EVENTS,
            "commit": IMPL_EVENTS,
            "compose-pr": MINIMAL_EVENTS,
            "github-review-pull-request": MINIMAL_EVENTS,
            "github-address-pull-request-reviews": MINIMAL_EVENTS,
        },
    )

    result = asyncio.run(
        run_pipeline(_gh_pipeline_path(tmp_path), argv=["--plan", "#42"], client=client)
    )
    assert result == 0

    assert client.calls, "expected at least one client call"
    bad = [c for c in client.calls if c.model != "gpt-5.4"]
    assert not bad, (
        f"{len(bad)} stage(s) used wrong model: {[(c.label, c.model) for c in bad]}"
    )


# ---------------------------------------------------------------------------
# stage_inputs wiring: gh pipeline reads instructions from state.json
# ---------------------------------------------------------------------------


def test_gh_stage_inputs_instructions_reach_plan(tmp_path, monkeypatch):
    """stage_inputs["instructions"] from state.json is passed to plan.Plan, and
    takes precedence over the CLI positional argument."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    session_dir, state_file = _patch_common(
        monkeypatch,
        tmp_path,
        state_data={"stage_inputs": {"instructions": "instr from state"}},
    )

    monkeypatch.setattr(subprocess, "run", _make_gh_subprocess())
    monkeypatch.setattr(
        "gremlins.stages.loop.LoopStage.run", _async(lambda self, pipe: None)
    )

    client = _CommittingClient(
        git_dir=tmp_path,
        session_dir=session_dir,
        fixtures={
            "plan": _issue_events(),
            "implement": IMPL_EVENTS,
            "commit": IMPL_EVENTS,
            "compose-pr": MINIMAL_EVENTS,
            "github-review-pull-request": MINIMAL_EVENTS,
            "github-address-pull-request-reviews": MINIMAL_EVENTS,
        },
    )

    result = asyncio.run(
        run_pipeline(
            _gh_pipeline_path(tmp_path), argv=["instr from cli"], client=client
        )
    )
    assert result == 0

    plan_call = next(c for c in client.calls if c.label == "plan")
    assert "instr from state" in plan_call.prompt
    assert "instr from cli" not in plan_call.prompt
