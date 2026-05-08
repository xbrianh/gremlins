"""Merged pipeline orchestrator (gh + local)."""

from __future__ import annotations

import argparse
import dataclasses
import json
import logging
import pathlib
import shutil
import sys
from collections.abc import Callable
from typing import Any, NoReturn

from gremlins.clients import ClientSpec
from gremlins.clients.protocol import ClaudeClient
from gremlins.clients.resolve import require_stage_spec
from gremlins.git import in_git_repo
from gremlins.pipeline import Pipeline as _PipelineData
from gremlins.pipeline import StageEntry
from gremlins.runner import build_parallel_stages, install_signal_handlers, run_stages
from gremlins.stages import (
    address_code,
    commit,
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
from gremlins.stages.chain import Chain
from gremlins.state import resolve_state_file, set_stage

logger = logging.getLogger(__name__)


def die(msg: str) -> NoReturn:
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


def read_stage_inputs(sf: pathlib.Path | None) -> dict[str, Any]:
    if sf is None or not sf.exists():
        return {}
    try:
        data = json.loads(sf.read_text(encoding="utf-8"))
        return data.get("stage_inputs") or {}
    except Exception:
        return {}


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
    STAGE_TYPES: dict[str, type[Stage]] = {
        "plan": plan.Plan,
        "implement": implement.Implement,
        "verify": verify.Verify,
        "review-code": review_code.ReviewCode,
        "address-code": address_code.AddressCode,
        "chain": Chain,
        "handoff-branch": handoff_branch_mod.HandoffBranch,
        "commit": commit.Commit,
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
        repo: str = "",
        target: str = "local",
        state_file: pathlib.Path | None = None,
        stage_specs: dict[str, ClientSpec] | None = None,
        spec_clients: dict[str, ClaudeClient] | None = None,
        test_client: ClaudeClient | None = None,
    ) -> None:
        unknown: list[str] = []
        for s in stages:
            if s.type == "parallel":
                unknown.extend(
                    c.type for c in s.children if c.type not in self.STAGE_TYPES
                )
            elif s.type not in self.STAGE_TYPES:
                unknown.append(s.type)
        if unknown:
            raise ValueError(f"Pipeline does not support stage type(s): {unknown}")
        self.stages = _expand_stage_entries(stages)
        self.args = args
        self.session_dir = session_dir
        self.gr_id = gr_id
        self.is_git = in_git_repo()
        self.pipeline_data = pipeline_data
        self.repo = repo
        self.target = target
        self.state_file = state_file
        self.stage_specs: dict[str, ClientSpec] = stage_specs or {}
        self.spec_clients: dict[str, ClaudeClient] = spec_clients or {}
        self.test_client = test_client

        sf = state_file if state_file is not None else resolve_state_file(gr_id)
        self.instructions: str = read_stage_inputs(sf).get("instructions") or " ".join(
            getattr(args, "instructions", None) or []
        )

        spec_path = getattr(args, "spec_path", None)
        spec_file = session_dir / "spec.md"
        if spec_path and not spec_file.exists():
            spec_src = pathlib.Path(spec_path)
            if not spec_src.is_file():
                raise ValueError(f"--spec: file not found: {spec_path}")
            if spec_src.stat().st_size == 0:
                raise ValueError(f"--spec: file is empty: {spec_path}")
            shutil.copyfile(spec_src, spec_file)

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
        self, entry: StageEntry, ctx: StageContext, spec: ClientSpec
    ) -> Callable[[], None]:
        model = spec.model
        args = self.args
        plan_file = self.session_dir / "plan.md"
        spec_file = self.session_dir / "spec.md"
        is_git = self.is_git
        gr_id = self.gr_id
        instructions = self.instructions
        plan_path = getattr(args, "plan_path", None)

        if entry.type == "plan":

            def _plan() -> None:
                plan_source = getattr(args, "plan_source", None) or plan_path
                if not entry.prompt_paths and not plan_source:
                    die(
                        f"stage {entry.name!r}: type 'plan' requires a 'prompt' field in the pipeline YAML"
                    )
                set_stage(gr_id, entry.name)
                if self.repo:
                    stage = plan.Plan(
                        entry,
                        model,
                        plan_source=plan_source,
                        instructions=instructions,
                        repo=self.repo,
                    )
                    stage.bind(ctx)
                    stage.run(self)
                else:
                    logger.info("planning (model: %s) -> %s", model, plan_file)
                    stage = plan.Plan(
                        entry,
                        model,
                        plan_source=plan_path,
                        plan_file=plan_file,
                        instructions=instructions,
                    )
                    stage.bind(ctx)
                    stage.run(None)

            return _plan

        if entry.type == "implement":

            def _implement() -> None:
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
                if not self.repo:
                    logger.info("implementing (model: %s, from %s)", model, plan_file)
                stage = implement.Implement(
                    entry,
                    model,
                    is_git=is_git,
                    spec_text=spec_text,
                )
                stage.bind(ctx)
                stage.run(self if self.repo else None)

            return _implement

        if entry.type == "handoff-branch":

            def _handoff_branch() -> None:
                set_stage(gr_id, entry.name)
                stage = handoff_branch_mod.HandoffBranch(entry, model)
                stage.bind(ctx)
                stage.run(self)

            return _handoff_branch

        if entry.type == "verify":
            if not self.repo:
                cmds = getattr(args, "cmds", None)
                if cmds is not None:
                    entry.options["cmds"] = cmds
                entry.options.setdefault(
                    "max_attempts", getattr(args, "test_max_attempts", 3)
                )

            def _verify() -> None:
                resolved_cmds = entry.options.get("cmds", [])
                if self.repo or resolved_cmds:
                    set_stage(gr_id, entry.name)
                    if resolved_cmds and not self.repo:
                        logger.info(
                            "running verify (cmds: %r, max-attempts: %s, model: %s)",
                            resolved_cmds,
                            entry.options.get("max_attempts"),
                            model,
                        )
                stage = verify.Verify(entry, model, is_git=is_git)
                stage.bind(ctx)
                stage.run(None)

            return _verify

        if entry.type == "commit":

            def _commit() -> None:
                set_stage(gr_id, entry.name)
                stage = commit.Commit(entry, model)
                stage.bind(ctx)
                stage.run(None)

            return _commit

        if entry.type == "open-github-pr":

            def _open_github_pr() -> None:
                set_stage(gr_id, entry.name)
                stage = open_github_pr.OpenGitHubPR(
                    entry,
                    model,
                    issue_url=read_state_field(self.state_file, "issue_url"),
                )
                stage.bind(ctx)
                pr_url = stage.run(None)
                logger.info("PR: %s", pr_url)

            return _open_github_pr

        if entry.type == "request-copilot":

            def _request_copilot() -> None:
                set_stage(gr_id, entry.name)
                stage = request_copilot.RequestCopilot(entry, model, repo=self.repo)
                stage.bind(ctx)
                stage.run(None)

            return _request_copilot

        if entry.type == "ghreview":

            def _ghreview() -> None:
                if not entry.prompt_paths:
                    die(
                        f"stage {entry.name!r}: type 'ghreview' requires a 'prompt' field in the pipeline YAML"
                    )
                set_stage(gr_id, entry.name)
                stage = review_code.ReviewCode(entry, model, plan_text="", is_git=True)
                stage.bind(ctx)
                stage.run(self)

            return _ghreview

        if entry.type == "wait-copilot":

            def _wait_copilot() -> None:
                set_stage(gr_id, entry.name)
                stage = wait_copilot.WaitCopilot(entry, model, repo=self.repo)
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
                set_stage(gr_id, entry.name)
                stage = address_code.AddressCode(entry, model, is_git=True)
                stage.bind(ctx)
                stage.run(self)

            return _ghaddress

        if entry.type == "wait-ci":

            def _wait_ci() -> None:
                set_stage(gr_id, entry.name)
                stage = wait_ci.WaitCI(entry, model)
                stage.bind(ctx)
                stage.run(None)

            return _wait_ci

        if entry.type == "review-code":
            review_stage_names: list[str] = []
            review_stage_dirs: dict[str, pathlib.Path] = {}
            for s in self.pipeline_data.stages:
                if s.type == "parallel":
                    for child in s.children:
                        if child.type == "review-code":
                            review_stage_names.append(child.name)
                            review_stage_dirs[child.name] = (
                                self.session_dir / s.name / child.name
                            )
                elif s.type == "review-code":
                    review_stage_names.append(s.name)
                    review_stage_dirs[s.name] = self.session_dir

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
            ac_review_stage_names: list[str] = []
            ac_review_stage_dirs: dict[str, pathlib.Path] = {}
            for s in self.pipeline_data.stages:
                if s.type == "parallel":
                    for child in s.children:
                        if child.type == "review-code":
                            ac_review_stage_names.append(child.name)
                            ac_review_stage_dirs[child.name] = (
                                self.session_dir / s.name / child.name
                            )
                elif s.type == "review-code":
                    ac_review_stage_names.append(s.name)
                    ac_review_stage_dirs[s.name] = self.session_dir

            def _address_code() -> None:
                set_stage(gr_id, entry.name)
                logger.info("addressing code reviews (model: %s)", model)
                stage = address_code.AddressCode(
                    entry,
                    model,
                    is_git=is_git,
                    review_stage_names=ac_review_stage_names,
                    review_stage_dirs=ac_review_stage_dirs,
                )
                stage.bind(ctx)
                stage.run(None)

            return _address_code

        if entry.type == "chain":

            def _chain() -> None:
                set_stage(gr_id, entry.name)
                logger.info(
                    "running chain stage (child: %s)",
                    entry.options.get("child", "local"),
                )
                stage = Chain(
                    entry,
                    spec,
                    pipeline_builder=self._build_child_stages,
                )
                stage.bind(ctx)
                stage.run(None)

            return _chain

        raise ValueError(f"unsupported stage type {entry.type!r}")

    def _collect_stages(self) -> list[tuple[str, Callable[[], None]]]:
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
                            self._make_runner(child, child_ctx, child_spec),
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
                stages.append((e.name, self._make_runner(e, stage_ctx, stage_spec)))
        return stages

    def run(self, *clients: ClaudeClient) -> None:
        install_signal_handlers(*clients)
        stages = self._collect_stages()
        run_stages(stages, resume_from=self.args.resume_from)

    def _build_child_stages(
        self,
        pipeline_name: str,
        plan_path: pathlib.Path,
        session_dir: pathlib.Path,
        resume_from: str | None,
    ) -> list[tuple[str, Callable[[], None]]]:
        import argparse as _argparse

        from gremlins.clients.resolve import collect_stage_specs
        from gremlins.pipeline import load_pipeline, resolve_pipeline_path

        pipeline = load_pipeline(
            resolve_pipeline_path(pipeline_name, pathlib.Path.cwd())
        )
        if any(s.type == "chain" for s in pipeline.stages):
            raise ValueError(
                f"child pipeline {pipeline_name!r} contains a 'chain' stage; "
                "nested chain stages are not supported"
            )
        child_args = _argparse.Namespace(
            plan_path=str(plan_path),
            spec_path=None,
            cmds=getattr(self.args, "cmds", None),
            test_max_attempts=getattr(self.args, "test_max_attempts", 3),
            instructions=None,
            resume_from=resume_from,
        )
        stage_specs = collect_stage_specs(pipeline, None)
        spec_clients: dict[str, ClaudeClient] = {
            str(spec): self._get_client(spec) for spec in stage_specs.values()
        }
        child_pipeline = Pipeline(
            pipeline.stages,
            args=child_args,
            session_dir=session_dir,
            gr_id=self.gr_id,
            pipeline_data=pipeline,
            stage_specs=stage_specs,
            spec_clients=spec_clients,
            test_client=self.test_client,
        )
        return child_pipeline._collect_stages()
