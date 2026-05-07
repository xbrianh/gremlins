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
from gremlins.pipeline import StageEntry
from gremlins.prompts import load_prompts
from gremlins.stages.base import Stage
from gremlins.stages.registry import register_stage
from gremlins.state import patch_state, resolve_state_file

logger = logging.getLogger(__name__)


def _fmt_escape(s: str) -> str:
    return s.replace("{", "{{").replace("}", "}}")


def _read_state_str(state_file: pathlib.Path | None, field: str) -> str:
    if state_file is None or not state_file.exists():
        return ""
    try:
        return json.loads(state_file.read_text(encoding="utf-8")).get(field) or ""
    except Exception:
        return ""


class Plan(Stage):
    def __init__(
        self,
        entry: StageEntry,
        model: str | None,
        *,
        instructions: str = "",
        plan_file: pathlib.Path | None = None,
        plan_source: str | None = None,
        ref: str = "",
        repo: str = "",
    ) -> None:
        super().__init__(entry, model)
        self.instructions = instructions
        self.plan_file = plan_file
        self.plan_source = plan_source
        self.ref = ref
        self.repo = repo

    def run(self, pipe: Any) -> None:
        plan_md = self.plan_file or (self.state.session_dir / "plan.md")

        if plan_md.exists() and plan_md.stat().st_size > 0:
            if pipe is not None:
                state_file = resolve_state_file(self.state.gr_id)
                pipe.issue_url = _read_state_str(state_file, "issue_url")
                pipe.issue_num = _read_state_str(state_file, "issue_num")
                pipe.issue_body = plan_md.read_text(encoding="utf-8")
                label = f" (issue #{pipe.issue_num})" if pipe.issue_num else ""
                logger.info("[1/8] plan resumed from snapshot: %s%s", plan_md, label)
            return

        if self.plan_source:
            src = pathlib.Path(self.plan_source)
            if src.is_file():
                self._resolve_file_source(self.plan_source, plan_md, pipe)
            else:
                self._resolve_issue_source(self.plan_source, plan_md, pipe)
            return

        self._run_agent(plan_md, pipe)

    def _run_agent(self, plan_md: pathlib.Path, pipe: Any) -> None:
        if self.repo:
            plan_prompt = load_prompts(self.prompt_paths).format(
                ref=_fmt_escape(self.ref or ""),
                instructions=_fmt_escape(self.instructions),
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
            pipe.issue_url = issue_url
            pipe.issue_num = issue_num
            pipe.issue_body = issue_body
        else:
            template = load_prompts(self.prompt_paths)
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

    def _resolve_file_source(self, path: str, plan_md: pathlib.Path, pipe: Any) -> None:
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
        if pipe is not None:
            pipe.issue_url = issue_url
            pipe.issue_num = issue_num
            pipe.issue_body = issue_body

    def _resolve_issue_source(self, ref: str, plan_md: pathlib.Path, pipe: Any) -> None:
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
        if pipe is not None:
            pipe.issue_url = issue_url
            pipe.issue_num = issue_num
            pipe.issue_body = issue_body

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
