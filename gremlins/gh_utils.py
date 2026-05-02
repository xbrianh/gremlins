"""GitHub CLI helpers used by the gh orchestrator and gh stages.

All functions that call ``gh`` or parse stream-json events for GitHub URLs
live here so the stage modules stay focused on orchestration.
"""

from __future__ import annotations

import json
import re
import subprocess


def get_repo() -> str:
    """Return the current repo's ``owner/name`` via ``gh repo view``."""
    r = subprocess.run(
        ["gh", "repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner"],
        capture_output=True, text=True, check=False,
    )
    if r.returncode != 0:
        raise RuntimeError(
            f"not in a gh-recognized repo: {r.stderr.strip() or r.stdout.strip()}"
        )
    return r.stdout.strip()


def parse_issue_ref(
    plan_source: str, repo: str
) -> tuple[str | None, str | None]:
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


def view_issue(issue_ref: str, repo: str) -> dict:
    """Fetch ``number``, ``url``, ``body`` for an issue via ``gh issue view``.

    Returns the parsed JSON dict. Raises ``RuntimeError`` when ``gh`` fails,
    times out, or returns unparseable output. The timeout is bounded so a
    hung ``gh`` (network stall, credential prompt) cannot block chain start
    indefinitely.
    """
    try:
        r = subprocess.run(
            [
                "gh", "issue", "view", issue_ref, "--repo", repo,
                "--json", "number,url,body",
            ],
            capture_output=True, text=True, check=False,
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
    events: list[dict],
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
    matching_ids: set = set()
    for evt in events:
        if evt.get("type") != "assistant":
            continue
        for c in (evt.get("message") or {}).get("content") or []:
            if not isinstance(c, dict):
                continue
            if (
                c.get("type") == "tool_use"
                and c.get("name") == "Bash"
                and re.search(cmd_pattern, (c.get("input") or {}).get("command") or "")
            ):
                matching_ids.add(c.get("id"))

    # Scan tool_result events for those IDs.
    last_tool_url: str | None = None
    for evt in events:
        if evt.get("type") != "user":
            continue
        for c in (evt.get("message") or {}).get("content") or []:
            if not isinstance(c, dict) or c.get("type") != "tool_result":
                continue
            if c.get("tool_use_id") not in matching_ids:
                continue
            body = c.get("content")
            if isinstance(body, list):
                text = "\n".join((p.get("text") or "") for p in body if isinstance(p, dict))
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


def check_copilot_review(repo: str, pr_num: str) -> str | None:
    """Return the first non-PENDING Copilot review state, or None if not ready."""
    r = subprocess.run(
        [
            "gh", "api", f"repos/{repo}/pulls/{pr_num}/reviews",
            "--jq", '.[] | select(.user.login | test("[Cc]opilot")) | .state',
        ],
        capture_output=True, text=True, check=False,
    )
    if r.returncode != 0:
        return None
    lines = [ln for ln in r.stdout.splitlines() if ln and ln != "PENDING"]
    return lines[0] if lines else None
