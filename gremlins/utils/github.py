from __future__ import annotations

import json
import re
import subprocess
from typing import Any, cast

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
GET_PR_CI_STATUS_TIMEOUT = 30  # seconds; bounds `gh pr view` shell-out in poll loop
VIEW_PR_TIMEOUT = 30  # seconds; bounds `gh pr view` shell-out


def view_pr(pr: str, *, project_root: str | None = None) -> dict[str, Any]:
    """Fetch url and headRefName for a PR via gh pr view."""
    try:
        r = proc.run(
            ["gh", "pr", "view", pr, "--json", "url,headRefName"],
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


def extract_gh_url(
    events: list[dict[str, Any]],
    url_pattern: str,
    cmd_pattern: str,
    label: str,
    text_result: str | None = None,
) -> str:
    """Extract a GitHub URL from a claude stream-json event list.

    Searches ``Bash`` tool_use events whose ``command`` matches ``cmd_pattern``
    (regex), finds their paired ``tool_result`` events, and returns the last
    URL matching ``url_pattern`` found in those results. Falls back to scanning
    the final ``result`` event's text if no tool_result match is found.

    Raises ``RuntimeError`` when no URL is found.
    """
    # Collect tool_use IDs for Bash commands matching cmd_pattern.
    matching_ids: set[str] = set()
    for evt in events:
        if evt.get("type") != "assistant":
            continue
        msg = cast(dict[str, Any], evt.get("message") or {})
        for c in cast(list[dict[str, Any]], msg.get("content") or []):
            inp = cast(dict[str, Any], c.get("input") or {})
            if (
                c.get("type") == "tool_use"
                and c.get("name") == "Bash"
                and re.search(cmd_pattern, str(inp.get("command") or ""))
            ):
                matching_ids.add(str(c.get("id") or ""))

    # Scan tool_result events for those IDs.
    last_tool_url: str | None = None
    for evt in events:
        if evt.get("type") != "user":
            continue
        msg = cast(dict[str, Any], evt.get("message") or {})
        for c in cast(list[dict[str, Any]], msg.get("content") or []):
            if c.get("type") != "tool_result":
                continue
            if c.get("tool_use_id") not in matching_ids:
                continue
            body = c.get("content")
            if isinstance(body, list):
                text = "\n".join(
                    str(cast(dict[str, Any], p).get("text") or "")
                    for p in cast(list[Any], body)
                    if isinstance(p, dict)
                )
            elif isinstance(body, str):
                text = body
            else:
                text = str(body) if body is not None else ""
            matches = re.findall(url_pattern, text)
            if matches:
                last_tool_url = matches[-1]

    if last_tool_url:
        return last_tool_url

    # Fallback: scan the last result event's text.
    for evt in reversed(events):
        if evt.get("type") == "result":
            result_text = evt.get("result") or ""
            matches = re.findall(url_pattern, result_text)
            if matches:
                return matches[-1]

    if text_result:
        matches = re.findall(url_pattern, text_result)
        if matches:
            return matches[-1]

    raise RuntimeError(f"failed to extract {label} URL from claude output events")


def parse_ci_status_response(stdout: str) -> dict[str, Any]:
    try:
        data = json.loads(stdout)
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"could not parse PR CI status response: {exc}") from exc
    return {
        "checks": cast(list[dict[str, Any]], data.get("statusCheckRollup") or []),
        "review_decision": data.get("reviewDecision") or "",
        "head_sha": data.get("headRefOid") or "",
    }


async def get_pr_ci_status_async(pr_url: str) -> dict[str, Any]:
    try:
        r = await proc.run_async(
            [
                "gh",
                "pr",
                "view",
                pr_url,
                "--json",
                "statusCheckRollup,reviewDecision,headRefOid",
            ],
            timeout=GET_PR_CI_STATUS_TIMEOUT,
        )
    except subprocess.TimeoutExpired as exc:
        raise RuntimeError(
            f"timed out after {GET_PR_CI_STATUS_TIMEOUT}s fetching CI status for "
            f"{pr_url!r} via `gh pr view`; check GitHub CLI authentication and network"
        ) from exc
    if r.returncode != 0:
        raise RuntimeError(f"could not fetch PR CI status: {r.stderr.strip()}")
    return parse_ci_status_response(r.stdout)


async def fetch_check_run_logs_async(details_url: str) -> str:
    m = re.search(r"/actions/runs/(\d+)", details_url or "")
    if not m:
        return ""
    run_id = m.group(1)
    r = await proc.run_async(["gh", "run", "view", run_id, "--log-failed"], timeout=30)
    if r.returncode == 0:
        return r.stdout.strip()[:10000]
    return ""


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


async def check_copilot_review_async(repo: str, pr_num: str) -> str | None:
    r = await proc.run_async(
        [
            "gh",
            "api",
            f"repos/{repo}/pulls/{pr_num}/reviews",
            "--jq",
            '.[] | select(.user.login | test("[Cc]opilot")) | .state',
        ],
    )
    if r.returncode != 0:
        raise RuntimeError(
            f"gh api reviews failed (exit {r.returncode}): {r.stderr.strip() or '(no stderr)'}"
        )
    lines = [ln for ln in r.stdout.splitlines() if ln and ln != "PENDING"]
    return lines[0] if lines else None
