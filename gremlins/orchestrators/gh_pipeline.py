"""GitHub pipeline orchestrator."""

from __future__ import annotations

import argparse
import json
import logging
import pathlib
import re
import shutil
import subprocess
from collections.abc import Callable

from gremlins.clients import ClientSpec
from gremlins.clients.protocol import ClaudeClient
from gremlins.gh_utils import parse_issue_ref, view_issue
from gremlins.git import DirtyOnly, HeadAdvanced, PreImplState, record_pre_impl_state
from gremlins.orchestrators.base import (
    Pipeline,
    die,
    read_stage_inputs,
    read_state_field,
)
from gremlins.pipeline import Pipeline as _PipelineData
from gremlins.pipeline import StageEntry
from gremlins.stages import (
    address_code,
    commit,
    commit_pr,
    implement,
    open_github_pr,
    plan,
    request_copilot,
    review_code,
    verify,
    wait_ci,
    wait_copilot,
)
from gremlins.stages import handoff_branch as handoff_branch_mod
from gremlins.stages.base import Stage, StageContext
from gremlins.stages.handoff_branch import HandoffBranchResult
from gremlins.state import patch_state, set_stage

logger = logging.getLogger(__name__)


def _update_description_from_plan(
    plan_md: pathlib.Path,
    state_file: pathlib.Path | None,
    gr_id: str | None = None,
    *,
    issue_title: str = "",
) -> None:
    if state_file is None or not state_file.exists():
        return
    try:
        data = json.loads(state_file.read_text(encoding="utf-8"))
        if data.get("description_explicit"):
            return
        if issue_title:
            patch_state(gr_id, description=issue_title[:60])
            return
        lines = plan_md.read_text(encoding="utf-8").splitlines()[:50]
        h1 = ""
        for line in lines:
            m = re.match(r"^#\s+(.+)", line)
            if m:
                h1 = m.group(1)[:60]
                break
        if h1:
            patch_state(gr_id, description=h1)
    except Exception:
        pass


class GHPipeline(Pipeline):
    target = "github"
    STAGE_TYPES: dict[str, type[Stage]] = {
        "plan": plan.Plan,
        "implement": implement.Implement,
        "handoff-branch": handoff_branch_mod.HandoffBranch,
        "verify": verify.Verify,
        "commit": commit.Commit,
        "commit-pr": commit_pr.CommitPR,
        "open-github-pr": open_github_pr.OpenGitHubPR,
        "request-copilot": request_copilot.RequestCopilot,
        "ghreview": review_code.ReviewCode,
        "ghaddress": address_code.AddressCode,
        "wait-ci": wait_ci.WaitCI,
        "wait-copilot": wait_copilot.WaitCopilot,
    }

    def __init__(
        self,
        stages: list[StageEntry],
        *,
        args: argparse.Namespace,
        session_dir: pathlib.Path,
        gr_id: str | None,
        pipeline_data: _PipelineData,
        repo: str,
        state_file: pathlib.Path | None,
        stage_specs: dict[str, ClientSpec] | None = None,
        spec_clients: dict[str, ClaudeClient] | None = None,
        test_client: ClaudeClient | None = None,
    ) -> None:
        super().__init__(
            stages,
            args=args,
            session_dir=session_dir,
            gr_id=gr_id,
            pipeline_data=pipeline_data,
            stage_specs=stage_specs,
            spec_clients=spec_clients,
            test_client=test_client,
        )
        self.repo = repo
        self.state_file = state_file
        self.instructions: str = read_stage_inputs(state_file).get(
            "instructions"
        ) or " ".join(getattr(args, "instructions", None) or [])
        self.issue_url: str = ""
        self.issue_num: str = ""
        self.issue_body: str = ""
        self.impl_pre_state: PreImplState | None = None
        self.impl_handoff_result: HandoffBranchResult | None = None
        self.impl_handoff_branch: str = ""
        self.impl_base_ref: str = ""
        self.pr_url: str = ""
        self.pr_num: str = ""

    def _ensure_pr_url(self) -> None:
        if self.pr_url:
            return
        saved = read_state_field(self.state_file, "pr_url")
        if not saved:
            die(
                f"--resume-from {self.args.resume_from}: no pr_url in state.json "
                "(rewind to implement?)"
            )
        self.pr_url = saved
        self.pr_num = saved.split("/")[-1]
        logger.info("resumed PR: %s", saved)

    def _do_plan_source(
        self,
        plan_src: str,
        plan_md: pathlib.Path,
        client: ClaudeClient,
        model: str | None,
    ) -> None:
        """Resolve --plan (file or issue ref) and populate self.issue_url/num/body."""
        issue_title = ""
        issue_url = ""
        issue_num = ""
        if pathlib.Path(plan_src).is_file():
            src = pathlib.Path(plan_src)
            if src.stat().st_size == 0:
                die(f"--plan: file is empty: {plan_src}")
            issue_body = src.read_text(encoding="utf-8")
            logger.info(
                "[1/8] plan supplied via --plan (file): %s — posting as GitHub issue",
                plan_src,
            )
            title_prompt = (
                "Produce a concise GitHub issue title (under 80 characters) "
                "summarizing the spec below. Output ONLY the title, nothing else."
                f"\n\n{issue_body}"
            )
            completed = client.run(title_prompt, label="plan-title", model=model)
            parts = (completed.text_result or "").strip().splitlines()
            issue_title = parts[0][:80] if parts else ""
            if not issue_title:
                die("--plan: title agent returned empty output")
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
                    plan_src,
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            if r.returncode != 0:
                die(f"--plan: failed to create GitHub issue: {r.stderr.strip()}")
            create_out = r.stdout + r.stderr
            m = re.search(r"https://github\.com/[^ )]+/issues/[0-9]+", create_out)
            if not m:
                die(
                    f"--plan: could not extract issue URL from gh output: {create_out.strip()}"
                )
            issue_url = m.group(0)
            issue_num = issue_url.split("/")[-1]
            logger.info("issue: %s", issue_url)
            shutil.copyfile(src, plan_md)
        else:
            target_repo, issue_ref = parse_issue_ref(plan_src, self.repo)
            if target_repo is None or issue_ref is None:
                die(
                    f"--plan: not a readable file or recognized issue reference: {plan_src}"
                )
            try:
                issue_data = view_issue(issue_ref, target_repo)
            except RuntimeError as exc:
                die(f"--plan: {exc}")
            issue_body = issue_data.get("body") or ""
            if not issue_body:
                die(f"--plan: issue {plan_src} has an empty body")
            resolved_url = issue_data.get("url") or ""
            resolved_num = str(issue_data.get("number") or "")
            issue_title = (issue_data.get("title") or "")[:60]
            if target_repo == self.repo:
                issue_url = resolved_url
                issue_num = resolved_num
            else:
                issue_url = ""
                issue_num = ""
            plan_md.write_text(issue_body + "\n", encoding="utf-8")
            logger.info(
                "[1/8] plan supplied via --plan (issue %s#%s)", target_repo, issue_ref
            )
        self.issue_url = issue_url
        self.issue_num = issue_num
        self.issue_body = issue_body
        patch_state(self.gr_id, issue_url=issue_url, issue_num=issue_num)
        _update_description_from_plan(
            plan_md, self.state_file, gr_id=self.gr_id, issue_title=issue_title
        )

    def _make_runner(
        self, entry: StageEntry, ctx: StageContext, spec: ClientSpec
    ) -> Callable[[], None]:
        model = spec.model
        if entry.type == "plan":

            def _plan() -> None:
                plan_md = self.session_dir / "plan.md"

                if plan_md.exists() and plan_md.stat().st_size > 0:
                    self.issue_url = read_state_field(self.state_file, "issue_url")
                    self.issue_num = read_state_field(self.state_file, "issue_num")
                    self.issue_body = plan_md.read_text(encoding="utf-8")
                    label = f" (issue #{self.issue_num})" if self.issue_num else ""
                    logger.info(
                        "[1/8] plan resumed from snapshot: %s%s", plan_md, label
                    )
                    return

                if self.args.plan_source:
                    set_stage(self.gr_id, entry.name)
                    self._do_plan_source(
                        self.args.plan_source,
                        plan_md,
                        self._get_client(spec),
                        model,
                    )
                    return

                if not entry.prompt_paths:
                    die(
                        f"stage {entry.name!r}: type 'plan' requires a 'prompt' field in the pipeline YAML"
                    )
                set_stage(self.gr_id, entry.name)
                logger.info("[1/8] running plan")
                stage = plan.Plan(
                    entry,
                    model,
                    ref=self.args.ref or "",
                    instructions=self.instructions,
                    repo=self.repo,
                )
                stage.bind(ctx)
                stage.run(self)

            return _plan

        if entry.type == "implement":

            def _implement() -> None:
                set_stage(self.gr_id, entry.name)
                logger.info("[2a/8] implementing plan")
                spec_file = self.session_dir / "spec.md"
                spec_text = ""
                if spec_file.exists():
                    try:
                        spec_text = spec_file.read_text(encoding="utf-8")
                    except (OSError, UnicodeDecodeError) as exc:
                        logger.warning(
                            "could not read spec.md (%s); proceeding without north-star context",
                            exc,
                        )
                self.impl_pre_state = record_pre_impl_state()
                patch_state(
                    self.gr_id,
                    impl_pre_head=self.impl_pre_state.head,
                    impl_pre_branch=self.impl_pre_state.branch,
                )
                stage = implement.Implement(
                    entry,
                    model,
                    plan_text=self.issue_body,
                    is_git=True,
                    spec_text=spec_text,
                )
                stage.bind(ctx)
                stage.run(self)

            return _implement

        if entry.type == "handoff-branch":

            def _handoff_branch() -> None:
                set_stage(self.gr_id, entry.name)
                logger.info("[2b/8] creating handoff branch")
                if self.impl_pre_state is None:
                    saved_head = read_state_field(self.state_file, "impl_pre_head")
                    if saved_head:
                        self.impl_pre_state = PreImplState(
                            head=saved_head,
                            branch=read_state_field(self.state_file, "impl_pre_branch"),
                        )
                stage = handoff_branch_mod.HandoffBranch(entry, model)
                stage.bind(ctx)
                result = stage.run(self)
                self.impl_handoff_result = result
                self.impl_handoff_branch = result.handoff_branch
                self.impl_base_ref = result.base_ref
                patch_state(
                    self.gr_id,
                    impl_handoff_branch=result.handoff_branch,
                    impl_base_ref=result.base_ref,
                )

            return _handoff_branch

        if entry.type == "verify":

            def _verify() -> None:
                set_stage(self.gr_id, entry.name)
                logger.info("[2c/8] verifying implementation")
                stage = verify.Verify(entry, model, is_git=True, commit_after_fix=False)
                stage.bind(ctx)
                stage.run(None)

            return _verify

        if entry.type == "commit":

            def _commit() -> None:
                set_stage(self.gr_id, entry.name)
                logger.info("[2d/8] committing changes")
                hb_result = self.impl_handoff_result
                if hb_result is not None:
                    impl_outcome = hb_result.outcome
                    impl_handoff_branch = hb_result.handoff_branch
                    base_ref = hb_result.base_ref
                else:
                    impl_handoff_branch = read_state_field(
                        self.state_file, "impl_handoff_branch"
                    )
                    base_ref = read_state_field(self.state_file, "impl_base_ref")
                    if not base_ref:
                        die(
                            "--resume-from commit: no impl_base_ref in state.json "
                            "(rewind to implement?)"
                        )
                    if impl_handoff_branch:
                        count_r = subprocess.run(
                            [
                                "git",
                                "rev-list",
                                "--count",
                                f"{base_ref}..{impl_handoff_branch}",
                            ],
                            capture_output=True,
                            text=True,
                            check=False,
                        )
                        if count_r.returncode != 0:
                            die(
                                f"--resume-from commit: impl_handoff_branch '{impl_handoff_branch}' "
                                f"not found or base_ref invalid (rewind to implement?)\n"
                                f"{count_r.stderr.strip()}"
                            )
                        commit_count = int(count_r.stdout.strip())
                        impl_outcome = HeadAdvanced(commit_count=commit_count)
                    else:
                        impl_outcome = DirtyOnly()
                stage = commit.Commit(
                    entry,
                    model,
                    impl_outcome=impl_outcome,
                    impl_handoff_branch=impl_handoff_branch,
                    base_ref=base_ref,
                    issue_url=self.issue_url,
                    cwd=None,
                )
                stage.bind(ctx)
                stage.run(None)

            return _commit

        if entry.type == "open-github-pr":

            def _open_github_pr() -> None:
                set_stage(self.gr_id, entry.name)
                logger.info("[2e/8] opening GitHub PR")
                stage = open_github_pr.OpenGitHubPR(
                    entry,
                    model,
                    issue_url=self.issue_url,
                )
                stage.bind(ctx)
                pr_url = stage.run(None)
                pr_num = pr_url.split("/")[-1]
                logger.info("PR: %s", pr_url)
                patch_state(self.gr_id, pr_url=pr_url)
                self.pr_url = pr_url
                self.pr_num = pr_num

            return _open_github_pr

        if entry.type == "commit-pr":

            def _commit_pr() -> None:
                set_stage(self.gr_id, entry.name)
                logger.info("[2de/8] committing changes and opening PR")
                hb_result = self.impl_handoff_result
                if hb_result is not None:
                    impl_outcome = hb_result.outcome
                    impl_handoff_branch = hb_result.handoff_branch
                    base_ref = hb_result.base_ref
                else:
                    impl_handoff_branch = read_state_field(
                        self.state_file, "impl_handoff_branch"
                    )
                    base_ref = read_state_field(self.state_file, "impl_base_ref")
                    if not base_ref:
                        die(
                            "--resume-from commit-pr: no impl_base_ref in state.json "
                            "(rewind to implement?)"
                        )
                    if impl_handoff_branch:
                        count_r = subprocess.run(
                            [
                                "git",
                                "rev-list",
                                "--count",
                                f"{base_ref}..{impl_handoff_branch}",
                            ],
                            capture_output=True,
                            text=True,
                            check=False,
                        )
                        if count_r.returncode != 0:
                            die(
                                f"--resume-from commit-pr: impl_handoff_branch '{impl_handoff_branch}' "
                                f"not found or base_ref invalid (rewind to implement?)\n"
                                f"{count_r.stderr.strip()}"
                            )
                        commit_count = int(count_r.stdout.strip())
                        impl_outcome = HeadAdvanced(commit_count=commit_count)
                    else:
                        impl_outcome = DirtyOnly()
                commit_stage = commit.Commit(
                    entry,
                    model,
                    impl_outcome=impl_outcome,
                    impl_handoff_branch=impl_handoff_branch,
                    base_ref=base_ref,
                    issue_url=self.issue_url,
                    cwd=None,
                )
                commit_stage.bind(ctx)
                commit_stage.run(None)
                pr_stage = open_github_pr.OpenGitHubPR(
                    entry,
                    model,
                    issue_url=self.issue_url,
                )
                pr_stage.bind(ctx)
                pr_url = pr_stage.run(None)
                pr_num = pr_url.split("/")[-1]
                logger.info("PR: %s", pr_url)
                patch_state(self.gr_id, pr_url=pr_url)
                self.pr_url = pr_url
                self.pr_num = pr_num

            return _commit_pr

        if entry.type == "request-copilot":

            def _request_copilot() -> None:
                self._ensure_pr_url()
                set_stage(self.gr_id, entry.name)
                logger.info("[3/8] requesting Copilot review")
                stage = request_copilot.RequestCopilot(
                    entry, model, repo=self.repo, pr_num=self.pr_num
                )
                stage.bind(ctx)
                stage.run(None)

            return _request_copilot

        if entry.type == "ghreview":

            def _ghreview() -> None:
                if not entry.prompt_paths:
                    die(
                        f"stage {entry.name!r}: type 'ghreview' requires a 'prompt' field in the pipeline YAML"
                    )
                self._ensure_pr_url()
                set_stage(self.gr_id, entry.name)
                logger.info("[4/8] running /ghreview")
                stage = review_code.ReviewCode(
                    entry,
                    model,
                    plan_text="",
                    is_git=True,
                    pr_url=self.pr_url,
                )
                stage.bind(ctx)
                stage.run(self)

            return _ghreview

        if entry.type == "wait-copilot":

            def _wait_copilot() -> None:
                self._ensure_pr_url()
                set_stage(self.gr_id, entry.name)
                logger.info(
                    "[5/8] waiting for Copilot review (20s interval, 10min timeout)"
                )
                stage = wait_copilot.WaitCopilot(
                    entry, model, repo=self.repo, pr_num=self.pr_num
                )
                stage.bind(ctx)
                state = stage.run(None)
                logger.info("Copilot review: %s", state)

            return _wait_copilot

        if entry.type == "ghaddress":

            def _ghaddress() -> None:
                if not entry.prompt_paths:
                    die(
                        f"stage {entry.name!r}: type 'ghaddress' requires a 'prompt' field in the pipeline YAML"
                    )
                self._ensure_pr_url()
                set_stage(self.gr_id, entry.name)
                logger.info("[6/8] running /ghaddress")
                stage = address_code.AddressCode(
                    entry,
                    model,
                    is_git=True,
                    pr_url=self.pr_url,
                )
                stage.bind(ctx)
                stage.run(self)

            return _ghaddress

        if entry.type == "wait-ci":

            def _wait_ci() -> None:
                self._ensure_pr_url()
                set_stage(self.gr_id, entry.name)
                logger.info(
                    "[7/8] waiting for CI checks (up to 3 attempts, 20min each)"
                )
                stage = wait_ci.WaitCI(entry, model, pr_url=self.pr_url)
                stage.bind(ctx)
                stage.run(None)

            return _wait_ci

        raise ValueError(f"unsupported stage type {entry.type!r} in gh pipeline")
