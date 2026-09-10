"""Tests for gremlins.executor.run and supporting git helpers.

Uses FakeClient throughout — no real claude subprocess or gh CLI calls
(gh calls are monkeypatched at the subprocess.run level).
"""

import asyncio
import json
import os
import pathlib
import re
import shutil
import subprocess
from collections.abc import Callable
from typing import Any

import pytest
from _gremlins_core.schemas import Pipeline
from conftest import MINIMAL_EVENTS

from gremlins.executor.run import _parse_args as _parse_gh_args
from gremlins.executor.run import run_pipeline
from tests.fake_client import FakeClient


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


def _gh_pipeline_path():
    from conftest import PIPELINE_FIXTURES_DIR

    return PIPELINE_FIXTURES_DIR / "gh.yaml"


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


def _try_handle_known_gh_cmd(cmd, cmd_resolved, fake_pr_number="101"):
    """Intercept known gh/shell commands for GH pipeline smoke tests.

    Returns a CompletedProcess if the command was handled, or None to
    let it fall through to the real implementation.
    """
    if not isinstance(cmd, str):
        return None
    import subprocess as _sp

    # Handle custom gremlins scripts that aren't gh subcommands.
    if "gh_resolve_plan_source" in cmd:
        return _sp.CompletedProcess(cmd, 0, "", "")
    if "gh_publish_issue" in cmd:
        # Write plan-issue-number.txt so verify_produced passes.
        m_pub = re.search(r'"([^"]+/plan-issue-number\.txt)"', cmd_resolved)
        if m_pub:
            p = pathlib.Path(m_pub.group(1))
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text("42\n")
        return _sp.CompletedProcess(cmd, 0, "", "")
    if cmd.lstrip().startswith("gh "):
        # If gh repo view, write repo.txt so discover stage passes
        if "gh repo view" in cmd:
            m_repo = re.search(r'"([^"]+/repo\.txt)"', cmd_resolved)
            if m_repo:
                p = pathlib.Path(m_repo.group(1))
                p.parent.mkdir(parents=True, exist_ok=True)
                p.write_text("owner/repo\n")
        # If gh pr diff, write diff.txt so fetch-pr-diff stage produces the artifact
        if "gh pr diff" in cmd:
            m_diff = re.search(r'"([^"]+/diff\.txt)"', cmd_resolved)
            if m_diff:
                p = pathlib.Path(m_diff.group(1))
                p.parent.mkdir(parents=True, exist_ok=True)
                p.write_text("diff --git a/f b/f\n")
        return _sp.CompletedProcess(cmd, 0, "", "")
    m = re.search(r'"([^"]+/pr-number\.txt)"', cmd_resolved)
    if m:
        p = pathlib.Path(m.group(1))
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(f"{fake_pr_number}\n")
        # Also write pr-url.txt and pr-branch.txt produced by push-and-open.
        url_p = p.parent / "pr-url.txt"
        url_p.write_text(f"https://github.com/owner/repo/pull/{fake_pr_number}\n")
        # pr-branch.txt already written by compose-pr; ensure it's non-empty.
        branch_p = p.parent / "pr-branch.txt"
        if not branch_p.exists() or branch_p.stat().st_size == 0:
            branch_p.write_text("issue-42-fake-slug\n")
        return _sp.CompletedProcess(cmd, 0, "", "")
    m2 = re.search(r'"([^"]+/pr-base-ref\.txt)"', cmd_resolved)
    if m2:
        p = pathlib.Path(m2.group(1))
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text("main\n")
        return _sp.CompletedProcess(cmd, 0, "", "")
    # publish-as-issue script: intercept to write a fake issue number so
    # verify_produced passes without a real gh CLI or git remote.
    m3 = re.search(r'"([^"]+/plan-issue-number\.txt)"', cmd_resolved)
    if m3 and "gh issue create" in cmd:
        p = pathlib.Path(m3.group(1))
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text("42\n")
        return _sp.CompletedProcess(cmd, 0, "", "")
    # gather-github-review-content: intercept to write a fake
    # github-review-content.md so the stage passes without a real gh CLI.
    m4 = re.search(r'"([^"]+/github-review-content\.md)"', cmd_resolved)
    if m4:
        p = pathlib.Path(m4.group(1))
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text("# PR Review Comments\n\nFake review content.\n")
        return _sp.CompletedProcess(cmd, 0, "", "")
    # Gremlins custom scripts (gh_*, git_*): the test doesn't have them on
    # PATH and doesn't need them to actually run — the artifacts they
    # produce are already pre-populated by _patch_common.
    if cmd.lstrip().startswith("gh_") or "git_diff_summary" in cmd:
        # Handle any > redirections so the output file is created.
        for m_redir in re.finditer(r'>\s*"([^"]+)"', cmd_resolved):
            p = pathlib.Path(m_redir.group(1))
            p.parent.mkdir(parents=True, exist_ok=True)
            if not p.exists():
                p.write_text("")
        return _sp.CompletedProcess(cmd, 0, "", "")
    return None


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
        "gremlins.executor.run._install_signal_handlers", lambda c, g: None
    )
    monkeypatch.setenv("GREMLINS_SANDBOX_ROOT", str(tmp_path))

    artifact_dir = tmp_path / "scratch" / "gr-test" / "artifacts"
    artifact_dir.mkdir(parents=True, exist_ok=True)

    state_file = tmp_path / "state" / "gr-test" / "state.json"
    state_file.parent.mkdir(parents=True, exist_ok=True)
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
    # plan is always a local file; plan-source-issue-number is bound when
    # the plan originated from a GitHub issue.
    # Only keys whose producing stage is skipped via skip_if_exists are
    # pre-registered. Keys the pipeline itself binds are merely pre-created on
    # disk — registering them here would now collide with the stage's bind.
    registry_data: dict = {
        "artifact://spec.md": "file://session/spec.md",
        "artifact://plan.md": "file://session/plan.md",
        "artifact://repo.txt": "file://session/repo.txt",
    }
    if base_ref_sha:
        # Write base_sha as a file (as the launcher does)
        base_sha_file = artifact_dir / "base_sha"
        base_sha_file.write_text(base_ref_sha, encoding="utf-8")
        registry_data["artifact://base_sha"] = str(base_sha_file)
    registry_file = artifact_dir.parent / "registry.json"
    registry_file.write_text(json.dumps(registry_data))
    # Create placeholder artifact files so file resolvers find them.
    (artifact_dir / "spec.md").write_text("", encoding="utf-8")
    (artifact_dir / "plan.md").write_text("# Plan\nDo stuff.\n", encoding="utf-8")
    (artifact_dir / "pr-title.txt").write_text("Fake PR Title\n")
    (artifact_dir / "pr-body.md").write_text("Fake PR body.\n")
    (artifact_dir / "pr-url.txt").write_text(
        f"https://github.com/owner/repo/pull/{fake_pr_number}\n"
    )
    (artifact_dir / "pr-branch.txt").write_text("issue-42-fake-slug\n")
    (artifact_dir / "pr-number.txt").write_text(f"{fake_pr_number}\n")
    (artifact_dir / "plan-source-issue-number.txt").write_text("42")
    (artifact_dir / "pr-title.txt").write_text("Fake PR Title\n")
    (artifact_dir / "pr-body.md").write_text("Fake PR body.\n")
    (artifact_dir / "diff-summary.txt").write_text("Files changed:\n  test.txt | 1 +\n")
    (artifact_dir / "pr-base-ref.txt").write_text("main\n")
    (artifact_dir / "repo.txt").write_text("owner/repo\n")

    from _gremlins_core.stages import _set_exec_shell_hook
    from _gremlins_core.utils import proc as _proc_mod

    _orig_shell = _proc_mod.run_shell_async

    async def _noop_gh_shell(cmd, *, cwd=None, env=None, timeout=None):
        env = env or {}
        # Resolve GREMLINS_ARTIFACT_DIR from env for path matching
        artifact_dir_resolved = env.get("GREMLINS_ARTIFACT_DIR") or os.environ.get(
            "GREMLINS_ARTIFACT_DIR", ""
        )
        cmd_resolved = (
            cmd.replace("$GREMLINS_ARTIFACT_DIR", artifact_dir_resolved)
            if isinstance(cmd, str)
            else cmd
        )
        handled = _try_handle_known_gh_cmd(cmd, cmd_resolved, fake_pr_number)
        if handled is not None:
            return handled
        return await _orig_shell(cmd, cwd=cwd, env=env, timeout=timeout)

    _set_exec_shell_hook(_noop_gh_shell)
    monkeypatch.setattr(
        "gremlins.executor.state.resolve_state_file", lambda gremlin_id=None: state_file
    )

    return artifact_dir, state_file


def _prepare_for_plan_stage(tmp_path: pathlib.Path) -> None:
    """Remove plan so skip_if_exists does not skip the plan stage.

    Also clears plan.md so bootstrap's plan?: cli_out doesn't re-bind it.
    """
    reg_path = tmp_path / "scratch" / "gr-test" / "registry.json"
    reg = json.loads(reg_path.read_text())
    reg.pop("artifact://plan.md", None)
    reg_path.write_text(json.dumps(reg))
    plan_md = tmp_path / "scratch" / "gr-test" / "artifacts" / "plan.md"
    if plan_md.exists():
        plan_md.write_text("", encoding="utf-8")


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
def test_parse_resume_from_commit():
    args = _parse_gh_args(["--resume-from", "commit"])
    assert args.resume_from == "commit"


def test_gh_pipeline_stage_names():
    from conftest import PIPELINE_FIXTURES_DIR

    pipeline = Pipeline.from_yaml(PIPELINE_FIXTURES_DIR / "gh.yaml")
    names = [s.name for s in pipeline.stages]
    assert names == [
        "plan",
        "resolve-plan-source",
        "publish-as-issue",
        "set-description",
        "implement",
        "git-commit",
        "require-impl-progress",
        "normalize",
        "verify-check",
        "verify-test",
        "open-pr",
        "diff-summary",
        "compose-pr",
        "push-and-open",
        "github-discover-repo",
        "github-request-copilot-review",
        "fetch-pr-diff",
        "github-review-pull-request",
        "github-discover-repo-2",
        "github-wait-copilot",
        "gather-github-review-content",
        "github-address-pull-request-reviews",
        "ci-gate",
    ]


# ---------------------------------------------------------------------------
# gh_main — smoke test: --plan issue-ref mode (plan stage skipped)
# ---------------------------------------------------------------------------


class _CommittingClient(FakeClient):
    """FakeClient that creates a git commit when the implement label runs.

    Also writes plan.md when the plan label runs so verify_produced passes for
    the plan recipe's out: { plan_file: file://session/plan.md } binding.
    """

    def __init__(
        self,
        *args,
        git_dir: pathlib.Path = None,
        artifact_dir: pathlib.Path = None,
        **kwargs,
    ):
        super().__init__(*args, **kwargs)
        self._git_dir = git_dir
        self._artifact_dir = artifact_dir

    def run(self, prompt, *, label, **kwargs):
        if label == "plan" and self._artifact_dir is not None:
            # The prompt contains {plan} substituted with the absolute
            # artifact path. Extract it so we write to the path the agent
            # was told to use.
            plan_md = None
            ad = re.escape(str(self._artifact_dir))
            m = re.search(ad + r"/[a-f0-9]*plan\.md", prompt)
            if m:
                plan_md = pathlib.Path(m.group(0))
            m2 = re.search(ad + r"/plan\.md", prompt)
            if m2:
                plan_md = pathlib.Path(m2.group(0))
            if plan_md is None:
                # Fall back to any quoted plan.md path in the prompt.
                m3 = re.search(r"`([^`]+plan\.md)`", prompt)
                if m3:
                    plan_md = pathlib.Path(m3.group(1))
            if plan_md is None:
                # Last resort: write to the canonical artifact dir path.
                plan_md = self._artifact_dir / "plan.md"
            if not plan_md.exists() or plan_md.stat().st_size == 0:
                plan_md.parent.mkdir(parents=True, exist_ok=True)
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

    artifact_dir, state_file = _patch_common(monkeypatch, tmp_path)

    # Pre-populate plan.md and plan-issue-number.txt (simulating what
    # resolve-plan-input and publish-as-issue do in production).
    (artifact_dir / "plan.md").write_text("# Plan\nDo stuff.\n", encoding="utf-8")
    (artifact_dir / "plan-issue-number.txt").write_text("42", encoding="utf-8")

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
        artifact_dir=artifact_dir,
        fixtures={
            "plan": MINIMAL_EVENTS,
            "implement": IMPL_EVENTS,
            "compose-pr": MINIMAL_EVENTS,
            "github-review-pull-request": MINIMAL_EVENTS,
            "github-address-pull-request-reviews": MINIMAL_EVENTS,
        },
    )

    result = asyncio.run(
        run_pipeline(_gh_pipeline_path(), argv=[], gremlin_id="gr-test", client=client)
    )
    assert result == 0

    labels = [c.label for c in client.calls]
    assert "implement" in labels


def test_plan_skip_if_exists_on_resume(tmp_path, monkeypatch):
    """Resume: plan stage skipped when plan artifact is already verified."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    artifact_dir, state_file = _patch_common(monkeypatch, tmp_path)
    # Overwrite with non-empty content so verified("plan") is True.
    (artifact_dir / "plan.md").write_text("# Plan\nDo stuff.\n", encoding="utf-8")

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
        artifact_dir=artifact_dir,
        fixtures={
            "implement": IMPL_EVENTS,
            "compose-pr": MINIMAL_EVENTS,
            "github-review-pull-request": MINIMAL_EVENTS,
            "github-address-pull-request-reviews": MINIMAL_EVENTS,
        },
    )

    result = asyncio.run(
        run_pipeline(_gh_pipeline_path(), argv=[], gremlin_id="gr-test", client=client)
    )
    assert result == 0
    labels = [c.label for c in client.calls]
    assert "plan" not in labels
    assert "implement" in labels


def test_publish_as_issue_skip_when_source_bound(tmp_path, monkeypatch):
    """publish-as-issue skipped when plan-source-issue-number is bound.

    When --plan #XYZ is passed at launch, the resolve-plan-source stage
    fetches the issue body and binds plan-source-issue-number. Both the
    plan agent (skip_if_exists: plan) and publish-as-issue (skip_if_exists:
    plan-source-issue-number) are skipped — no new GitHub issue is created.
    """
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    artifact_dir, state_file = _patch_common(monkeypatch, tmp_path)

    # _patch_common already sets plan-source-issue-number in registry + file
    # and writes plan.md — both skips fire automatically.

    shell_cmds: list[str] = []
    from _gremlins_core.stages import _set_exec_shell_hook
    from _gremlins_core.utils import proc as _proc_mod

    _orig_shell = _proc_mod.run_shell_async

    async def _recording_shell(cmd, **kwargs):
        if isinstance(cmd, str):
            shell_cmds.append(cmd)
        # Try to handle known commands before falling through.
        env = kwargs.get("env") or {}
        artifact_dir_resolved = env.get("GREMLINS_ARTIFACT_DIR") or os.environ.get(
            "GREMLINS_ARTIFACT_DIR", ""
        )
        cmd_resolved = (
            cmd.replace("$GREMLINS_ARTIFACT_DIR", artifact_dir_resolved)
            if isinstance(cmd, str)
            else cmd
        )
        handled = _try_handle_known_gh_cmd(cmd, cmd_resolved)
        if handled is not None:
            return handled
        return await _orig_shell(cmd, **kwargs)

    _set_exec_shell_hook(_recording_shell)

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
        artifact_dir=artifact_dir,
        fixtures={
            "implement": IMPL_EVENTS,
            "compose-pr": MINIMAL_EVENTS,
            "github-review-pull-request": MINIMAL_EVENTS,
            "github-address-pull-request-reviews": MINIMAL_EVENTS,
        },
    )

    result = asyncio.run(
        run_pipeline(_gh_pipeline_path(), argv=[], gremlin_id="gr-test", client=client)
    )
    assert result == 0
    assert not any("gh issue create" in cmd for cmd in shell_cmds)
    assert "implement" in [c.label for c in client.calls]


def test_plan_no_h1_issue_body(tmp_path, monkeypatch):
    """The bootstrap prepends an H1 when the fetched issue body lacks one."""
    import os

    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    # Fake gh binary
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

    from gremlins.executor.bootstrap import run_bootstrap

    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir(parents=True, exist_ok=True)

    bootstrap_cmds = [
        f'gh issue view "#42" --json body --jq .body > "{artifact_dir}/plan.md"',
        f'''if ! head -1 "{artifact_dir}/plan.md" | grep -q '^# '; then
          title=$(gh issue view "#42" --json title --jq .title)
          printf '# %s\n\n' "$title" | cat - "{artifact_dir}/plan.md" > "{artifact_dir}/plan.md.tmp"
          mv "{artifact_dir}/plan.md.tmp" "{artifact_dir}/plan.md"
        fi''',
        f'gh issue view "#42" --json number --jq .number | tr -d \'\\n\' > "{artifact_dir}/plan-source-issue-number.txt"',
    ]

    async def _run():
        for cmd in bootstrap_cmds:
            await run_bootstrap([cmd], tmp_path)

    asyncio.run(_run())

    plan_content = (artifact_dir / "plan.md").read_text(encoding="utf-8")
    assert plan_content.startswith("# ")
    assert (artifact_dir / "plan.md").stat().st_size > 0
    issue_num = (artifact_dir / "plan-source-issue-number.txt").read_text(
        encoding="utf-8"
    )
    assert issue_num == "42", f"expected '42', got {issue_num!r}"


def test_plan_stage_uses_bundled_prompt_not_slash_command(tmp_path, monkeypatch):
    """Plan stage builds a real prompt from the bundled ghplan.md, not /ghplan."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    artifact_dir, state_file = _patch_common(monkeypatch, tmp_path)
    _prepare_for_plan_stage(tmp_path)

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
        artifact_dir=artifact_dir,
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
        run_pipeline(_gh_pipeline_path(), argv=[], gremlin_id="gr-test", client=client)
    )
    assert result == 0

    plan_call = next(c for c in client.calls if c.label == "plan")
    assert not plan_call.prompt.startswith("/ghplan")
    assert "/ghplan" not in plan_call.prompt


def test_model_forwarded_to_all_stages(tmp_path, monkeypatch):
    """Injected client.model is forwarded to every client.run call."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    artifact_dir, state_file = _patch_common(monkeypatch, tmp_path)

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
        artifact_dir=artifact_dir,
        model="gpt-4o",
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
            _gh_pipeline_path(),
            argv=[],
            gremlin_id="gr-test",
            client=client,
        )
    )
    assert result == 0

    for call in client.calls:
        assert call.model == "gpt-4o", f"stage {call.label!r} got model={call.model!r}"


def test_gh_main_defaults_to_pipeline_model(tmp_path, monkeypatch):
    """Regression: ghgremlin must default to the pipeline's default_client
    (xai:grok-4), not fall through to the provider's runtime default (which
    may silently run every stage on a different model).
    """
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    artifact_dir, state_file = _patch_common(monkeypatch, tmp_path)

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
        artifact_dir=artifact_dir,
        model="grok-4",
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
        run_pipeline(_gh_pipeline_path(), argv=[], gremlin_id="gr-test", client=client)
    )
    assert result == 0

    # Every recorded run must have model == the cmd template (sonnet is
    # embedded in the command). Asserting on every call (not just calls[0])
    # catches the case where one stage is fixed but another is overlooked.
    assert client.calls, "expected at least one client call"
    expected_model = "grok-4"
    bad = [c for c in client.calls if c.model != expected_model]
    assert not bad, (
        f"{len(bad)} stage(s) ran on a non-sonnet model: "
        f"{[(c.label, c.model) for c in bad]}"
    )


def test_gh_main_client_specifier_model(tmp_path, monkeypatch):
    """Model from --client provider:model flows into all stage run() calls."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    artifact_dir, state_file = _patch_common(monkeypatch, tmp_path)

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
        artifact_dir=artifact_dir,
        model="gpt-4o",
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
            _gh_pipeline_path(),
            argv=[],
            gremlin_id="gr-test",
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
    """--resume-from implement reads plan.md from artifact_dir and runs implement onward."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    state_data = {
        "issue_url": "https://github.com/owner/repo/issues/99",
        "issue_num": "99",
    }
    artifact_dir, state_file = _patch_common(
        monkeypatch, tmp_path, state_data=state_data
    )
    (artifact_dir / "plan.md").write_text(
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
        artifact_dir=artifact_dir,
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
            _gh_pipeline_path(),
            argv=["--resume-from", "implement"],
            gremlin_id="gr-test",
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

    artifact_dir, state_file = _patch_common(
        monkeypatch, tmp_path, fake_pr_number="200"
    )

    # Pre-populate pr-url.txt and diff.txt and register them so the review
    # stage can resolve them (fetch-pr-diff and push-and-open are skipped on
    # resume, so nothing binds these on this run).
    (artifact_dir / "pr-url.txt").write_text("https://github.com/owner/repo/pull/200\n")
    (artifact_dir / "diff.txt").write_text("diff --git a/f b/f\n")
    registry_path = tmp_path / "scratch" / "gr-test" / "registry.json"
    reg = json.loads(registry_path.read_text())
    reg["artifact://pr-url.txt"] = "file://session/pr-url.txt"
    reg["artifact://diff.txt"] = "file://session/diff.txt"
    registry_path.write_text(json.dumps(reg))

    data = json.loads(state_file.read_text())
    data["issue_url"] = "https://github.com/owner/repo/issues/5"
    state_file.write_text(json.dumps(data))

    monkeypatch.setattr(
        "gremlins.stages.loop.LoopStage.run", _async(lambda self, pipe: None)
    )

    monkeypatch.setattr(subprocess, "run", _make_gh_subprocess())

    client = FakeClient(
        fixtures={
            "github-review-pull-request": MINIMAL_EVENTS,
            "github-address-pull-request-reviews": MINIMAL_EVENTS,
        }
    )

    result = asyncio.run(
        run_pipeline(
            _gh_pipeline_path(),
            argv=["--resume-from", "github-review-pull-request"],
            gremlin_id="gr-test",
            client=client,
        )
    )
    assert result == 0

    review_calls = [c for c in client.calls if c.label == "github-review-pull-request"]
    assert len(review_calls) == 1
    assert "https://github.com/owner/repo/pull/200" in review_calls[0].prompt


def test_plan_file_path_includes_plan_title_cost_in_total(tmp_path, monkeypatch):
    """Plan stage cost is aggregated into the persisted total_cost_usd.

    Regression guard for #157 (missing plan-title cost) and #164 (plan-title
    moved to stream-json mode for cost capture). Reads total_cost_usd from the
    on-disk state.json to verify the persistence step at gh.py:471-473.
    """
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    artifact_dir, state_file = _patch_common(monkeypatch, tmp_path)
    _prepare_for_plan_stage(tmp_path)

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
        artifact_dir=artifact_dir,
        fixtures=fixtures,
    )
    result = asyncio.run(
        run_pipeline(_gh_pipeline_path(), argv=[], gremlin_id="gr-test", client=client)
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


def test_parse_resume_from_open_pr():
    args = _parse_gh_args(["--resume-from", "open-pr"])
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

    artifact_dir, state_file = _patch_common(monkeypatch, tmp_path)

    # plan-source-issue-number.txt is bound by resolve-plan-source, which is
    # skipped on this resume — register it directly so compose-pr resolves it.
    registry_path = tmp_path / "scratch" / "gr-test" / "registry.json"
    reg = json.loads(registry_path.read_text())
    reg["artifact://plan-source-issue-number.txt"] = (
        "file://session/plan-source-issue-number.txt"
    )
    registry_path.write_text(json.dumps(reg))

    data = json.loads(state_file.read_text())
    data["issue_url"] = "https://github.com/owner/repo/issues/42"
    state_file.write_text(json.dumps(data))

    monkeypatch.setattr(subprocess, "run", _make_gh_subprocess())
    monkeypatch.setattr(
        "gremlins.stages.loop.LoopStage.run", _async(lambda self, pipe: None)
    )

    client = FakeClient(
        fixtures={
            "compose-pr": MINIMAL_EVENTS,
            "github-review-pull-request": MINIMAL_EVENTS,
            "github-address-pull-request-reviews": MINIMAL_EVENTS,
        }
    )

    result = asyncio.run(
        run_pipeline(
            _gh_pipeline_path(),
            argv=["--resume-from", "open-pr"],
            gremlin_id="gr-test",
            client=client,
        )
    )
    assert result == 0

    labels = [c.label for c in client.calls]
    assert "implement" not in labels, "implement must not run on open-pr resume"
    assert "compose-pr" in labels

    compose_pr_call = next(c for c in client.calls if c.label == "compose-pr")
    assert "42" in compose_pr_call.prompt, (
        "compose-pr must receive issue number from plan-source-issue-number, not a gh:// URI"
    )

    review_calls = [c for c in client.calls if c.label == "github-review-pull-request"]
    assert len(review_calls) == 1
    assert "https://github.com/owner/repo/pull/101" in review_calls[0].prompt
    # Verify push-and-open wrote pr to registry.json
    registry_path = tmp_path / "scratch" / "gr-test" / "registry.json"
    assert registry_path.exists(), "registry.json should have been written"
    assert (
        json.loads(registry_path.read_text()).get("artifact://pr-url.txt") is not None
    )


# ---------------------------------------------------------------------------
# github-wait-copilot stage: argument wiring
# ---------------------------------------------------------------------------


def test_github_wait_copilot_stage_argument_wiring(tmp_path, monkeypatch):
    """github-wait-copilot loop receives repo and artifact_dir; pr_url is written to state by GitHubOpenPullRequest."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    artifact_dir, state_file = _patch_common(monkeypatch, tmp_path, fake_pr_number="77")

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
        artifact_dir=artifact_dir,
        model="gpt-4o",
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
            _gh_pipeline_path(),
            argv=[],
            gremlin_id="gr-test",
            client=client,
        )
    )
    assert result == 0

    assert "github-wait-copilot" in captured_stages
    _, copilot_state = captured_stages["github-wait-copilot"]
    assert copilot_state.artifact_dir == artifact_dir
    # pr is written to registry.json by push-and-open
    registry_path = tmp_path / "scratch" / "gr-test" / "registry.json"
    assert registry_path.exists(), "registry.json should have been written"
    assert (
        json.loads(registry_path.read_text()).get("artifact://pr-url.txt") is not None
    )


# ---------------------------------------------------------------------------
# ci-gate stage: argument wiring, ordering, and resume behavior
# ---------------------------------------------------------------------------


def test_github_wait_ci_stage_argument_wiring(tmp_path, monkeypatch):
    """ci-gate LoopStage receives model and artifact_dir; pr_url is written to state by GitHubOpenPullRequest."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    artifact_dir, state_file = _patch_common(monkeypatch, tmp_path, fake_pr_number="77")

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
        artifact_dir=artifact_dir,
        model="gpt-4o",
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
            _gh_pipeline_path(),
            argv=[],
            gremlin_id="gr-test",
            client=client,
        )
    )
    assert result == 0

    stage = captured_stage["stage"]
    assert stage.client.model == "gpt-4o"
    assert captured_stage["state"].artifact_dir == artifact_dir
    # pr is written to registry.json by push-and-open
    registry_path = tmp_path / "scratch" / "gr-test" / "registry.json"
    assert registry_path.exists(), "registry.json should have been written"
    assert (
        json.loads(registry_path.read_text()).get("artifact://pr-url.txt") is not None
    )


def test_github_wait_ci_stage_ordering(tmp_path, monkeypatch):
    """ci-gate runs after github-address-pull-request-reviews and exactly once."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    artifact_dir, _state_file = _patch_common(monkeypatch, tmp_path)

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
        artifact_dir=artifact_dir,
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
        run_pipeline(_gh_pipeline_path(), argv=[], gremlin_id="gr-test", client=client)
    )
    assert result == 0

    assert order[0] == "verify-check", "verify must run before other tracked stages"
    assert order[-1] == "ci-gate"
    assert order.count("verify-check") == 1
    assert order.count("verify-test") == 1
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

    _artifact_dir, state_file = _patch_common(monkeypatch, tmp_path)

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

    client = FakeClient(fixtures={})

    result = asyncio.run(
        run_pipeline(
            _gh_pipeline_path(),
            argv=["--resume-from", "ci-gate"],
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
    """verify.run receives fix_model, cwd via options, artifact_dir via ctx."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    artifact_dir, _state_file = _patch_common(
        monkeypatch, tmp_path, fake_pr_number="77"
    )

    monkeypatch.setattr(
        subprocess,
        "run",
        _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n"),
    )

    captured_stage = {}

    async def record_verify(self, state):
        if self.name == "verify-check":
            captured_stage["stage"] = self
            captured_stage["state"] = state

    monkeypatch.setattr("gremlins.stages.loop.LoopStage.run", record_verify)

    client = _CommittingClient(
        git_dir=tmp_path,
        artifact_dir=artifact_dir,
        model="gpt-4o",
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
            _gh_pipeline_path(),
            argv=[],
            gremlin_id="gr-test",
            client=client,
        )
    )
    assert result == 0

    stage = captured_stage["stage"]
    assert stage.client.model == "gpt-4o"
    # cmds are on the cmd exec stage inside the loop body; first cmd is the user cmd
    cmd_stage = stage.body[0]
    assert "make check" in cmd_stage.options.get("cmds")[0]
    assert "printf 'done'" in cmd_stage.options.get("cmds")[0]
    assert captured_stage["state"].artifact_dir == artifact_dir


def test_resume_from_verify(tmp_path, monkeypatch):
    """--resume-from verify-check skips plan and implement, runs verify onward."""
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

    _artifact_dir, state_file = _patch_common(monkeypatch, tmp_path)

    data = json.loads(state_file.read_text())
    data["issue_url"] = "https://github.com/owner/repo/issues/5"
    state_file.write_text(json.dumps(data))

    verify_calls = []

    monkeypatch.setattr(
        "gremlins.stages.loop.LoopStage.run",
        _async(lambda self, pipe: verify_calls.append(self)),
    )

    monkeypatch.setattr(subprocess, "run", _make_gh_subprocess())

    client = FakeClient(
        fixtures={
            "compose-pr": MINIMAL_EVENTS,
            "github-review-pull-request": MINIMAL_EVENTS,
            "github-address-pull-request-reviews": MINIMAL_EVENTS,
        }
    )

    result = asyncio.run(
        run_pipeline(
            _gh_pipeline_path(),
            argv=["--resume-from", "verify-check"],
            gremlin_id="gr-test",
            client=client,
        )
    )
    assert result == 0

    labels = [c.label for c in client.calls]
    assert "implement" not in labels, "implement must not run on verify resume"
    assert sum(1 for s in verify_calls if s.name == "verify-check") == 1


def test_gh_main_writes_stage_to_state(tmp_path, monkeypatch):
    """set_stage writes the stage name to the state file threaded through State."""
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    artifact_dir, state_file = _patch_common(monkeypatch, tmp_path)

    monkeypatch.setattr(
        subprocess, "run", _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n")
    )
    monkeypatch.setattr(
        "gremlins.stages.loop.LoopStage.run", _async(lambda self, pipe: None)
    )

    client = _CommittingClient(
        git_dir=tmp_path,
        artifact_dir=artifact_dir,
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
            _gh_pipeline_path(),
            argv=[],
            gremlin_id="gr-test",
            client=client,
        )
    )
    assert result == 0

    data = json.loads(state_file.read_text())
    assert data.get("stage") == "ci-gate"


def test_gh_main_state_client_tracks_effective_model(tmp_path, monkeypatch):
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    artifact_dir, state_file = _patch_common(monkeypatch, tmp_path)

    monkeypatch.setattr(
        subprocess, "run", _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n")
    )
    monkeypatch.setattr(
        "gremlins.stages.loop.LoopStage.run", _async(lambda self, pipe: None)
    )

    client = _CommittingClient(
        git_dir=tmp_path,
        artifact_dir=artifact_dir,
        model="gpt-4o",
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
            _gh_pipeline_path(),
            argv=[],
            gremlin_id="gr-test",
            client=client,
        )
    )
    assert result == 0

    data = json.loads(state_file.read_text())
    assert "model" not in data


def test_gh_main_pipeline_default_client_model(tmp_path, monkeypatch):
    """pipeline.default_client model used when --client is absent.

    Regression: the model was extracted only from --model / --client, not from
    the pipeline's default_client. A pipeline with default_client: openai:gpt-4o
    produced model=sonnet, causing the Copilot client to fail immediately.
    """
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    artifact_dir, state_file = _patch_common(monkeypatch, tmp_path)

    # Override Pipeline.from_yaml to inject default_client: openai:gpt-4o and
    # re-fill stage clients so every stage inherits that model.
    from _gremlins_core.clients import RustClient as Client
    from _gremlins_core.schemas import fill_stage_clients

    _real_from_yaml = Pipeline.from_yaml

    def _strip_clients_2(stage):
        stage.client = None
        for child in getattr(stage, "body", []):
            _strip_clients_2(child)

    def _from_yaml_copilot_default(path, **kwargs):
        pipeline = _real_from_yaml(path, **kwargs)
        new_default = Client("openai", "gpt-4o")
        for s in pipeline.stages:
            _strip_clients_2(s)
        fill_stage_clients(pipeline.stages, new_default)
        pipeline.default_client = new_default
        return pipeline

    monkeypatch.setattr(
        "_gremlins_core.schemas.Pipeline.from_yaml", _from_yaml_copilot_default
    )

    monkeypatch.setattr(
        subprocess, "run", _make_gh_subprocess(issue_body="# Plan\nDo stuff.\n")
    )
    monkeypatch.setattr(
        "gremlins.stages.loop.LoopStage.run", _async(lambda self, pipe: None)
    )

    client = _CommittingClient(
        git_dir=tmp_path,
        artifact_dir=artifact_dir,
        model="gpt-4o",
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
        run_pipeline(_gh_pipeline_path(), argv=[], gremlin_id="gr-test", client=client)
    )
    assert result == 0

    assert client.calls, "expected at least one client call"
    bad = [c for c in client.calls if c.model != "gpt-4o"]
    assert not bad, (
        f"{len(bad)} stage(s) used wrong model: {[(c.label, c.model) for c in bad]}"
    )


def test_publish_as_issue_runs_when_no_source_bound(tmp_path, monkeypatch):
    """publish-as-issue runs when plan-source-issue-number is absent.

    When no --plan is passed at launch, the resolve-plan-source stage
    has no issue ref to resolve, so plan-source-issue-number is never
    bound. publish-as-issue sees no skip guard, runs gh issue create,
    and writes plan-issue-number.txt.
    """
    _init_git_repo(tmp_path)
    monkeypatch.chdir(tmp_path)

    artifact_dir, state_file = _patch_common(monkeypatch, tmp_path)

    # Remove plan-source-issue-number so publish-as-issue doesn't skip.
    (artifact_dir / "plan-source-issue-number.txt").unlink(missing_ok=True)
    registry_path = tmp_path / "scratch" / "gr-test" / "registry.json"
    reg = json.loads(registry_path.read_text())
    reg.pop("artifact://plan-source-issue-number.txt", None)
    registry_path.write_text(json.dumps(reg))

    # Remove plan.md so the plan agent runs instead of skipping.
    (artifact_dir / "plan.md").unlink(missing_ok=True)
    reg.pop("artifact://plan.md", None)
    registry_path.write_text(json.dumps(reg))

    shell_cmds: list[str] = []
    from _gremlins_core.stages import _set_exec_shell_hook
    from _gremlins_core.utils import proc as _proc_mod

    _orig_shell = _proc_mod.run_shell_async

    async def _recording_shell(cmd, **kwargs):
        if isinstance(cmd, str):
            shell_cmds.append(cmd)
        # Try to handle known commands before falling through.
        env = kwargs.get("env") or {}
        artifact_dir_resolved = env.get("GREMLINS_ARTIFACT_DIR") or os.environ.get(
            "GREMLINS_ARTIFACT_DIR", ""
        )
        cmd_resolved = (
            cmd.replace("$GREMLINS_ARTIFACT_DIR", artifact_dir_resolved)
            if isinstance(cmd, str)
            else cmd
        )
        handled = _try_handle_known_gh_cmd(cmd, cmd_resolved)
        if handled is not None:
            return handled
        return await _orig_shell(cmd, **kwargs)

    _set_exec_shell_hook(_recording_shell)

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
        artifact_dir=artifact_dir,
        fixtures={
            "plan": MINIMAL_EVENTS,
            "implement": IMPL_EVENTS,
            "compose-pr": MINIMAL_EVENTS,
            "github-review-pull-request": MINIMAL_EVENTS,
            "github-address-pull-request-reviews": MINIMAL_EVENTS,
        },
    )

    result = asyncio.run(
        run_pipeline(_gh_pipeline_path(), argv=[], gremlin_id="gr-test", client=client)
    )
    assert result == 0

    assert any("gh_publish_issue" in cmd for cmd in shell_cmds), (
        "publish-as-issue should have run gh_publish_issue"
    )
    assert (artifact_dir / "plan-issue-number.txt").exists(), (
        "publish-as-issue should have written plan-issue-number.txt"
    )
    assert "plan" in [c.label for c in client.calls]
