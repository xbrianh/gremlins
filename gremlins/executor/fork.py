"""Fork a state into a new independent copy."""

from __future__ import annotations

import asyncio
import pathlib
import shutil

from gremlins import paths as _paths
from gremlins.artifacts.registry import ArtifactRegistry
from gremlins.executor.state import State, StateData, build_state
from gremlins.utils import git as _git_mod


async def fork_state(
    state: State,
    target_id: str,
    *,
    project_root: str,
    state_root: pathlib.Path | None = None,
    parent_id: str = "",
    group_name: str = "",
    child_key: str = "",
    worktree_parent: pathlib.Path | None = None,
) -> State:
    """Create an independent copy of a running state.

    Copies artifact directory, registry, and optionally creates a fresh
    worktree at the same commit SHA. Persists state.json with child identity
    fields so subprocess children can load their state correctly.
    """
    if state_root is None:
        state_root = _paths.state_root()
    if worktree_parent is None:
        worktree_parent = state.worktree_parent

    child_state_dir = state_root / target_id
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
            project_root, sha, worktree_parent=worktree_parent
        )
        child_worktree = pathlib.Path(child_worktree_path)

    # Touch log file like _init_child_dir does
    (child_state_dir / "log").touch()

    # Persist state.json with child identity fields
    child_data = StateData(
        gremlin_id=target_id,
        parent_id=parent_id,
        group_name=group_name,
        child_key=child_key,
        project_root=state.data.project_root,
        permissions_file=state.data.permissions_file,
        bypass=state.data.bypass,
        setup_kind=state.data.setup_kind,
        pipeline_path=state.data.pipeline_path,
        kind=state.data.kind,
    )
    child_data.persist(child_state_dir)

    # Load fresh registry from child's registry.json
    child_registry = ArtifactRegistry(
        artifact_dir=child_artifact_dir,
        cwd=child_worktree,
    )

    # Build new state with updated values
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
        child_key=child_key,
        parent_stage=state.parent_stage,
        base_ref=state.base_ref,
    )

    return child_state
