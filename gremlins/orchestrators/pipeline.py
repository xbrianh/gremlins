"""Pipeline orchestrator classes."""

from __future__ import annotations

import argparse
import dataclasses
import logging
import pathlib
import shutil
import sys
from collections.abc import Callable, Iterator
from typing import NoReturn

from gremlins.clients import ClientSpec
from gremlins.clients.protocol import ClaudeClient
from gremlins.clients.resolve import require_stage_spec
from gremlins.git import in_git_repo
from gremlins.pipeline import Pipeline as _PipelineData
from gremlins.pipeline import StageEntry
from gremlins.pipeline import load_pipeline as _load_pipeline
from gremlins.runner import build_parallel_stages, install_signal_handlers, run_stages
from gremlins.stages import (
    address_code,
    commit_pr,
    ghaddress,
    ghplan,
    ghreview,
    implement,
    plan,
    request_copilot,
    review_code,
    verify,
    wait_ci,
    wait_copilot,
)
from gremlins.stages.base import Stage
from gremlins.stages.context import StageContext
from gremlins.state import resolve_session_dir, set_stage

logger = logging.getLogger(__name__)


def _die(msg: str) -> NoReturn:
    sys.stderr.write(f"error: {msg}\n")
    sys.stderr.flush()
    sys.exit(1)


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
            # children are not resume targets; only the three group-level stages are
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


class PipelineRunner:
    STAGE_TYPES: dict[str, type[Stage]] = {}
    target: str = ""

    def __init__(
        self,
        stages: list[StageEntry],
        *,
        args: argparse.Namespace,
        session_dir: pathlib.Path,
        gr_id: str | None,
    ) -> None:
        if self.STAGE_TYPES:
            unknown = [
                s.type
                for s in stages
                if s.type != "parallel" and s.type not in self.STAGE_TYPES
            ]
            if unknown:
                raise ValueError(
                    f"{type(self).__name__} does not support stage type(s): {unknown}"
                )
        self.stages = _expand_stage_entries(stages)
        self.args = args
        self.session_dir = session_dir
        self.gr_id = gr_id
        self.is_git = in_git_repo()

    @classmethod
    def from_yaml(
        cls,
        path: pathlib.Path,
        *,
        args: argparse.Namespace,
        gr_id: str | None,
    ) -> "PipelineRunner":
        pipeline_data = _load_pipeline(path)
        session_dir = resolve_session_dir(gr_id)
        return cls(
            pipeline_data.stages,
            args=args,
            session_dir=session_dir,
            gr_id=gr_id,
        )

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

    def run(self, *clients: ClaudeClient) -> None:
        # stub — stage-running loop lands in a later plan step
        install_signal_handlers(*clients)


# Keep old name as alias for backwards compatibility during migration
Pipeline = PipelineRunner


class LocalPipeline(PipelineRunner):
    target = "local"
    STAGE_TYPES: dict[str, type[Stage]] = {
        "plan": plan.Plan,
        "implement": implement.Implement,
        "verify": verify.Verify,
        "review-code": review_code.ReviewCode,
        "address-code": address_code.AddressCode,
    }

    @classmethod
    def from_yaml(
        cls,
        path: pathlib.Path,
        *,
        args: argparse.Namespace,
        gr_id: str | None,
    ) -> "LocalPipeline":
        pipeline_data = _load_pipeline(path)
        session_dir = resolve_session_dir(gr_id)
        return cls(
            pipeline_data.stages,
            args=args,
            session_dir=session_dir,
            gr_id=gr_id,
            _pipeline_data=pipeline_data,
        )

    def __init__(
        self,
        stages: list[StageEntry],
        *,
        args: argparse.Namespace,
        session_dir: pathlib.Path,
        gr_id: str | None,
        _pipeline_data: _PipelineData,
        _test_client: ClaudeClient | None = None,
        _spec_clients: dict[str, ClaudeClient] | None = None,
        _stage_specs: dict[str, ClientSpec] | None = None,
    ) -> None:
        super().__init__(stages, args=args, session_dir=session_dir, gr_id=gr_id)
        self._pipeline_data = _pipeline_data
        self._test_client = _test_client
        self._spec_clients: dict[str, ClaudeClient] = _spec_clients or {}
        self._stage_specs: dict[str, ClientSpec] = _stage_specs or {}
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

    def _get_client(self, spec: ClientSpec) -> ClaudeClient:
        if self._test_client is not None:
            return self._test_client
        return self._spec_clients[str(spec)]

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
                        logger.info("plan supplied via --plan (copied) -> %s", plan_file)
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
                for s in self._pipeline_data.stages:
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
            entry.options.setdefault("max_attempts", getattr(args, "test_max_attempts", 3))

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

    def run(self, *clients: ClaudeClient) -> None:
        install_signal_handlers(*clients)

        stage_specs = self._stage_specs
        gr_id = self.gr_id

        stages: list[tuple[str, Callable[[], None]]] = []
        for e in self._pipeline_data.stages:
            if e.type == "parallel":
                group_dir = self.session_dir / e.name
                group_dir.mkdir(parents=True, exist_ok=True)
                child_runners: list[tuple[str, StageContext, Callable[[], None]]] = []
                for child in e.children:
                    child_spec = require_stage_spec(stage_specs, child.name)
                    child_dir = group_dir / child.name
                    child_dir.mkdir(parents=True, exist_ok=True)
                    child_ctx = StageContext(
                        client=self._get_client(child_spec),
                        session_dir=child_dir,
                        gr_id=gr_id,
                        child_key=child.name,
                    )
                    child_runners.append(
                        (child.name, child_ctx, self._make_runner(child, child_ctx, child_spec.model))
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
                stage_spec = require_stage_spec(stage_specs, e.name)
                stage_ctx = StageContext(
                    client=self._get_client(stage_spec),
                    session_dir=self.session_dir,
                    gr_id=gr_id,
                )
                stages.append((e.name, self._make_runner(e, stage_ctx, stage_spec.model)))

        run_stages(stages, resume_from=self.args.resume_from)


class GHPipeline(PipelineRunner):
    target = "github"
    STAGE_TYPES: dict[str, type[Stage]] = {
        "plan": ghplan.GHPlan,
        "implement": implement.Implement,
        "verify": verify.Verify,
        "commit-pr": commit_pr.CommitPR,
        "request-copilot": request_copilot.RequestCopilot,
        "ghreview": ghreview.GHReview,
        "ghaddress": ghaddress.GHAddress,
        "wait-ci": wait_ci.WaitCI,
        "wait-copilot": wait_copilot.WaitCopilot,
    }
