"""Gremlin: pipeline orchestrator."""

from __future__ import annotations

import asyncio
import dataclasses
import json
import logging
import os
import pathlib
import shutil
from collections.abc import Awaitable, Callable, Sequence
from typing import TYPE_CHECKING, Any, cast

from gremlins import paths as _paths
from gremlins.artifacts.registry import ArtifactRegistry
from gremlins.artifacts.uri import Uri
from gremlins.clients.client import PACKAGE_DEFAULT, Client
from gremlins.executor.state import State, StateData, build_state
from gremlins.pipeline import Pipeline as _PipelineData
from gremlins.pipeline.discovery import resolve_pipeline_path
from gremlins.pipeline.loader import STAGE_TYPES
from gremlins.protocols import StageProtocol
from gremlins.utils import git as _git_mod
from gremlins.utils.yaml_io import YamlLoadError as _YamlLoadError
from gremlins.utils.yaml_io import dump_yaml_text

if TYPE_CHECKING:
    from gremlins.stages.base import Stage

logger = logging.getLogger(__name__)


def _apply_client_override(stages: Sequence[StageProtocol], cli: Client) -> None:
    for stage in stages:
        stage.client = cli
        body = getattr(stage, "body", [])
        if body:
            _apply_client_override(body, cli)


def _collect_pipeline_model(stages: list[StageProtocol]) -> str | None:
    """Recursively collect a non-fake model from the pipeline stages."""
    for stage in stages:
        if stage.client and stage.client.model and stage.client.model != "fake":
            return stage.client.model
        body = getattr(stage, "body", [])
        if body:
            m = _collect_pipeline_model(body)
            if m:
                return m
    return None


def read_stage_inputs(sf: pathlib.Path | None) -> dict[str, Any]:
    if sf is None or not sf.exists():
        return {}
    try:
        data = json.loads(sf.read_text(encoding="utf-8"))
        return data.get("stage_inputs") or {}
    except Exception:
        return {}


def _expand_stage_entries(raw_stages: Sequence[StageProtocol]) -> list[StageProtocol]:
    top_level_names = {e.name for e in raw_stages}
    child_names: set[str] = set()
    seen: set[str] = set()
    result: list[StageProtocol] = []

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
    state: State | None

    def __init__(
        self,
        stages: list[Stage],
        *,
        state_dir: pathlib.Path,
        gremlin_id: str | None,
        pipeline_data: _PipelineData,
        worktree_dir: pathlib.Path | None = None,
        worktree_parent: pathlib.Path | None = None,
        resume_from: str | None = None,
        repo: str = "",
        state_file: pathlib.Path | None = None,
        project_root: str = "",
        base_ref_sha: str = "",
        base_ref: str = "",
        fetch_worktree: bool = False,
        pipeline_path: str = "",
        pipeline_args: list[str] | None = None,
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
        self.gremlin_id = gremlin_id
        self.pipeline_data = pipeline_data
        self.worktree_dir = worktree_dir
        self.worktree_parent = worktree_parent
        self.resume_from = resume_from
        self.repo = repo
        self.state_file = state_file
        self.project_root = project_root
        self.base_ref_sha = base_ref_sha
        self.base_ref = base_ref
        self.fetch_worktree = fetch_worktree
        self.pipeline_path = pipeline_path
        self.pipeline_args = pipeline_args or []
        self.state = None

    @property
    def artifact_dir(self) -> pathlib.Path:
        return self.state_dir / "artifacts"

    @property
    def state_data(self) -> StateData:
        return StateData.load(self.gremlin_id)

    @property
    def _cwd(self) -> str:
        return (
            str(self.worktree_dir)
            if self.worktree_dir is not None
            else (self.project_root or str(pathlib.Path.cwd()))
        )

    @property
    def finished(self) -> bool:
        return (self.state_dir / "finished").is_file()

    async def fork(
        self,
        state: State,
        target_id: str,
        *,
        parent_id: str = "",
        group_name: str = "",
        child_key: str = "",
        pipeline: _PipelineData | None = None,
    ) -> State:
        """Create an independent copy of a running gremlin.

        Copies artifact directory, registry, and optionally creates a fresh
        worktree at the same commit SHA. Persists state.json with child identity
        fields if provided.
        """
        child_state_dir = self.state_dir.parent / target_id
        child_artifact_dir = child_state_dir / "artifacts"

        # Copy artifact directory and registry in thread to avoid blocking event loop
        child_artifact_dir.parent.mkdir(parents=True, exist_ok=True)
        await asyncio.to_thread(
            shutil.copytree, state.artifact_dir, child_artifact_dir, dirs_exist_ok=True
        )

        # Copy registry.json from the same directory as source artifacts
        src_registry = state.artifact_dir.parent / "registry.json"
        if src_registry.exists():
            await asyncio.to_thread(
                shutil.copy2, src_registry, child_state_dir / "registry.json"
            )

        # Create new worktree if needed
        child_worktree = None
        if state.worktree is not None:
            sha = _git_mod.head_sha(cwd=state.worktree)
            if not sha:
                raise RuntimeError(f"could not resolve HEAD in {state.worktree}")
            child_worktree_path = await _git_mod.setup_detached_worktree_async(
                self.project_root, sha, worktree_parent=state.worktree_parent
            )
            child_worktree = pathlib.Path(child_worktree_path)

        # Load fresh registry from child's registry.json
        child_registry = ArtifactRegistry(
            artifact_dir=child_artifact_dir,
            cwd=child_worktree,
        )

        # Build new state with updated values
        effective_pipeline = pipeline or state.pipeline_data
        child_pipeline_path = state.data.pipeline_path
        if pipeline is not None:
            branch_yaml_path = child_state_dir / "pipeline.yaml"
            stage_dicts = [
                s.raw_dict for s in pipeline.stages if s.raw_dict is not None
            ]
            await asyncio.to_thread(
                branch_yaml_path.write_text,
                dump_yaml_text({"stages": stage_dicts}),
                encoding="utf-8",
            )
            child_pipeline_path = str(branch_yaml_path)

        child_data = dataclasses.replace(
            state.data,
            gremlin_id=target_id,
            parent_id=parent_id or state.data.parent_id,
            group_name=group_name or state.data.group_name,
            child_key=child_key or state.data.child_key,
            pipeline_path=child_pipeline_path,
        )
        child_cwd = state.cwd
        if child_worktree is not None and state.worktree is not None:
            child_cwd = str(child_worktree)
        child_state = build_state(
            data=child_data,
            client=state.client,
            artifact_dir=child_artifact_dir,
            args=state.args,
            pipeline_data=effective_pipeline,
            repo=state.repo,
            cwd=child_cwd,
            worktree=child_worktree,
            worktree_parent=state.worktree_parent,
            artifacts=child_registry,
            child_key=child_key or state.data.child_key,
            parent_stage=state.parent_stage,
            base_ref=state.base_ref,
        )

        # Persist state.json with child identity fields
        child_data.persist(child_state_dir)
        (child_state_dir / "log").touch()

        return child_state

    def validate_resume_target(self) -> None:
        if not self.resume_from:
            return
        valid_names = [entry.name for entry in self.stages]
        if self.resume_from not in valid_names:
            raise ValueError(
                f"resume from {self.resume_from!r} is not a valid stage; "
                f"valid: {valid_names}"
            )

    def _set_gremlin_recursive(self, stage: StageProtocol) -> None:
        stage.gremlin = self
        body = getattr(stage, "body", [])
        for nested in body:
            self._set_gremlin_recursive(nested)

    def _make_build_state_kwargs(
        self, data: StateData, client: Client
    ) -> dict[str, Any]:
        return {
            "data": data,
            "client": client,
            "artifact_dir": self.artifact_dir,
            "pipeline_data": self.pipeline_data,
            "repo": self.repo,
            "cwd": self._cwd,
            "worktree": self.worktree_dir,
            "worktree_parent": self.worktree_parent,
            "artifacts": self.registry,
            "base_ref": self.base_ref,
        }

    def _collect_stages(
        self, stages: Sequence[StageProtocol]
    ) -> list[tuple[str, Callable[[], Awaitable[Any]]]]:
        built: list[tuple[str, Callable[[], Awaitable[Any]]]] = []
        for e in stages:
            self._set_gremlin_recursive(e)
            stage_client = e.client or PACKAGE_DEFAULT
            stage_data = StateData(
                gremlin_id=self.gremlin_id, state_file=self.state_file
            )
            stage_state = build_state(
                **self._make_build_state_kwargs(stage_data, stage_client)
            )
            built.append(
                (e.name, stage_state.make_runner(e, scope=stages, gremlin=self))
            )
        return built

    def _unbind_stale_exec_artifacts(self) -> None:
        assert self.resume_from is not None
        names = [s.name for s in self.stages]
        start_idx = names.index(self.resume_from)
        for stage in self.stages[start_idx:]:
            if stage.type == "exec":
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
    def open(cls, gremlin_id: str) -> Gremlin:
        """Reconstruct a Gremlin from a persisted state directory.

        Loads state.json, resolves the pipeline, and returns a Gremlin instance
        without any side effects (no directory creation, no worktree setup).
        Raises FileNotFoundError if state directory is missing, ValueError if
        state.json is malformed or pipeline cannot be loaded.
        """
        from gremlins.cli.pipeline_args import resolve_pipeline

        state_dir = _paths.state_root() / gremlin_id
        sf = state_dir / "state.json"

        if not state_dir.is_dir():
            raise FileNotFoundError(f"no state at {state_dir}")
        if not sf.is_file():
            raise FileNotFoundError(f"no state.json at {sf}")

        try:
            state_raw = json.loads(sf.read_text(encoding="utf-8"))
        except Exception as exc:
            raise ValueError(f"could not parse state.json: {exc}") from exc

        if not isinstance(state_raw, dict):
            raise ValueError(
                f"state.json must be a JSON object, not {type(state_raw).__name__}"
            )
        state_raw = cast(dict[str, Any], state_raw)

        # Extract persisted fields from state.json
        kind = cast(str, state_raw.get("kind") or "")
        project_root = cast(str, state_raw.get("project_root") or _paths.project_root())
        pipeline_args = cast(list[str], state_raw.get("pipeline_args") or [])
        pipeline_path = cast(str, state_raw.get("pipeline_path") or "")
        worktree_dir_str = cast(str, state_raw.get("workdir") or "")

        # Resolve pipeline (hermetic check first, then fallback)
        hermetic = state_dir / "pipeline.yaml"
        if hermetic.is_file():
            pipeline_path = str(hermetic)
        elif kind:
            try:
                filtered, resolved = resolve_pipeline(
                    kind, tuple(pipeline_args), project_root
                )
                pipeline_args = filtered
                pipeline_path = resolved
            except FileNotFoundError:
                pass

        # Load pipeline (optional for status checks, required for execution)
        pipeline = None
        if pipeline_path or kind:
            try:
                pipeline = _PipelineData.from_yaml(
                    resolve_pipeline_path(
                        pipeline_path or kind, pathlib.Path(project_root)
                    )
                )
            except FileNotFoundError:
                # Pipeline not found (e.g., test or recovery scenario)
                # Create minimal stub pipeline to allow status checks
                pipeline = _PipelineData(
                    name=kind or "unknown",
                    path=pathlib.Path(pipeline_path)
                    if pipeline_path
                    else pathlib.Path("."),
                    stages=[],
                )
            except Exception as exc:
                raise ValueError(
                    f"could not load pipeline for {gremlin_id}: {exc}"
                ) from exc

        if pipeline is None:
            pipeline = _PipelineData(name="unknown", path=pathlib.Path("."), stages=[])

        # Construct Gremlin
        worktree_dir = pathlib.Path(worktree_dir_str) if worktree_dir_str else None

        return cls(
            pipeline.stages,
            state_dir=state_dir,
            gremlin_id=gremlin_id,
            pipeline_data=pipeline,
            worktree_dir=worktree_dir,
            project_root=project_root,
            pipeline_path=pipeline_path,
            pipeline_args=pipeline_args,
        )

    @classmethod
    def initialize_with_runtime(
        cls,
        *,
        gremlin_id: str | None,
        state_dir: pathlib.Path,
        project_dir: pathlib.Path,
        pipeline_ref: str,
        worktree_parent: pathlib.Path | None = None,
        resume_from: str | None = None,
        project_root: str = "",
        base_ref_sha: str = "",
        base_ref: str = "",
        fetch_worktree: bool = False,
        worktree_dir: pathlib.Path | None = None,
        client_label: str = "",
        repo: str = "",
        stage_inputs: dict[str, Any] | None = None,
        client: Client | None = None,
    ) -> Gremlin:
        try:
            pipeline_path = resolve_pipeline_path(pipeline_ref, project_dir)
            pipeline = _PipelineData.from_yaml(pipeline_path)
        except (FileNotFoundError, _YamlLoadError) as exc:
            raise ValueError(str(exc)) from exc
        resolved_client = None
        if client_label and client and client.provider == "fake":
            parsed = Client.parse(client_label)
            client.model = parsed.model
            resolved_client = client
        elif client_label:
            resolved_client = Client.parse(client_label)
        elif client:
            if client.provider == "fake":
                model_from_pipeline = _collect_pipeline_model(list(pipeline.stages))
                if model_from_pipeline and model_from_pipeline != client.model:
                    client.model = model_from_pipeline
                resolved_client = client
            else:
                resolved_client = client

        if resolved_client:
            _apply_client_override(list(pipeline.stages), resolved_client)
        self = cls(
            pipeline.stages,
            state_dir=state_dir,
            gremlin_id=gremlin_id,
            pipeline_data=pipeline,
            worktree_dir=worktree_dir,
            worktree_parent=worktree_parent,
            resume_from=resume_from,
            project_root=project_root,
            base_ref_sha=base_ref_sha,
            base_ref=base_ref,
            fetch_worktree=fetch_worktree,
            repo=repo,
        )

        State.setup_dirs(
            self.state_dir,
            self.artifact_dir,
            self.gremlin_id,
        )

        worktree_created: str | None = None
        try:
            if self.worktree_dir is None and self.project_root and self.gremlin_id:
                workdir = _git_mod.setup_workdir(
                    self.project_root,
                    self.base_ref_sha,
                    fetch=self.fetch_worktree,
                    state_dir=self.state_dir,
                    worktree_parent=self.worktree_parent,
                )
                worktree_created = workdir
                self.worktree_dir = pathlib.Path(workdir)
                st = StateData.load(self.gremlin_id)
                st.patch(
                    workdir=workdir,
                    worktree_base=self.base_ref_sha,
                    setup_kind="worktree-detached",
                )

            if self.worktree_dir is not None:
                os.chdir(self.worktree_dir)

            self.registry = ArtifactRegistry(
                artifact_dir=self.artifact_dir,
                cwd=self.worktree_dir,
            )
            for key, value in (stage_inputs or {}).items():
                if value is not None and not self.registry.produced(key):
                    self.registry.write(key, value)
            if not self.registry.produced("spec"):
                self.registry.bind("spec", Uri.parse("file://session/spec.md"))
            if not self.registry.produced("base_sha"):
                sha = _git_mod.head_sha(cwd=self.worktree_dir)
                if sha:
                    self.registry.bind("base_sha", Uri.parse(f"git://commit/{sha}"))

            state_data = StateData.load(self.gremlin_id)
            self.state = build_state(
                **self._make_build_state_kwargs(
                    state_data, resolved_client or PACKAGE_DEFAULT
                )
            )
        except Exception:
            if worktree_created:
                _git_mod.remove_worktree(self.project_root, worktree_created)
            raise

        return self
