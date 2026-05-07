"""Pipeline base class and shared utilities."""

from __future__ import annotations

import argparse
import dataclasses
import json
import logging
import pathlib
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
from gremlins.stages.base import Stage, StageContext
from gremlins.state import set_stage

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
        self, entry: StageEntry, ctx: StageContext, spec: ClientSpec
    ) -> Callable[[], None]:
        raise NotImplementedError

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
