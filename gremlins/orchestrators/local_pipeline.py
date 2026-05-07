"""Local pipeline orchestrator."""

from __future__ import annotations

import argparse
import logging
import pathlib
import shutil
from collections.abc import Callable, Iterator

from gremlins.clients import ClientSpec
from gremlins.clients.protocol import ClaudeClient
from gremlins.orchestrators.base import Pipeline, die
from gremlins.pipeline import Pipeline as _PipelineData
from gremlins.pipeline import StageEntry
from gremlins.stages import address_code, implement, plan, review_code, verify
from gremlins.stages.base import Stage, StageContext
from gremlins.stages.chain import Chain
from gremlins.state import set_stage

logger = logging.getLogger(__name__)


class LocalPipeline(Pipeline):
    target = "local"
    STAGE_TYPES: dict[str, type[Stage]] = {
        "plan": plan.Plan,
        "implement": implement.Implement,
        "verify": verify.Verify,
        "review-code": review_code.ReviewCode,
        "address-code": address_code.AddressCode,
        "chain": Chain,
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
        self, entry: StageEntry, ctx: StageContext, spec: ClientSpec
    ) -> Callable[[], None]:
        model = spec.model
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
                        die(
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

        raise ValueError(f"unsupported stage type {entry.type!r} in local pipeline")

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
        child_pipeline = LocalPipeline(
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
