"""Pipeline orchestrator classes."""

from __future__ import annotations

import argparse
import dataclasses
import json
import logging
import pathlib
import shutil
import subprocess
import sys
from collections.abc import Callable, Iterator
from typing import NoReturn

from gremlins.clients import ClientSpec
from gremlins.clients.protocol import ClaudeClient
from gremlins.clients.resolve import require_stage_spec
from gremlins.git import DirtyOnly, HeadAdvanced, in_git_repo
from gremlins.pipeline import Pipeline as _PipelineData
from gremlins.pipeline import StageEntry
from gremlins.runner import build_parallel_stages, install_signal_handlers, run_stages
from gremlins.stages import (
    address_code,
    commit,
    ghaddress,
    ghplan,
    ghreview,
    implement,
    open_github_pr,
    plan,
    request_copilot,
    review_code,
    verify,
    wait_ci,
    wait_copilot,
)
from gremlins.stages.base import Stage, StageContext
from gremlins.stages.implement import ImplStageResult
from gremlins.state import patch_state, set_stage

logger = logging.getLogger(__name__)


def _die(msg: str) -> NoReturn:
    sys.stderr.write(f"error: {msg}\n")
    sys.stderr.flush()
    sys.exit(1)


def read_state_field(sf: pathlib.Path | None, field: str) -> str:
    if sf is None or not sf.exists():
        return ""
    try:
        data = json.loads(sf.read_text(encoding="utf-8"))
        return data.get(field) or ""
    except Exception:
        return ""


def _expand_stage_entries(raw_stages: list[StageEntry]) -> list[StageEntry]:
    top_level_names = {e.name for e in raw_stages}
    child_names: set[str] = set()
    seen: set[str] = set()
    result: list[StageEntry] = []

    for entry in raw_stages:
        if entry.type == "parallel":
            for child in entry.children:
                if child.name in child_names or child.name in top_level_names:
                    raise ValueError(f"duplicate child stage name {child.name!r}")
                child_names.add(child.name)
            for name, typ in [
                (f"{entry.name}-fanout", "parallel-fanout"),
                (entry.name, "parallel-group"),
                (f"{entry.name}-fanin", "parallel-fanin"),
            ]:
                if name in seen:
                    raise ValueError(f"pipeline has duplicate stage name {name!r}")
                seen.add(name)
                result.append(dataclasses.replace(entry, name=name, type=typ))
        else:
            if entry.name in seen:
                raise ValueError(f"pipeline has duplicate stage name {entry.name!r}")
            seen.add(entry.name)
            result.append(entry)

    return result


class Pipeline:
    STAGE_TYPES: dict[str, type[Stage]] = {}
    target: str = ""

    def __init__(
        self,
        stages: list[StageEntry],
        *,
        args: argparse.Namespace,
        session_dir: pathlib.Path,
        gr_id: str | None,
        pipeline_data: _PipelineData,
        stage_specs: dict[str, ClientSpec] | None = None,
        spec_clients: dict[str, ClaudeClient] | None = None,
        test_client: ClaudeClient | None = None,
    ) -> None:
        if self.STAGE_TYPES:
            unknown: list[str] = []
            for s in stages:
                if s.type == "parallel":
                    unknown.extend(
                        c.type for c in s.children if c.type not in self.STAGE_TYPES
                    )
                elif s.type not in self.STAGE_TYPES:
                    unknown.append(s.type)
            if unknown:
                raise ValueError(
                    f"{type(self).__name__} does not support stage type(s): {unknown}"
                )
        self.stages = _expand_stage_entries(stages)
        self.args = args
        self.session_dir = session_dir
        self.gr_id = gr_id
        self.is_git = in_git_repo()
        self.pipeline_data = pipeline_data
        self.stage_specs: dict[str, ClientSpec] = stage_specs or {}
        self.spec_clients: dict[str, ClaudeClient] = spec_clients or {}
        self.test_client = test_client

    def _get_client(self, spec: ClientSpec) -> ClaudeClient:
        if self.test_client is not None:
            return self.test_client
        return self.spec_clients[str(spec)]

    def validate_resume_target(self) -> None:
        resume_from = getattr(self.args, "resume_from", None)
        if not resume_from:
            return
        valid_names = [entry.name for entry in self.stages]
        if resume_from not in valid_names:
            raise ValueError(
                f"--resume-from {resume_from!r} is not a valid stage; "
                f"valid: {valid_names}"
            )

    def _make_runner(
        self, entry: StageEntry, ctx: StageContext, model: str
    ) -> Callable[[], None]:
        raise NotImplementedError

    def run(self, *clients: ClaudeClient) -> None:
        install_signal_handlers(*clients)

        gr_id = self.gr_id
        stages: list[tuple[str, Callable[[], None]]] = []
        for e in self.pipeline_data.stages:
            if e.type == "parallel":
                group_dir = self.session_dir / e.name
                group_dir.mkdir(parents=True, exist_ok=True)
                child_runners: list[tuple[str, StageContext, Callable[[], None]]] = []
                for child in e.children:
                    child_spec = require_stage_spec(self.stage_specs, child.name)
                    child_dir = group_dir / child.name
                    child_dir.mkdir(parents=True, exist_ok=True)
                    child_ctx = StageContext(
                        client=self._get_client(child_spec),
                        session_dir=child_dir,
                        gr_id=gr_id,
                        child_key=child.name,
                    )
                    child_runners.append(
                        (
                            child.name,
                            child_ctx,
                            self._make_runner(child, child_ctx, child_spec.model),
                        )
                    )
                stages.extend(
                    build_parallel_stages(
                        e.name,
                        child_runners,
                        max_concurrent=e.max_concurrent,
                        set_stage_fn=lambda n: set_stage(gr_id, n),
                        cancel_on_bail=e.cancel_on_bail,
                        bail_policy=e.bail_policy,
                        gr_id=gr_id,
                        project_root=pathlib.Path.cwd(),
                    )
                )
            else:
                stage_spec = require_stage_spec(self.stage_specs, e.name)
                stage_ctx = StageContext(
                    client=self._get_client(stage_spec),
                    session_dir=self.session_dir,
                    gr_id=gr_id,
                )
                stages.append(
                    (e.name, self._make_runner(e, stage_ctx, stage_spec.model))
                )

        run_stages(stages, resume_from=self.args.resume_from)


class LocalPipeline(Pipeline):
    target = "local"
    STAGE_TYPES: dict[str, type[Stage]] = {
        "plan": plan.Plan,
        "implement": implement.Implement,
        "verify": verify.Verify,
        "review-code": review_code.ReviewCode,
        "address-code": address_code.AddressCode,
    }

    def __init__(
        self,
        stages: list[StageEntry],
        *,
        args: argparse.Namespace,
        session_dir: pathlib.Path,
        gr_id: str | None,
        pipeline_data: _PipelineData,
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
        self.plan_copied_from_source = False

        plan_path = getattr(args, "plan_path", None)
        spec_path = getattr(args, "spec_path", None)

        plan_file = session_dir / "plan.md"
        if plan_path and not plan_file.exists():
            src = pathlib.Path(plan_path)
            if not src.is_file():
                raise ValueError(f"--plan: file not found: {plan_path}")
            if src.stat().st_size == 0:
                raise ValueError(f"--plan: file is empty: {plan_path}")
            shutil.copyfile(src, plan_file)
            self.plan_copied_from_source = True

        spec_file = session_dir / "spec.md"
        if spec_path and not spec_file.exists():
            spec_src = pathlib.Path(spec_path)
            if not spec_src.is_file():
                raise ValueError(f"--spec: file not found: {spec_path}")
            if spec_src.stat().st_size == 0:
                raise ValueError(f"--spec: file is empty: {spec_path}")
            shutil.copyfile(spec_src, spec_file)

    def _make_runner(
        self, entry: StageEntry, ctx: StageContext, model: str
    ) -> Callable[[], None]:
        args = self.args
        plan_file = self.session_dir / "plan.md"
        spec_file = self.session_dir / "spec.md"
        is_git = self.is_git
        gr_id = self.gr_id
        plan_copied_from_source = self.plan_copied_from_source
        instructions = " ".join(getattr(args, "instructions", None) or [])
        plan_path = getattr(args, "plan_path", None)

        if entry.type == "plan":

            def _plan() -> None:
                if plan_path:
                    if plan_copied_from_source:
                        logger.info(
                            "plan supplied via --plan (copied) -> %s", plan_file
                        )
                    else:
                        logger.info("plan reused from snapshot -> %s", plan_file)
                else:
                    set_stage(gr_id, entry.name)
                    logger.info("planning (model: %s) -> %s", model, plan_file)
                    if not entry.prompt_paths:
                        _die(
                            f"stage {entry.name!r}: type 'plan' requires a 'prompt' field in the pipeline YAML"
                        )
                    stage = plan.Plan(
                        entry,
                        model,
                        plan_file=plan_file,
                        instructions=instructions,
                    )
                    stage.bind(ctx)
                    stage.run(None)

            return _plan

        if entry.type == "implement":

            def _implement() -> None:
                plan_text = plan_file.read_text(encoding="utf-8")
                spec_text = ""
                if spec_file.exists():
                    try:
                        spec_text = spec_file.read_text(encoding="utf-8")
                    except (OSError, UnicodeDecodeError) as exc:
                        logger.warning(
                            "could not read spec.md (%s); proceeding without north-star context",
                            exc,
                        )
                set_stage(gr_id, entry.name)
                logger.info("implementing (model: %s, from %s)", model, plan_file)
                stage = implement.Implement(
                    entry,
                    model,
                    plan_text=plan_text,
                    is_git=is_git,
                    spec_text=spec_text,
                )
                stage.bind(ctx)
                stage.run(None)

            return _implement

        if entry.type == "review-code":

            def _review_code() -> None:
                plan_text = plan_file.read_text(encoding="utf-8")
                set_stage(gr_id, entry.name)
                logger.info("reviewing code (model: %s)", model)
                stage = review_code.ReviewCode(
                    entry,
                    model,
                    plan_text=plan_text,
                    is_git=is_git,
                )
                stage.bind(ctx)
                review_file = stage.run(None)
                logger.info("code review (%s): %s", model, review_file)

            return _review_code

        if entry.type == "address-code":

            def _all_stages() -> Iterator[StageEntry]:
                for s in self.pipeline_data.stages:
                    yield s
                    if s.type == "parallel":
                        yield from s.children

            review_stage_names = [
                s.name for s in _all_stages() if s.type == "review-code"
            ]

            def _address_code() -> None:
                set_stage(gr_id, entry.name)
                logger.info("addressing code reviews (model: %s)", model)
                stage = address_code.AddressCode(
                    entry,
                    model,
                    is_git=is_git,
                    review_stage_names=review_stage_names,
                )
                stage.bind(ctx)
                stage.run(None)

            return _address_code

        if entry.type == "verify":
            cmds = getattr(args, "cmds", None)
            if cmds is not None:
                entry.options["cmds"] = cmds
            entry.options.setdefault(
                "max_attempts", getattr(args, "test_max_attempts", 3)
            )

            def _verify() -> None:
                resolved_cmds = entry.options.get("cmds", [])
                if resolved_cmds:
                    set_stage(gr_id, entry.name)
                    logger.info(
                        "running verify (cmds: %r, max-attempts: %s, model: %s)",
                        resolved_cmds,
                        entry.options.get("max_attempts"),
                        model,
                    )
                stage = verify.Verify(
                    entry,
                    model,
                    is_git=is_git,
                    commit_after_fix=is_git,
                )
                stage.bind(ctx)
                stage.run(None)

            return _verify

        raise ValueError(f"unsupported stage type {entry.type!r} in local pipeline")


class GHPipeline(Pipeline):
    target = "github"
    STAGE_TYPES: dict[str, type[Stage]] = {
        "plan": ghplan.GHPlan,
        "implement": implement.Implement,
        "verify": verify.Verify,
        "commit": commit.Commit,
        "open-github-pr": open_github_pr.OpenGitHubPR,
        "request-copilot": request_copilot.RequestCopilot,
        "ghreview": ghreview.GHReview,
        "ghaddress": ghaddress.GHAddress,
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
        self.instructions: str = " ".join(getattr(args, "instructions", None) or [])
        self.issue_url: str = ""
        self.issue_num: str = ""
        self.issue_body: str = ""
        self.impl_result: ImplStageResult | None = None
        self.pr_url: str = ""
        self.pr_num: str = ""

    def _ensure_pr_url(self) -> None:
        if self.pr_url:
            return
        saved = read_state_field(self.state_file, "pr_url")
        if not saved:
            _die(
                f"--resume-from {self.args.resume_from}: no pr_url in state.json "
                "(rewind to implement?)"
            )
        self.pr_url = saved
        self.pr_num = saved.split("/")[-1]
        logger.info("resumed PR: %s", saved)

    def _make_runner(
        self, entry: StageEntry, ctx: StageContext, model: str
    ) -> Callable[[], None]:
        if entry.type == "plan":

            def _plan() -> None:
                if self.args.plan_source:
                    return
                if not entry.prompt_paths:
                    _die(
                        f"stage {entry.name!r}: type 'plan' requires a 'prompt' field in the pipeline YAML"
                    )
                set_stage(self.gr_id, entry.name)
                logger.info("[1/8] running ghplan")
                stage = ghplan.GHPlan(
                    entry,
                    model,
                    ref=self.args.ref or "",
                    instructions=self.instructions,
                    repo=self.repo,
                )
                stage.bind(ctx)
                result = stage.run(None)
                self.issue_url = result.issue_url
                self.issue_num = result.issue_num
                self.issue_body = result.issue_body

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
                stage = implement.Implement(
                    entry,
                    model,
                    plan_text=self.issue_body,
                    is_git=True,
                    kind="gh",
                    issue_num=self.issue_num,
                    spec_text=spec_text,
                )
                stage.bind(ctx)
                impl_result = stage.run(None)
                if impl_result is None:
                    _die("implement stage did not produce a result")
                self.impl_result = impl_result
                patch_state(
                    self.gr_id,
                    impl_handoff_branch=impl_result.handoff_branch,
                    impl_base_ref=impl_result.pre_state.head,
                )

            return _implement

        if entry.type == "verify":

            def _verify() -> None:
                set_stage(self.gr_id, entry.name)
                logger.info("[2b/8] verifying implementation")
                stage = verify.Verify(entry, model, is_git=True, commit_after_fix=False)
                stage.bind(ctx)
                stage.run(None)

            return _verify

        if entry.type == "commit":

            def _commit() -> None:
                set_stage(self.gr_id, entry.name)
                logger.info("[2c/8] committing changes")
                impl_result = self.impl_result
                if impl_result is not None:
                    impl_outcome = impl_result.outcome
                    impl_handoff_branch = impl_result.handoff_branch
                    base_ref = impl_result.pre_state.head
                else:
                    impl_handoff_branch = read_state_field(
                        self.state_file, "impl_handoff_branch"
                    )
                    base_ref = read_state_field(self.state_file, "impl_base_ref")
                    if not base_ref:
                        _die(
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
                            _die(
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
                logger.info("[2d/8] opening GitHub PR")
                stage = open_github_pr.OpenGitHubPR(
                    entry,
                    model,
                    issue_url=self.issue_url,
                    cwd=None,
                )
                stage.bind(ctx)
                pr_url = stage.run(None)
                pr_num = pr_url.split("/")[-1]
                logger.info("PR: %s", pr_url)
                patch_state(self.gr_id, pr_url=pr_url)
                self.pr_url = pr_url
                self.pr_num = pr_num

            return _open_github_pr

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
                    _die(
                        f"stage {entry.name!r}: type 'ghreview' requires a 'prompt' field in the pipeline YAML"
                    )
                self._ensure_pr_url()
                set_stage(self.gr_id, entry.name)
                logger.info("[4/8] running /ghreview")
                stage = ghreview.GHReview(entry, model, pr_url=self.pr_url)
                stage.bind(ctx)
                stage.run(None)

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
                    _die(
                        f"stage {entry.name!r}: type 'ghaddress' requires a 'prompt' field in the pipeline YAML"
                    )
                self._ensure_pr_url()
                set_stage(self.gr_id, entry.name)
                logger.info("[6/8] running /ghaddress")
                stage = ghaddress.GHAddress(entry, model, pr_url=self.pr_url)
                stage.bind(ctx)
                stage.run(None)

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
