"""GitHub CLI helpers used by the gh orchestrator and gh stages.

All functions that call ``gh`` or parse stream-json events for GitHub URLs
live here so the stage modules stay focused on orchestration.
"""

from __future__ import annotations

import json
import re
import subprocess
from typing import Any, cast


def get_repo() -> str:
    """Return the current repo's ``owner/name`` via ``gh repo view``."""
    r = subprocess.run(
        ["gh", "repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner"],
        capture_output=True,
        text=True,
        check=False,
    )
    if r.returncode != 0:
        raise RuntimeError(
            f"not in a gh-recognized repo: {r.stderr.strip() or r.stdout.strip()}"
        )
    return r.stdout.strip()


def parse_issue_ref(plan_source: str, repo: str) -> tuple[str | None, str | None]:
    """Parse an issue reference string into ``(target_repo, issue_num)``.

    Recognized shapes (matching ghgremlin's --plan contract):
      * ``42`` / ``#42``                              → (repo, "42")
      * ``owner/name#42``                             → ("owner/name", "42")
      * ``https://github.com/owner/name/issues/42``   → ("owner/name", "42")

    Returns ``(None, None)`` when ``plan_source`` doesn't match any shape so
    the caller can distinguish issue refs from local file paths.
    """
    m = re.match(r"^#?([0-9]+)$", plan_source)
    if m:
        return repo, m.group(1)
    m = re.match(r"^([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+)#([0-9]+)$", plan_source)
    if m:
        return m.group(1), m.group(2)
    m = re.match(
        r"^https://github\.com/([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+)/issues/([0-9]+)(#.*)?$",
        plan_source,
    )
    if m:
        return m.group(1), m.group(2)
    return None, None


VIEW_ISSUE_TIMEOUT = 30  # seconds; bounds `gh issue view` shell-out
GET_PR_CI_STATUS_TIMEOUT = 30  # seconds; bounds `gh pr view` shell-out in poll loop
GET_REQUIRED_CHECK_NAMES_TIMEOUT = (
    30  # seconds; bounds `gh pr checks --required` shell-out
)


def view_issue(issue_ref: str, repo: str) -> dict[str, Any]:
    """Fetch ``number``, ``url``, ``body`` for an issue via ``gh issue view``.

    Returns the parsed JSON dict. Raises ``RuntimeError`` when ``gh`` fails,
    times out, or returns unparseable output. The timeout is bounded so a
    hung ``gh`` (network stall, credential prompt) cannot block chain start
    indefinitely.
    """
    try:
        r = subprocess.run(
            [
                "gh",
                "issue",
                "view",
                issue_ref,
                "--repo",
                repo,
                "--json",
                "number,url,body",
            ],
            capture_output=True,
            text=True,
            check=False,
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

    raise RuntimeError(f"failed to extract {label} URL from claude output events")


def get_required_check_names(pr_url: str) -> set[str]:
    """Return the set of required check names for a PR via branch protection.

    Shells out to ``gh pr checks <pr_url> --required --json name``. Returns an
    empty set when there are no required checks or branch protection is off
    (``gh`` exits 0 with an empty array). Raises ``RuntimeError`` on timeout,
    non-zero exit, or unparseable JSON.
    """
    try:
        r = subprocess.run(
            ["gh", "pr", "checks", pr_url, "--required", "--json", "name"],
            capture_output=True,
            text=True,
            check=False,
            timeout=GET_REQUIRED_CHECK_NAMES_TIMEOUT,
        )
    except subprocess.TimeoutExpired as exc:
        raise RuntimeError(
            f"timed out after {GET_REQUIRED_CHECK_NAMES_TIMEOUT}s fetching required "
            f"check names for {pr_url!r} via `gh pr checks`; check GitHub CLI "
            f"authentication and network connectivity"
        ) from exc
    if r.returncode != 0:
        raise RuntimeError(
            f"could not fetch required check names for {pr_url!r}: {r.stderr.strip()}"
        )
    try:
        data = json.loads(r.stdout)
    except json.JSONDecodeError as exc:
        raise RuntimeError(
            f"could not parse required check names response: {exc}"
        ) from exc
    return {entry["name"] for entry in data if "name" in entry}


def get_pr_ci_status(pr_url: str) -> dict[str, Any]:
    """Return CI check status and review decision for a PR.

    Returns dict with:
    - 'checks': list of required check objects from statusCheckRollup (may be empty)
    - 'review_decision': reviewDecision string (e.g. 'REVIEW_REQUIRED', 'APPROVED', '')

    Checks are filtered to only those listed as required by branch protection. When no
    required checks are configured the list is empty and the ci-gate stage skips.
    """
    try:
        r = subprocess.run(
            ["gh", "pr", "view", pr_url, "--json", "statusCheckRollup,reviewDecision"],
            capture_output=True,
            text=True,
            check=False,
            timeout=GET_PR_CI_STATUS_TIMEOUT,
        )
    except subprocess.TimeoutExpired as exc:
        raise RuntimeError(
            f"timed out after {GET_PR_CI_STATUS_TIMEOUT}s fetching CI status for "
            f"{pr_url!r} via `gh pr view`; check GitHub CLI authentication and network"
        ) from exc
    if r.returncode != 0:
        raise RuntimeError(f"could not fetch PR CI status: {r.stderr.strip()}")
    try:
        data = json.loads(r.stdout)
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"could not parse PR CI status response: {exc}") from exc
    all_checks = data.get("statusCheckRollup") or []
    required_names = get_required_check_names(pr_url)
    if required_names:
        checks = [
            c
            for c in all_checks
            if (c.get("name") or c.get("context")) in required_names
        ]
        if not checks:
            # Required checks are configured but none have started reporting yet.
            # Return a synthetic pending entry so the poller keeps waiting rather
            # than treating the empty list as "no checks / done".
            checks = [
                {
                    "__typename": "CheckRun",
                    "name": "__required_pending__",
                    "status": "IN_PROGRESS",
                }
            ]
    else:
        checks = []
    return {
        "checks": checks,
        "review_decision": data.get("reviewDecision") or "",
    }


def fetch_check_run_logs(details_url: str) -> str:
    """Try to fetch failed-step logs for a GitHub Actions check run.

    Extracts the workflow run ID from `details_url` and calls
    `gh run view <id> --log-failed`. Returns empty string when the URL
    doesn't match Actions or the gh call fails.
    """
    m = re.search(r"/actions/runs/(\d+)", details_url or "")
    if not m:
        return ""
    run_id = m.group(1)
    r = subprocess.run(
        ["gh", "run", "view", run_id, "--log-failed"],
        capture_output=True,
        text=True,
        check=False,
        timeout=30,
    )
    if r.returncode == 0:
        return r.stdout.strip()[:10000]
    return ""


def check_copilot_review(repo: str, pr_num: str) -> str | None:
    """Return the first non-PENDING Copilot review state, or None if not ready."""
    r = subprocess.run(
        [
            "gh",
            "api",
            f"repos/{repo}/pulls/{pr_num}/reviews",
            "--jq",
            '.[] | select(.user.login | test("[Cc]opilot")) | .state',
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    if r.returncode != 0:
        return None
    lines = [ln for ln in r.stdout.splitlines() if ln and ln != "PENDING"]
    return lines[0] if lines else None
