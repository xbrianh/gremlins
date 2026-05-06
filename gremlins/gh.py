"""Helpers that shell out to the gh CLI (not git)."""

import subprocess


def resolve_default_branch(project_root: str) -> str:
    """Resolve origin's default branch via gh CLI. Raises RuntimeError on failure."""
    try:
        r = subprocess.run(
            [
                "gh",
                "repo",
                "view",
                "--json",
                "defaultBranchRef",
                "-q",
                ".defaultBranchRef.name",
            ],
            capture_output=True,
            text=True,
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
