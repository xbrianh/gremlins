"""Open-GitHub-PR stage for the gh pipeline."""

from __future__ import annotations

import json
import logging
import re
from typing import Any, cast

from gremlins.clients.protocol import CompletedRun
from gremlins.gh_utils import extract_gh_url
from gremlins.prompts import BUNDLED_PROMPT_DIR
from gremlins.stages.base import Stage
from gremlins.stages.registry import register_stage
from gremlins.state import patch_state, read_state_str, resolve_state_file

logger = logging.getLogger(__name__)


def _load(name: str) -> str:
    return (BUNDLED_PROMPT_DIR / name).read_text(encoding="utf-8")


class OpenGitHubPR(Stage):
    def __init__(
        self,
        name: str,
        model: str | None,
        prompts: list[str],
        options: dict[str, Any],
        *,
        issue_url: str,
        base_ref: str | None = None,
    ) -> None:
        super().__init__(name, model, prompts, options)
        self.issue_url = issue_url
        self.base_ref = base_ref

    def run(self, pipe: Any) -> str:
        sf = resolve_state_file(self.state.gr_id)
        chain_base_ref = self._prev_child_branch(sf)
        base_ref = (
            chain_base_ref
            or self.base_ref
            or read_state_str(sf, "base_ref_name")
            or "main"
        )

        issue_num = self.issue_url.split("/")[-1] if self.issue_url else ""

        if issue_num:
            closes_clause = f"Include 'Closes #{issue_num}' in the PR body."
        else:
            closes_clause = (
                "Do NOT include any 'Closes #N' or 'Fixes #N' link in the PR body."
            )

        base_prompt = _load("open_github_pr.md").format(base_ref=base_ref).rstrip()
        prompt = f"{base_prompt} {closes_clause}"

        completed: CompletedRun = self.run_claude(
            prompt,
            label="open-github-pr",
            raw_path=self.state.session_dir / "stream-open-github-pr.jsonl",
            capture_events=True,
        )

        pr_url = extract_gh_url(
            completed.events or [],
            url_pattern=r"https://github\.com/[^ )]+/pull/[0-9]+",
            cmd_pattern=r"gh pr create",
            label="PR",
            text_result=completed.text_result,
        )
        self._record_child_pr(sf, pr_url)
        patch_state(self.state.gr_id, pr_url=pr_url)
        logger.info("PR: %s", pr_url)
        return pr_url

    def _prev_child_branch(self, sf: Any) -> str:
        if sf is None or not sf.exists():
            return ""
        try:
            data = json.loads(sf.read_text(encoding="utf-8"))
            chain_st = data.get("chain_state")
            if not isinstance(chain_st, dict):
                return ""
            chain_st = cast(dict[str, Any], chain_st)
            n = int(chain_st.get("handoff_count", 0))
            records: list[dict[str, Any]] = list(chain_st.get("child_records") or [])
            for rec in records:
                if rec.get("n") == n - 1:
                    return str(rec.get("branch") or "")
        except Exception:
            pass
        return ""

    def _record_child_pr(self, sf: Any, pr_url: str) -> None:
        if sf is None or not sf.exists() or not self.state.gr_id:
            return
        try:
            data = json.loads(sf.read_text(encoding="utf-8"))
            chain_st = data.get("chain_state")
            if not isinstance(chain_st, dict):
                return
            chain_st = cast(dict[str, Any], chain_st)
            n = int(chain_st.get("handoff_count", 0))
            m = re.search(r"/pull/(\d+)$", pr_url)
            pr_number = int(m.group(1)) if m else None
            records: list[dict[str, Any]] = list(chain_st.get("child_records") or [])
            for rec in records:
                if rec.get("n") == n:
                    rec["pr_url"] = pr_url
                    if pr_number is not None:
                        rec["pr_number"] = pr_number
                    break
            else:
                new_rec: dict[str, Any] = {"n": n, "pr_url": pr_url}
                if pr_number is not None:
                    new_rec["pr_number"] = pr_number
                records.append(new_rec)
            chain_st["child_records"] = records
            patch_state(self.state.gr_id, chain_state=chain_st)
        except Exception:
            pass


register_stage("open-github-pr", OpenGitHubPR)
