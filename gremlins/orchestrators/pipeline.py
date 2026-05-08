"""Merged pipeline orchestrator (gh + local)."""

from __future__ import annotations

import argparse
import dataclasses
import json
import logging
import pathlib
import shutil
from collections.abc import Callable
from typing import Any

import gremlins.stages.all as _stages_all  # noqa: F401  # type: ignore[reportUnusedImport]
from gremlins.clients import ClientSpec
from gremlins.clients.protocol import ClaudeClient
from gremlins.clients.resolve import require_stage_spec
from gremlins.git import in_git_repo
from gremlins.pipeline import PipelineDef as _PipelineData
from gremlins.pipeline import StageEntry
from gremlins.runner import run_stages
from gremlins.stages.base import StageContext
from gremlins.stages.registry import STAGE_BUILDERS, STAGE_NEEDS_PIPE
from gremlins.state import resolve_state_file, set_stage

logger = logging.getLogger(__name__)


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
            for child in entry.body:
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


class StageRunner:
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
                    c.type for c in s.body if c.type not in STAGE_BUILDERS
                )
            elif s.type not in STAGE_BUILDERS:
                unknown.append(s.type)
        if unknown:
            raise ValueError(f"StageRunner does not support stage type(s): {unknown}")
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
        builder = STAGE_BUILDERS[entry.type]
        gr_id = self.gr_id
        pipe = self

        def _run() -> None:
            set_stage(gr_id, entry.name)
            stage = builder(entry, spec, pipe)
            stage.bind(ctx)
            stage.run(pipe if STAGE_NEEDS_PIPE.get(entry.type) else None)

        return _run

    def _build_parallel_stage(
        self, e: StageEntry, gr_id: str | None
    ) -> Any:
        from gremlins.stages.parallel import ParallelStage

        group_dir = self.session_dir / e.name
        group_dir.mkdir(parents=True, exist_ok=True)
        child_runners: list[tuple[str, StageContext, Callable[[], None]]] = []
        for child in e.body:
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
                (child.name, child_ctx, self._make_runner(child, child_ctx, child_spec))
            )
        return ParallelStage(
            e,
            None,
            child_runners,
            max_concurrent=e.max_concurrent,
            cancel_on_bail=e.cancel_on_bail,
            bail_policy=e.bail_policy,
            gr_id=gr_id,
            project_root=pathlib.Path.cwd(),
            set_stage_fn=lambda n: set_stage(gr_id, n),
        )

    def _collect_stages(
        self, stages: list[StageEntry]
    ) -> list[tuple[str, Callable[[], None]]]:
        gr_id = self.gr_id
        built: list[tuple[str, Callable[[], None]]] = []
        for e in stages:
            if e.type == "parallel":
                built.extend(self._build_parallel_stage(e, gr_id).build_runtime_stages())
            else:
                stage_spec = require_stage_spec(self.stage_specs, e.name)
                stage_ctx = StageContext(
                    client=self._get_client(stage_spec),
                    session_dir=self.session_dir,
                    gr_id=gr_id,
                )
                built.append((e.name, self._make_runner(e, stage_ctx, stage_spec)))
        return built

    def run(self) -> None:
        built = self._collect_stages(self.pipeline_data.stages)
        run_stages(built, resume_from=self.args.resume_from)

    def build_child_stages(
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
            plan=str(plan_path),
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
        child_runner = StageRunner(
            pipeline.stages,
            args=child_args,
            session_dir=session_dir,
            gr_id=self.gr_id,
            pipeline_data=pipeline,
            stage_specs=stage_specs,
            spec_clients=spec_clients,
            test_client=self.test_client,
        )
        return child_runner._collect_stages(pipeline.stages)
