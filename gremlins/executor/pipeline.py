"""Merged pipeline orchestrator (gh + local)."""

from __future__ import annotations

import argparse
import json
import logging
import pathlib
import shutil
from collections.abc import Callable
from typing import Any

from gremlins.clients.client import PACKAGE_DEFAULT, Client
from gremlins.executor.state import State, resolve_state_file
from gremlins.pipeline import Pipeline as _PipelineData
from gremlins.pipeline.loader import STAGE_TYPES
from gremlins.stages.base import Stage

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


def run_stages(
    stages: list[tuple[str, Callable[[], None]]], *, resume_from: str | None = None
) -> None:
    start_idx = 0
    if resume_from is not None:
        names = [name for name, _ in stages]
        if resume_from not in names:
            raise ValueError(
                f"resume_from {resume_from!r} is not a valid stage; valid: {names}"
            )
        start_idx = names.index(resume_from)
    for _, fn in stages[start_idx:]:
        fn()


class Pipeline:
    def __init__(
        self,
        stages: list[Stage],
        *,
        args: argparse.Namespace,
        session_dir: pathlib.Path,
        gremlin_id: str | None,
        pipeline_data: _PipelineData,
        repo: str = "",
        state_file: pathlib.Path | None = None,
        test_client: Client | None = None,
    ) -> None:
        unknown: list[str] = []
        for s in stages:
            if s.type not in STAGE_TYPES:
                unknown.append(s.type)
            elif s.type == "parallel":
                unknown.extend(c.type for c in s.body if c.type not in STAGE_TYPES)
            elif s.type == "loop":
                unknown.extend(c.type for c in s.body if c.type not in STAGE_TYPES)
        if unknown:
            raise ValueError(f"Pipeline does not support stage type(s): {unknown}")
        self.stages = _expand_stage_entries(stages)
        self.args = args
        self.session_dir = session_dir
        self.gremlin_id = gremlin_id
        self.pipeline_data = pipeline_data
        self.repo = repo
        self.state_file = state_file
        self.test_client = test_client

        sf = state_file if state_file is not None else resolve_state_file(gremlin_id)
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
            resolved = self.test_client or e.client or PACKAGE_DEFAULT
            stage_state = State(
                client=resolved,
                session_dir=self.session_dir,
                gremlin_id=self.gremlin_id,
                state_file=self.state_file,
                args=self.args,
                pipeline_data=self.pipeline_data,
                repo=self.repo,
                instructions=self.instructions,
                test_client=self.test_client,
            )
            built.append((e.name, stage_state.make_runner(e, scope=stages)))
        return built

    def run(self) -> None:
        built = self._collect_stages(self.stages)
        run_stages(built, resume_from=self.args.resume_from)
