"""Gremlin: pipeline orchestrator."""

from __future__ import annotations

import argparse
import json
import logging
import os
import pathlib
import shutil
from collections.abc import Awaitable, Callable, Sequence
from typing import Any

from gremlins.artifacts.engine import EngineContext
from gremlins.artifacts.registry import ArtifactRegistry
from gremlins.clients.client import PACKAGE_DEFAULT, Client
from gremlins.executor.state import State, StateData, build_state
from gremlins.pipeline import Pipeline as _PipelineData
from gremlins.pipeline.discovery import resolve_pipeline_path
from gremlins.pipeline.loader import STAGE_TYPES
from gremlins.stages.base import Stage
from gremlins.utils import git as _git_mod
from gremlins.utils.yaml_io import YamlLoadError as _YamlLoadError

logger = logging.getLogger(__name__)


def _apply_client_override(stages: list[Stage], cli: Client) -> None:
    for stage in stages:
        stage.client = cli
        body = getattr(stage, "body", [])
        if body:
            _apply_client_override(body, cli)


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


async def run_stages(
    stages: Sequence[tuple[str, Callable[[], Awaitable[Any]]]],
    *,
    resume_from: str | None = None,
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
        await fn()


class Gremlin:
    registry: ArtifactRegistry

    def __init__(
        self,
        stages: list[Stage],
        *,
        state_dir: pathlib.Path,
        session_dir: pathlib.Path,
        gremlin_id: str | None,
        pipeline_data: _PipelineData,
        worktree_dir: pathlib.Path | None = None,
        worktree_parent: pathlib.Path | None = None,
        resume_from: str | None = None,
        instructions: str = "",
        spec: str | None = None,
        plan: str | None = None,
        cmds: list[str] | None = None,
        test_max_attempts: int = 3,
        repo: str = "",
        state_file: pathlib.Path | None = None,
        test_client: Client | None = None,
        project_root: str = "",
        base_ref_sha: str = "",
        setup_kind: str = "worktree-branch",
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
            raise ValueError(f"Gremlin does not support stage type(s): {unknown}")

        self.stages = _expand_stage_entries(stages)
        for s in self.stages:
            if not s.path:
                s.path = s.name
        self.state_dir = state_dir
        self.session_dir = session_dir
        self.gremlin_id = gremlin_id
        self.pipeline_data = pipeline_data
        self.worktree_dir = worktree_dir
        self.worktree_parent = worktree_parent
        self.resume_from = resume_from
        self.instructions = instructions
        self.spec = spec
        self.plan = plan
        self.cmds = cmds
        self.test_max_attempts = test_max_attempts
        self.repo = repo
        self.state_file = state_file
        self.test_client = test_client
        self.project_root = project_root
        self.base_ref_sha = base_ref_sha
        self.setup_kind = setup_kind

    def validate_resume_target(self) -> None:
        if not self.resume_from:
            return
        valid_names = [entry.name for entry in self.stages]
        if self.resume_from not in valid_names:
            raise ValueError(
                f"--resume-from {self.resume_from!r} is not a valid stage; "
                f"valid: {valid_names}"
            )

    def _collect_stages(
        self, stages: list[Stage]
    ) -> list[tuple[str, Callable[[], Awaitable[Any]]]]:
        args = argparse.Namespace(
            plan=self.plan,
            cmds=self.cmds,
            test_max_attempts=self.test_max_attempts,
            resume_from=self.resume_from,
            spec=self.spec,
            instructions=[self.instructions] if self.instructions else [],
        )
        # attempt is always "" here; the loop patches it per-iteration via dataclasses.replace.
        engine_ctx = EngineContext(loop_iteration=1, attempt="", current_scope=())
        built: list[tuple[str, Callable[[], Awaitable[Any]]]] = []
        for e in stages:
            stage_client = e.client or PACKAGE_DEFAULT
            resolved = self.test_client or stage_client
            stage_state = build_state(
                data=StateData(gremlin_id=self.gremlin_id, state_file=self.state_file),
                client=resolved,
                session_dir=self.session_dir,
                args=args,
                pipeline_data=self.pipeline_data,
                repo=self.repo,
                instructions=self.instructions,
                test_client=self.test_client,
                stage_model=stage_client.model if self.test_client else "",
                worktree_parent=self.worktree_parent,
                artifacts=self.registry,
                engine_ctx=engine_ctx,
            )
            built.append((e.name, stage_state.make_runner(e, scope=stages)))
        return built

    def _unbind_stale_exec_artifacts(self) -> None:
        from gremlins.stages.exec import Exec

        assert self.resume_from is not None
        names = [s.name for s in self.stages]
        start_idx = names.index(self.resume_from)
        for stage in self.stages[start_idx:]:
            if isinstance(stage, Exec):
                for key in stage.out_map:
                    if self.registry.produced(key):
                        self.registry.unbind(key)

    async def run(self) -> None:
        if not hasattr(self, "registry"):
            raise RuntimeError("call initialize_with_runtime() before run()")
        if self.resume_from is not None:
            self._unbind_stale_exec_artifacts()
        built = self._collect_stages(self.stages)
        await run_stages(built, resume_from=self.resume_from)

    @classmethod
    def initialize_with_runtime(
        cls,
        *,
        gremlin_id: str | None,
        state_dir: pathlib.Path,
        project_dir: pathlib.Path,
        pipeline_ref: str,
        session_dir: pathlib.Path | None = None,
        worktree_parent: pathlib.Path | None = None,
        instructions: str = "",
        resume_from: str | None = None,
        plan: str | None = None,
        spec: str | None = None,
        cmds: list[str] | None = None,
        test_max_attempts: int = 3,
        test_client: Client | None = None,
        project_root: str = "",
        base_ref_sha: str = "",
        setup_kind: str = "worktree-branch",
        worktree_dir: pathlib.Path | None = None,
        client_label: str = "",
    ) -> Gremlin:
        try:
            pipeline_path = resolve_pipeline_path(pipeline_ref, project_dir)
            pipeline = _PipelineData.from_yaml(pipeline_path)
        except (FileNotFoundError, _YamlLoadError) as exc:
            raise ValueError(str(exc)) from exc
        if client_label:
            _apply_client_override(list(pipeline.stages), Client.parse(client_label))
        self = cls(
            pipeline.stages,
            state_dir=state_dir,
            session_dir=session_dir
            if session_dir is not None
            else state_dir / "artifacts",
            gremlin_id=gremlin_id,
            pipeline_data=pipeline,
            worktree_dir=worktree_dir,
            worktree_parent=worktree_parent,
            resume_from=resume_from,
            instructions=instructions,
            spec=spec,
            plan=plan,
            cmds=cmds,
            test_max_attempts=test_max_attempts,
            test_client=test_client,
            project_root=project_root,
            base_ref_sha=base_ref_sha,
            setup_kind=setup_kind,
        )

        State.setup_dirs(
            self.state_dir,
            self.session_dir,
            self.gremlin_id,
            instructions=self.instructions or "",
        )

        worktree_created: str | None = None
        try:
            if self.worktree_dir is None and self.project_root and self.gremlin_id:
                workdir, branch, worktree_base, actual_setup_kind = (
                    _git_mod.setup_workdir(
                        self.setup_kind,
                        self.project_root,
                        self.base_ref_sha,
                        self.gremlin_id,
                        self.state_dir,
                        worktree_parent=self.worktree_parent,
                    )
                )
                worktree_created = workdir
                self.worktree_dir = pathlib.Path(workdir)
                st = StateData.load(self.gremlin_id)
                st.patch(
                    workdir=workdir,
                    worktree_base=worktree_base,
                    setup_kind=actual_setup_kind,
                )
                if actual_setup_kind == "worktree-branch" and branch:
                    st.append_artifact({"type": "branch", "name": branch})

            if self.spec:
                spec_file = self.session_dir / "spec.md"
                if not spec_file.exists():
                    spec_src = pathlib.Path(self.spec)
                    if not spec_src.is_file():
                        raise ValueError(f"--spec: file not found: {self.spec}")
                    if spec_src.stat().st_size == 0:
                        raise ValueError(f"--spec: file is empty: {self.spec}")
                    shutil.copyfile(spec_src, spec_file)

            if self.plan and not self.pipeline_data.needs_gh():
                plan_file = self.session_dir / "plan.md"
                if not plan_file.exists():
                    src = pathlib.Path(self.plan)
                    if src.is_file():
                        shutil.copyfile(src, plan_file)

            if self.worktree_dir is not None:
                os.chdir(self.worktree_dir)

            self.registry = ArtifactRegistry(
                session_dir=self.session_dir,
                cwd=self.worktree_dir,
            )
        except Exception:
            if worktree_created:
                _git_mod.remove_worktree(self.project_root, worktree_created)
            raise

        return self
