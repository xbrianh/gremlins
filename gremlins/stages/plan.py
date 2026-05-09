"""Plan stage (local and GitHub)."""

from __future__ import annotations

import json
import logging
import pathlib
import re
import shutil
import subprocess
import sys
from typing import Any

from gremlins.gh_utils import extract_gh_url, get_repo, parse_issue_ref, view_issue
from gremlins.stages.base import Stage, StageInput
from gremlins.stages.registry import register_stage
from gremlins.state import patch_state, read_state_str, resolve_state_file

logger = logging.getLogger(__name__)


def _fmt_escape(s: str) -> str:
    return s.replace("{", "{{").replace("}", "}}")


class Plan(Stage):
    def __init__(
        self,
        name: str,
        model: str | None,
        prompts: list[str],
        options: dict[str, Any],
        *,
        instructions: str = "",
        plan: str | None = None,
        repo: str = "",
    ) -> None:
        super().__init__(name, model, prompts, options)
        self.instructions = instructions
        self.plan = plan
        self.repo = repo

    @classmethod
    def orchestration_args(cls) -> list[StageInput]:
        return [
            StageInput("instructions", str, required=False, default="", help="extra instructions for the planning agent"),
            StageInput("plan", str, required=False, default=None, help="path to a plan file or GitHub issue ref (owner/repo#N or #N)"),
            StageInput("repo", str, required=False, default="", help="GitHub repo (owner/name) to operate on"),
        ]

    def run(self, pipe: Any) -> None:
        plan_md = self.state.session_dir / "plan.md"

        if plan_md.exists() and plan_md.stat().st_size > 0:
            state_file = resolve_state_file(self.state.gr_id)
            issue_num = read_state_str(state_file, "issue_num")
            label = f" (issue #{issue_num})" if issue_num else ""
            logger.info("[1/8] plan resumed from snapshot: %s%s", plan_md, label)
            return

        if self.plan:
            src = pathlib.Path(self.plan)
            if src.is_file():
                self._resolve_file_source(self.plan, plan_md)
            else:
                self._resolve_issue_source(self.plan, plan_md)
            return

        self._run_agent(plan_md)

    def _run_agent(self, plan_md: pathlib.Path) -> None:
        if self.repo:
            state_file = resolve_state_file(self.state.gr_id)
            base_ref_name = read_state_str(state_file, "base_ref_name")
            plan_prompt = (
                "\n\n".join(self.prompts)
                .rstrip()
                .format(
                    base_ref=_fmt_escape(base_ref_name),
                    instructions=_fmt_escape(self.instructions),
                )
            )
            completed = self.run_claude(
                plan_prompt,
                label="plan",
                raw_path=self.state.session_dir / "ghplan-out.jsonl",
                capture_events=True,
            )
            issue_url = extract_gh_url(
                completed.events or [],
                url_pattern=r"https://github\.com/[^ )]+/issues/[0-9]+",
                cmd_pattern=r"gh issue create",
                label="issue",
                text_result=completed.text_result,
            )
            issue_num = issue_url.split("/")[-1]
            logger.info("issue: %s", issue_url)
            patch_state(self.state.gr_id, issue_url=issue_url, issue_num=issue_num)
            issue_body = _fetch_issue_body(issue_num, self.repo)
            plan_md.write_text(issue_body, encoding="utf-8")
        else:
            template = "\n\n".join(self.prompts).rstrip()
            prompt = template.format(
                plan_file=plan_md,
                instructions=self.instructions,
            )
            completed = self.run_claude(
                prompt,
                label="plan",
                raw_path=self.state.session_dir / "stream-plan.jsonl",
            )
            if not plan_md.exists() or plan_md.stat().st_size == 0:
                snippet = (completed.text_result or "")[:200].strip()
                detail = f"; model said: {snippet}" if snippet else ""
                raise RuntimeError(f"plan stage did not produce {plan_md}{detail}")

    def _resolve_file_source(self, path: str, plan_md: pathlib.Path) -> None:
        src = pathlib.Path(path)
        if src.stat().st_size == 0:
            sys.stderr.write(f"error: --plan: file is empty: {path}\n")
            sys.stderr.flush()
            sys.exit(1)
        issue_body = src.read_text(encoding="utf-8")

        if not self.repo:
            shutil.copyfile(src, plan_md)
            return

        logger.info(
            "[1/8] plan supplied via --plan (file): %s — posting as GitHub issue", path
        )
        title_prompt = (
            "Produce a concise GitHub issue title (under 80 characters) "
            "summarizing the spec below. Output ONLY the title, nothing else."
            f"\n\n{issue_body}"
        )
        completed = self.run_claude(
            title_prompt,
            label="plan-title",
            raw_path=self.state.session_dir / "plan-title.jsonl",
        )
        parts = (completed.text_result or "").strip().splitlines()
        issue_title = parts[0][:80] if parts else ""
        if not issue_title:
            sys.stderr.write("error: --plan: title agent returned empty output\n")
            sys.stderr.flush()
            sys.exit(1)
        r = subprocess.run(
            [
                "gh",
                "issue",
                "create",
                "--repo",
                self.repo,
                "--title",
                issue_title,
                "--body-file",
                path,
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        if r.returncode != 0:
            sys.stderr.write(
                f"error: --plan: failed to create GitHub issue: {r.stderr.strip()}\n"
            )
            sys.stderr.flush()
            sys.exit(1)
        create_out = r.stdout + r.stderr
        m = re.search(r"https://github\.com/[^ )]+/issues/[0-9]+", create_out)
        if not m:
            sys.stderr.write(
                f"error: --plan: could not extract issue URL from gh output: {create_out.strip()}\n"
            )
            sys.stderr.flush()
            sys.exit(1)
        issue_url = m.group(0)
        issue_num = issue_url.split("/")[-1]
        logger.info("issue: %s", issue_url)
        shutil.copyfile(src, plan_md)
        patch_state(self.state.gr_id, issue_url=issue_url, issue_num=issue_num)
        self._update_description(plan_md, issue_title=issue_title)

    def _resolve_issue_source(self, ref: str, plan_md: pathlib.Path) -> None:
        target_repo, issue_ref = parse_issue_ref(ref, self.repo or "")
        if issue_ref is None:
            sys.stderr.write(
                f"error: --plan: not a readable file or recognized issue reference: {ref}\n"
            )
            sys.stderr.flush()
            sys.exit(1)
        if not target_repo:
            try:
                target_repo = get_repo()
            except RuntimeError as exc:
                sys.stderr.write(f"error: --plan: could not resolve repo: {exc}\n")
                sys.stderr.flush()
                sys.exit(1)
        try:
            issue_data = view_issue(issue_ref, target_repo)
        except RuntimeError as exc:
            sys.stderr.write(f"error: --plan: {exc}\n")
            sys.stderr.flush()
            sys.exit(1)
        issue_body = issue_data.get("body") or ""
        if not issue_body:
            sys.stderr.write(f"error: --plan: issue {ref} has an empty body\n")
            sys.stderr.flush()
            sys.exit(1)
        resolved_url = issue_data.get("url") or ""
        resolved_num = str(issue_data.get("number") or "")
        issue_title = (issue_data.get("title") or "")[:60]
        plan_md.write_text(issue_body + "\n", encoding="utf-8")
        if self.repo and target_repo == self.repo:
            issue_url = resolved_url
            issue_num = resolved_num
        else:
            issue_url = ""
            issue_num = ""
        logger.info(
            "[1/8] plan supplied via --plan (issue %s#%s)", target_repo, issue_ref
        )
        patch_state(self.state.gr_id, issue_url=issue_url, issue_num=issue_num)
        self._update_description(plan_md, issue_title=issue_title)

    def _update_description(
        self, plan_md: pathlib.Path, *, issue_title: str = ""
    ) -> None:
        state_file = resolve_state_file(self.state.gr_id)
        if state_file is None or not state_file.exists():
            return
        try:
            data = json.loads(state_file.read_text(encoding="utf-8"))
            if data.get("description_explicit"):
                return
            if issue_title:
                patch_state(self.state.gr_id, description=issue_title[:60])
                return
            lines = plan_md.read_text(encoding="utf-8").splitlines()[:50]
            h1 = ""
            for line in lines:
                m = re.match(r"^#\s+(.+)", line)
                if m:
                    h1 = m.group(1)[:60]
                    break
            if h1:
                patch_state(self.state.gr_id, description=h1)
        except Exception:
            pass


def _fetch_issue_body(issue_num: str, repo: str) -> str:
    try:
        issue_data = view_issue(issue_num, repo)
    except RuntimeError as exc:
        sys.stderr.write(f"error: could not fetch issue #{issue_num} body: {exc}\n")
        sys.stderr.flush()
        sys.exit(1)
    body = (issue_data.get("body") or "").strip()
    if not body:
        sys.stderr.write(f"error: issue #{issue_num} has an empty body\n")
        sys.stderr.flush()
        sys.exit(1)
    return body


register_stage("plan", Plan)
