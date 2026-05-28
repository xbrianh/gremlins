from __future__ import annotations

import json
import re
import subprocess
from typing import Any

from gremlins.utils import proc


def get_repo(*, timeout: float | None = 10) -> str:
    """Return the current repo's ``owner/name`` via ``gh repo view``."""
    r = proc.run(
        ["gh", "repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner"],
        timeout=timeout,
    )
    if r.returncode != 0:
        raise RuntimeError(
            f"not in a gh-recognized repo: {r.stderr.strip() or r.stdout.strip()}"
        )
    return r.stdout.strip()


def current_repo() -> str:
    """Return ``owner/name`` of the current repo, or '' on any error."""
    try:
        return get_repo()
    except (RuntimeError, subprocess.SubprocessError, OSError):
        return ""


def fetch_issue(plan: str) -> dict[str, Any] | None:
    """Resolve an issue-ref plan arg to its ``gh issue view`` JSON dict."""
    try:
        target_repo, issue_ref = parse_issue_ref(plan, "")
    except Exception:
        return None
    if issue_ref is None:
        return None
    if not target_repo:
        target_repo = current_repo()
    if not target_repo:
        return None
    try:
        return view_issue(issue_ref, target_repo)
    except Exception:
        return None


def parse_issue_ref(plan_source: str, repo: str) -> tuple[str | None, str | None]:
    """Parse an issue reference string into ``(target_repo, issue_num)``.

    Recognized shapes:
      * ``#42``                  → (repo, "42")
      * ``owner/name#42``        → ("owner/name", "42")

    Returns ``(None, None)`` for anything else (file paths, bare integers, URLs).
    The ``#`` prefix is the disambiguator that distinguishes an issue ref from a
    file path or bare integer.
    """
    m = re.match(r"^#([0-9]+)$", plan_source)
    if m:
        return repo, m.group(1)
    m = re.match(r"^([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+)#([0-9]+)$", plan_source)
    if m:
        return m.group(1), m.group(2)
    return None, None


VIEW_ISSUE_TIMEOUT = 30  # seconds; bounds `gh issue view` shell-out
VIEW_PR_TIMEOUT = 30  # seconds; bounds `gh pr view` shell-out


def view_pr(pr: str, *, project_root: str | None = None) -> dict[str, Any]:
    """Fetch url and headRefName for a PR via gh pr view."""
    try:
        r = proc.run(
            ["gh", "pr", "view", pr, "--json", "url,number,headRefName"],
            timeout=VIEW_PR_TIMEOUT,
            cwd=project_root,
        )
    except subprocess.TimeoutExpired as exc:
        raise RuntimeError(
            f"timed out after {VIEW_PR_TIMEOUT}s while resolving PR {pr!r} via `gh pr view`"
        ) from exc
    if r.returncode != 0:
        msg = r.stderr.strip() or r.stdout.strip()
        raise RuntimeError(f"gh pr view failed for {pr!r}: {msg}")
    try:
        return json.loads(r.stdout)
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"gh pr view returned invalid JSON for {pr!r}") from exc


def view_issue(issue_ref: str, repo: str) -> dict[str, Any]:
    """Fetch ``number``, ``url``, ``body`` for an issue via ``gh issue view``.

    Returns the parsed JSON dict. Raises ``RuntimeError`` when ``gh`` fails,
    times out, or returns unparseable output. The timeout is bounded so a
    hung ``gh`` (network stall, credential prompt) cannot block chain start
    indefinitely.
    """
    try:
        r = proc.run(
            [
                "gh",
                "issue",
                "view",
                issue_ref,
                "--repo",
                repo,
                "--json",
                "number,url,body,title",
            ],
            timeout=VIEW_ISSUE_TIMEOUT,
        )
    except subprocess.TimeoutExpired as exc:
        raise RuntimeError(
            f"timed out after {VIEW_ISSUE_TIMEOUT}s while resolving issue "
            f"{issue_ref!r} in {repo!r} via `gh issue view`; check GitHub "
            f"CLI authentication, prompts, and network connectivity"
        ) from exc
    if r.returncode != 0:
        raise RuntimeError(
            f"could not resolve issue {issue_ref!r} in {repo!r}: {r.stderr.strip()}"
        )
    try:
        return json.loads(r.stdout)
    except json.JSONDecodeError as exc:
        raise RuntimeError(
            f"gh issue view returned invalid JSON for {issue_ref!r} in {repo!r}. "
            f"stdout: {r.stdout.strip()} stderr: {r.stderr.strip()}"
        ) from exc


def resolve_default_branch(project_root: str) -> str:
    """Resolve origin's default branch via gh CLI. Raises RuntimeError on failure."""
    try:
        r = proc.run(
            [
                "gh",
                "repo",
                "view",
                "--json",
                "defaultBranchRef",
                "-q",
                ".defaultBranchRef.name",
            ],
            cwd=project_root,
            timeout=30,
        )
    except FileNotFoundError:
        raise RuntimeError("gh CLI not found on PATH")
    except subprocess.TimeoutExpired:
        raise RuntimeError("gh repo view timed out after 30s")
    if r.returncode != 0 or not r.stdout.strip():
        raise RuntimeError(f"gh repo view failed: {r.stderr.strip() or 'empty output'}")
    return r.stdout.strip()
