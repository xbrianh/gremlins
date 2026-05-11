"""Merged pipeline orchestrator (gh + local)."""

from __future__ import annotations

import argparse
import json
import logging
import pathlib
import shutil
from collections.abc import Callable
from typing import Any

import gremlins.stages.all as _stages_all  # noqa: F401  # type: ignore[reportUnusedImport]
from gremlins.clients.client import Client
from gremlins.git import in_git_repo
from gremlins.runner import run_stages
from gremlins.pipeline import Pipeline as _PipelineData
from gremlins.stage_clients import require_stage_spec
from gremlins.stages.base import RuntimeState, Stage
from gremlins.stages.registry import STAGE_REGISTRY
from gremlins.state import resolve_state_file

logger = logging.getLogger(__name__)


def read_stage_inputs(sf: pathlib.Path | None) -> dict[str, Any]:
    if sf is None or not sf.exists():
        return {}
    try:
        data = json.loads(sf.read_text(encoding="utf-8"))
        return data.get("stage_inputs") or {}
    except Exception:
        return {}


def _expand_stage_entries(raw_stages: list[Stage]) -> list[Stage]:
    top_level_names = {e.name for e in raw_stages}
    child_names: set[str] = set()
    seen: set[str] = set()
    result: list[Stage] = []

    for entry in raw_stages:
        if entry.type == "parallel":
            for child in entry.body:
                if child.name in child_names or child.name in top_level_names:
                    raise ValueError(f"duplicate child stage name {child.name!r}")
                child_names.add(child.name)
        if entry.name in seen:
            raise ValueError(f"pipeline has duplicate stage name {entry.name!r}")
        seen.add(entry.name)
        result.append(entry)

    return result


class Pipeline:
    def __init__(
        self,
        stages: list[Stage],
        *,
        args: argparse.Namespace,
        session_dir: pathlib.Path,
        gr_id: str | None,
        pipeline_data: _PipelineData,
        repo: str = "",
        state_file: pathlib.Path | None = None,
        stage_specs: dict[str, Client] | None = None,
        spec_clients: dict[str, Client] | None = None,
        test_client: Client | None = None,
    ) -> None:
        unknown: list[str] = []
        for s in stages:
            if s.type not in STAGE_REGISTRY:
                unknown.append(s.type)
            elif s.type == "parallel":
                unknown.extend(c.type for c in s.body if c.type not in STAGE_REGISTRY)
            elif s.type == "loop":
                unknown.extend(c.type for c in s.body if c.type not in STAGE_REGISTRY)
        if unknown:
            raise ValueError(f"Pipeline does not support stage type(s): {unknown}")
        self.stages = _expand_stage_entries(stages)
        self.args = args
        self.session_dir = session_dir
        self.gr_id = gr_id
        self.is_git = in_git_repo()
        self.pipeline_data = pipeline_data
        self.repo = repo
        self.state_file = state_file
        self.stage_specs: dict[str, Client] = stage_specs or {}
        self.spec_clients: dict[str, Client] = spec_clients or {}
        self.test_client = test_client

        sf = state_file if state_file is not None else resolve_state_file(gr_id)
        self.instructions: str = read_stage_inputs(sf).get("instructions") or " ".join(
            getattr(args, "instructions", None) or []
        )

        spec_path = getattr(args, "spec", None)
        spec_file = session_dir / "spec.md"
        if spec_path and not spec_file.exists():
            spec_src = pathlib.Path(spec_path)
            if not spec_src.is_file():
                raise ValueError(f"--spec: file not found: {spec_path}")
            if spec_src.stat().st_size == 0:
                raise ValueError(f"--spec: file is empty: {spec_path}")
            shutil.copyfile(spec_src, spec_file)

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

    def _collect_stages(
        self, stages: list[Stage]
    ) -> list[tuple[str, Callable[[], None]]]:
        built: list[tuple[str, Callable[[], None]]] = []
        for e in stages:
            stage_spec = require_stage_spec(self.stage_specs, e.name)
            resolved = self.test_client or self.spec_clients.get(
                str(stage_spec), stage_spec
            )
            # Propagate the spec model to the stage so that stages calling
            # run_claude (which falls back to state.client.model when self.model
            # is None) use the spec model rather than the live client's model.
            if e.model is None and stage_spec.model:
                e.model = stage_spec.model
            stage_state = RuntimeState(
                client=resolved,
                session_dir=self.session_dir,
                gr_id=self.gr_id,
                state_file=self.state_file,
                args=self.args,
                pipeline_data=self.pipeline_data,
                repo=self.repo,
                instructions=self.instructions,
                stage_specs=self.stage_specs,
                spec_clients=self.spec_clients,
                is_git=self.is_git,
                test_client=self.test_client,
            )
            built.append((e.name, stage_state.make_runner(e, scope=stages)))
        return built

    def run(self) -> None:
        built = self._collect_stages(self.pipeline_data.stages)
        run_stages(built, resume_from=self.args.resume_from)
