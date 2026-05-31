"""Gremlin: pipeline orchestrator."""

from __future__ import annotations

import argparse
import asyncio
import dataclasses
import json
import logging
import os
import pathlib
import re
import shutil
from collections.abc import Awaitable, Callable, Sequence
from typing import Any, cast

from gremlins import paths as _paths
from gremlins.artifacts.registry import ArtifactRegistry
from gremlins.artifacts.uri import Uri
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
        gremlin_id: str | None,
        pipeline_data: _PipelineData,
        worktree_dir: pathlib.Path | None = None,
        worktree_parent: pathlib.Path | None = None,
        resume_from: str | None = None,
        instructions: str = "",
        spec: str | None = None,
        plan: str | None = None,
        repo: str = "",
        state_file: pathlib.Path | None = None,
        test_client: Client | None = None,
        project_root: str = "",
        base_ref_sha: str = "",
        base_ref: str = "",
        fetch_worktree: bool = False,
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
        self.instructions = instructions
        self.spec = spec
        self.plan = plan
        self.repo = repo
        self.state_file = state_file
        self.test_client = test_client
        self.project_root = project_root
        self.base_ref_sha = base_ref_sha
        self.base_ref = base_ref
        self.fetch_worktree = fetch_worktree

    @property
    def artifact_dir(self) -> pathlib.Path:
        return self.state_dir / "artifacts"

    async def fork(self, state: State, target_id: str) -> State:
        """Create an independent copy of a running gremlin.

        Copies artifact directory, registry, and optionally creates a fresh
        worktree at the same commit SHA.
        """
        child_state_dir = self.state_dir.parent / target_id
        child_artifact_dir = child_state_dir / "artifacts"

        # Copy artifact directory and registry in thread to avoid blocking event loop
        child_artifact_dir.parent.mkdir(parents=True, exist_ok=True)
        await asyncio.to_thread(shutil.copytree, state.artifact_dir, child_artifact_dir)

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
        child_data = dataclasses.replace(state.data, gremlin_id=target_id)
        child_cwd = state.cwd
        if child_worktree is not None and state.worktree is not None:
            child_cwd = str(child_worktree)
        child_state = build_state(
            data=child_data,
            client=state.client,
            artifact_dir=child_artifact_dir,
            args=state.args,
            pipeline_data=state.pipeline_data,
            repo=state.repo,
            cwd=child_cwd,
            instructions=state.instructions,
            test_client=state.test_client,
            stage_model=state.stage_model,
            worktree=child_worktree,
            worktree_parent=state.worktree_parent,
            artifacts=child_registry,
            child_key=state.child_key,
            parent_stage=state.parent_stage,
            base_ref=state.base_ref,
        )

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

    def _collect_stages(
        self, stages: list[Stage]
    ) -> list[tuple[str, Callable[[], Awaitable[Any]]]]:
        args = argparse.Namespace(
            plan=self.plan,
            resume_from=self.resume_from,
            spec=self.spec,
            instructions=[self.instructions] if self.instructions else [],
        )
        cwd = (
            str(self.worktree_dir)
            if self.worktree_dir is not None
            else (self.project_root or str(pathlib.Path.cwd()))
        )
        built: list[tuple[str, Callable[[], Awaitable[Any]]]] = []
        for e in stages:
            stage_client = e.client or PACKAGE_DEFAULT
            resolved = self.test_client or stage_client
            stage_state = build_state(
                data=StateData(gremlin_id=self.gremlin_id, state_file=self.state_file),
                client=resolved,
                artifact_dir=self.artifact_dir,
                args=args,
                pipeline_data=self.pipeline_data,
                repo=self.repo,
                cwd=cwd,
                instructions=self.instructions,
                test_client=self.test_client,
                stage_model=stage_client.model if self.test_client else "",
                worktree=self.worktree_dir,
                worktree_parent=self.worktree_parent,
                artifacts=self.registry,
                base_ref=self.base_ref,
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
        instructions = cast(str, state_raw.get("instructions") or "")

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

        # Load pipeline (required for reconstruction)
        pipeline = None
        if pipeline_path or kind:
            try:
                pipeline = _PipelineData.from_yaml(
                    resolve_pipeline_path(
                        pipeline_path or kind, pathlib.Path(project_root)
                    )
                )
            except Exception as exc:
                logger.debug(f"failed to load pipeline for {gremlin_id}: {exc}")

        if pipeline is None:
            raise ValueError(f"could not load pipeline for {gremlin_id}")

        # Construct Gremlin
        worktree_dir = pathlib.Path(worktree_dir_str) if worktree_dir_str else None

        return cls(
            pipeline.stages,
            state_dir=state_dir,
            gremlin_id=gremlin_id,
            pipeline_data=pipeline,
            worktree_dir=worktree_dir,
            instructions=instructions,
            project_root=project_root,
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
        instructions: str = "",
        resume_from: str | None = None,
        plan: str | None = None,
        spec: str | None = None,
        test_client: Client | None = None,
        project_root: str = "",
        base_ref_sha: str = "",
        base_ref: str = "",
        fetch_worktree: bool = False,
        worktree_dir: pathlib.Path | None = None,
        client_label: str = "",
        repo: str = "",
        stage_inputs: dict[str, Any] | None = None,
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
            gremlin_id=gremlin_id,
            pipeline_data=pipeline,
            worktree_dir=worktree_dir,
            worktree_parent=worktree_parent,
            resume_from=resume_from,
            instructions=instructions,
            spec=spec,
            plan=plan,
            test_client=test_client,
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
            instructions=self.instructions or "",
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

            if self.spec:
                spec_file = self.artifact_dir / "spec.md"
                if not spec_file.exists():
                    spec_src = pathlib.Path(self.spec)
                    if not spec_src.is_file():
                        raise ValueError(f"--spec: file not found: {self.spec}")
                    if spec_src.stat().st_size == 0:
                        raise ValueError(f"--spec: file is empty: {self.spec}")
                    shutil.copyfile(spec_src, spec_file)

            if self.plan and not self.pipeline_data.github_integration:
                plan_file = self.artifact_dir / "plan.md"
                if not plan_file.exists():
                    src = pathlib.Path(self.plan)
                    if src.is_file():
                        shutil.copyfile(src, plan_file)

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
            # When --plan is a GH issue ref on a github_integration pipeline,
            # the opaque issue URI is what compose-pr's plan.uri? needs.
            plan_issue_uri: str | None = None
            if self.pipeline_data.github_integration and self.plan:
                m = re.match(
                    r"^(?:[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+)?#([0-9]+)$", self.plan
                )
                if m:
                    plan_issue_uri = f"gh://issue/{m.group(1)}"

            if not self.registry.produced("plan"):
                if (self.artifact_dir / "plan.md").exists():
                    if not self.pipeline_data.github_integration:
                        self.registry.bind("plan", Uri.parse("file://session/plan.md"))
                    elif plan_issue_uri is not None:
                        self.registry.bind("plan", Uri.parse(plan_issue_uri))
                    elif self.registry.produced("plan-issue-number"):
                        n = str(self.registry.read("plan-issue-number")).strip()
                        self.registry.bind("plan", Uri.parse(f"gh://issue/{n}"))
                    # else: github_integration with no issue ref/number yet —
                    # publish-as-issue will bind plan (avoid DuplicateArtifact).
            elif (
                plan_issue_uri is not None
                and self.registry.resolve("plan").scheme == "file"
            ):
                # resume: upgrade an existing file:// plan to the issue URI so
                # compose-pr resolves it even when the plan stage was skipped.
                self.registry.unbind("plan")
                self.registry.bind("plan", Uri.parse(plan_issue_uri))
        except Exception:
            if worktree_created:
                _git_mod.remove_worktree(self.project_root, worktree_created)
            raise

        return self
