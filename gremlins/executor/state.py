"""Gremlin execution context and state.json I/O.

State is the single per-invocation object passed to every Stage.run().  It
carries both the runtime execution context (client, session_dir, etc.) and
the state.json I/O methods (patch, emit_bail, check_bail, etc.).

Module-level helpers (validate_gr_id, pipeline_uses_gh, landable_shape, and
the raw free-function forms of every I/O primitive) remain importable directly
from this module so non-stage callers (bail.py, fleet/, cli/) that have a gr_id
but no State instance can call them without constructing a full State.
"""

from __future__ import annotations

import argparse
import dataclasses
import datetime
import json
import logging
import pathlib
import re
import secrets
from collections.abc import Callable
from typing import TYPE_CHECKING, Any

from gremlins import paths as _paths
from gremlins.clients.client import Client
from gremlins.clients.protocol import CompletedRun
from gremlins.utils.state_file import _locked_update

if TYPE_CHECKING:
    from gremlins.pipeline import Pipeline
    from gremlins.stages.base import Stage
    from gremlins.utils.git import PreImplState

logger = logging.getLogger(__name__)

_GR_ID_RE = re.compile(r"^[A-Za-z0-9_-]+$")

_GH_STAGE_TYPES = frozenset(
    {
        "open-github-pr",
        "request-copilot",
        "wait-copilot",
        "wait-ci",
    }
)

BAIL_CLASS_REVIEWER_REQUESTED_CHANGES = "reviewer_requested_changes"
BAIL_CLASS_SECURITY = "security"
BAIL_CLASS_SECRETS = "secrets"
BAIL_CLASS_OTHER = "other"


def validate_gr_id(gr_id: str) -> None:
    """Raise ValueError if gr_id is not a safe, non-path-traversing identifier."""
    if ".." in gr_id or not _GR_ID_RE.match(gr_id):
        raise ValueError(f"gr_id contains illegal characters: {gr_id!r}")


def _stages_use_gh(stages: list[Stage]) -> bool:
    return any(
        s.type in _GH_STAGE_TYPES or (s.body and _stages_use_gh(s.body)) for s in stages
    )


def pipeline_uses_gh(pipeline: Pipeline) -> bool:
    return _stages_use_gh(pipeline.stages)


def resolve_state_file(gr_id: str | None) -> pathlib.Path | None:
    """Return path to state.json for gr_id, or None when gr_id is absent."""
    if not gr_id:
        return None
    return _paths.state_root() / gr_id / "state.json"


def resolve_session_dir(gr_id: str | None = None) -> pathlib.Path:
    """Resolve the artifacts directory for the current run."""
    state_root = _paths.state_root()
    if gr_id:
        session_dir = state_root / gr_id / "artifacts"
    else:
        ts = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
        rand = secrets.token_hex(3)
        session_dir = state_root / "direct" / f"{ts}-{rand}" / "artifacts"
    session_dir.mkdir(parents=True, exist_ok=True)
    return session_dir


def set_stage(
    gr_id: str | None,
    stage: str,
    sub_stage: object = None,
) -> None:
    """Write stage and optional sub-stage to state.json."""
    try:
        if not stage or not gr_id:
            return
        sf = resolve_state_file(gr_id)
        if sf is None or not sf.exists():
            return
        now = datetime.datetime.now(datetime.UTC).strftime("%Y-%m-%dT%H:%M:%SZ")
        if sub_stage is not None:
            patch_state(gr_id, stage=stage, stage_updated_at=now, sub_stage=sub_stage)
        else:
            patch_state(
                gr_id, _delete=("sub_stage",), stage=stage, stage_updated_at=now
            )
    except Exception:
        pass


def emit_bail(
    gr_id: str | None,
    bail_class: str,
    bail_detail: str = "",
    *,
    child_key: str | None = None,
) -> None:
    """Write bail_class (and optional bail_detail) to state.json."""
    try:
        if not gr_id or not bail_class:
            return
        sf = resolve_state_file(gr_id)
        if sf is None or not sf.exists():
            return
        if child_key is None:
            if bail_detail:
                patch_state(gr_id, bail_class=bail_class, bail_detail=bail_detail)
            else:
                patch_state(gr_id, _delete=("bail_detail",), bail_class=bail_class)
        else:
            shard: dict[str, str] = {"bail_class": bail_class}
            if bail_detail:
                shard["bail_detail"] = bail_detail

            def _merge(data: dict[str, Any]) -> None:
                shards: dict[str, Any] = dict(data.get("parallel_bails") or {})
                shards[child_key] = shard
                data["parallel_bails"] = shards

            _locked_update(sf, _merge)
    except Exception:
        pass


def read_state_str(state_file: pathlib.Path | None, field: str) -> str:
    """Return a string field from state.json, or '' on missing file or error."""
    if state_file is None or not state_file.exists():
        return ""
    try:
        return json.loads(state_file.read_text(encoding="utf-8")).get(field) or ""
    except Exception:
        return ""


def patch_state(
    gr_id: str | None, _delete: tuple[str, ...] = (), **fields: object
) -> None:
    """Merge keyword fields into state.json atomically under an exclusive file lock."""
    sf = resolve_state_file(gr_id)
    if sf is None or not sf.exists():
        return
    try:

        def _apply(data: dict[str, Any]) -> None:
            for key in _delete:
                data.pop(key, None)
            data.update(fields)

        _locked_update(sf, _apply)
    except Exception:
        pass


def patch_parallel_worktrees(
    gr_id: str | None,
    group_name: str,
    *,
    base_head: str | None,
    paths: dict[str, str] | None,
) -> None:
    """Set or clear ``parallel_worktrees[group_name]`` in state.json."""
    if not gr_id or not group_name:
        return
    sf = resolve_state_file(gr_id)
    if sf is None or not sf.exists():
        return
    try:

        def _apply(data: dict[str, Any]) -> None:
            groups: dict[str, Any] = dict(data.get("parallel_worktrees") or {})
            if base_head is None and paths is None:
                groups.pop(group_name, None)
            else:
                groups[group_name] = {
                    "base_head": base_head or "",
                    "paths": dict(paths or {}),
                }
            if groups:
                data["parallel_worktrees"] = groups
            else:
                data.pop("parallel_worktrees", None)

        _locked_update(sf, _apply)
    except Exception:
        pass


def read_pr_url(gr_id: str | None) -> str:
    for art in reversed(read_artifacts(gr_id)):
        if art.get("type") == "pr":
            return str(art.get("url") or "")
    return ""


def last_pr_branch(gr_id: str | None) -> str:
    for art in reversed(read_artifacts(gr_id)):
        if art.get("type") == "pr":
            return str(art.get("branch") or "")
    return ""


def read_pr_num(gr_id: str | None) -> str:
    url = read_pr_url(gr_id)
    return url.split("/")[-1] if url else ""


def append_artifact(gr_id: str | None, artifact: dict[str, Any]) -> None:
    sf = resolve_state_file(gr_id)
    if sf is None or not sf.exists():
        return
    try:

        def _apply(data: dict[str, Any]) -> None:
            arts: list[Any] = list(data.get("artifacts") or [])
            arts.append(artifact)
            data["artifacts"] = arts

        _locked_update(sf, _apply)
    except Exception:
        logger.warning("failed to append artifact", exc_info=True)


def read_artifacts(gr_id: str | None) -> list[dict[str, Any]]:
    sf = resolve_state_file(gr_id)
    if sf is None or not sf.exists():
        return []
    try:
        data: dict[str, Any] = json.loads(sf.read_text(encoding="utf-8"))
        artifacts: list[Any] = data.get("artifacts") or []
        return [a for a in artifacts if isinstance(a, dict)]
    except (json.JSONDecodeError, OSError):
        return []


def last_artifact_branch(gr_id: str | None) -> str:
    for art in reversed(read_artifacts(gr_id)):
        if art.get("type") == "branch":
            return str(art.get("name") or "")
        if art.get("type") == "pr":
            return str(art.get("branch") or "")
    return ""


def landable_shape(state: dict[str, Any]) -> str:
    """Classify artifact shape for land dispatch."""
    artifacts = list(state.get("artifacts") or [])
    branches: dict[str, bool] = {}
    prs: list[dict[str, Any]] = []

    for art in artifacts:
        if art.get("type") == "branch":
            name = str(art.get("name") or "")
            if name and name not in branches:
                branches[name] = False
        elif art.get("type") == "pr":
            prs.append(art)
            branch = str(art.get("branch") or "")
            if branch in branches:
                branches[branch] = True

    unmerged = [n for n, has_pr in branches.items() if not has_pr]

    if not prs and not unmerged:
        return "empty"
    if not prs and len(unmerged) == 1:
        return "one_branch"
    if not prs:
        return "many_branches"
    if unmerged:
        return "many_branches"
    if len(prs) == 1:
        return "one_pr"
    return "many_prs"


def check_bail(
    gr_id: str | None,
    label: str = "stage",
    *,
    child_key: str | None = None,
) -> None:
    """Raise RuntimeError if a bail_class was written to state.json."""
    sf = resolve_state_file(gr_id)
    if sf is None or not sf.exists():
        return
    try:
        data: dict[str, Any] = json.loads(sf.read_text(encoding="utf-8"))
        if child_key is None:
            bail_class = data.get("bail_class", "")
        else:
            parallel_bails: dict[str, Any] = data.get("parallel_bails") or {}
            shard: dict[str, Any] = parallel_bails.get(child_key) or {}
            bail_class = shard.get("bail_class", "")
        if bail_class:
            detail_path = (
                f"parallel_bails[{child_key!r}].bail_detail"
                if child_key is not None
                else "bail_detail"
            )
            raise RuntimeError(
                f"{label} bailed: bail_class={bail_class} (see state.json {detail_path})"
            )
    except RuntimeError:
        raise
    except Exception:
        pass


def _client_dict() -> dict[str, Client]:
    return {}


def _stage_list() -> list[Stage]:
    return []


def _read_state_json(sf: pathlib.Path | None) -> dict[str, Any]:
    if sf is None or not sf.exists():
        return {}
    try:
        return json.loads(sf.read_text(encoding="utf-8"))
    except json.JSONDecodeError:
        return {}


def _read_impl_pre_state(
    session_dir: pathlib.Path, sd: dict[str, Any]
) -> PreImplState | None:
    from gremlins.utils.git import PreImplState

    sidecar = session_dir / ".impl-pre-state.json"
    if sidecar.exists():
        try:
            data: dict[str, Any] = json.loads(sidecar.read_text(encoding="utf-8"))
            head = data.get("head") or ""
            if head:
                return PreImplState(head=head)
        except (json.JSONDecodeError, OSError):
            pass
    head = sd.get("impl_pre_head") or ""
    if head:
        return PreImplState(head=head)
    return None


@dataclasses.dataclass
class State:
    # required per-stage
    client: Client
    session_dir: pathlib.Path
    # pipeline-wide (all have defaults so tests can omit them)
    gr_id: str | None = None
    state_file: pathlib.Path | None = None
    args: argparse.Namespace = dataclasses.field(default_factory=argparse.Namespace)
    pipeline_data: Pipeline | None = None
    repo: str = ""
    instructions: str = ""
    stage_specs: dict[str, Client] = dataclasses.field(default_factory=_client_dict)
    spec_clients: dict[str, Client] = dataclasses.field(default_factory=_client_dict)
    is_git: bool = False
    test_client: Client | None = None
    # per-stage optional
    child_key: str | None = None
    worktree: pathlib.Path | None = None
    current_scope: list[Stage] = dataclasses.field(default_factory=_stage_list)
    # runtime-derived (populated from state.json before each stage run)
    issue_url: str = ""
    base_ref_name: str = ""
    issue_num: str = ""
    impl_pre_state: PreImplState | None = None

    @property
    def cwd(self) -> pathlib.Path:
        return self.worktree if self.worktree is not None else pathlib.Path.cwd()

    def get_client(self, spec: Client) -> Client:
        if self.test_client is not None:
            return self.test_client
        return self.spec_clients.get(str(spec), spec)

    def make_runner(
        self,
        entry: Stage,
        scope: list[Stage] | None = None,
    ) -> Callable[[], None]:
        base_state = self
        gr_id = self.gr_id
        scope_list = list(scope) if scope is not None else []

        def _run() -> None:
            set_stage(gr_id, entry.name)
            sf = (
                base_state.state_file
                if base_state.state_file is not None
                else resolve_state_file(gr_id)
            )
            sd = _read_state_json(sf)
            state = dataclasses.replace(
                base_state,
                current_scope=scope_list,
                issue_url=sd.get("issue_url") or "",
                base_ref_name=sd.get("base_ref_name") or "",
                issue_num=sd.get("issue_num") or "",
                impl_pre_state=_read_impl_pre_state(base_state.session_dir, sd),
            )
            entry.run(state)

        return _run

    # --- state.json I/O methods ---

    def patch(self, _delete: tuple[str, ...] = (), **fields: object) -> None:
        patch_state(self.gr_id, _delete=_delete, **fields)

    def read_str(self, field: str) -> str:
        return read_state_str(self.state_file, field)

    def set_stage(self, stage: str, sub_stage: object = None) -> None:
        set_stage(self.gr_id, stage, sub_stage)

    def emit_bail(self, bail_class: str, bail_detail: str = "") -> None:
        emit_bail(self.gr_id, bail_class, bail_detail, child_key=self.child_key)

    def check_bail(self, label: str = "stage") -> None:
        check_bail(self.gr_id, label, child_key=self.child_key)

    def append_artifact(self, artifact: dict[str, Any]) -> None:
        append_artifact(self.gr_id, artifact)

    def read_pr_url(self) -> str:
        return read_pr_url(self.gr_id)

    def last_pr_branch(self) -> str:
        return last_pr_branch(self.gr_id)

    def read_pr_num(self) -> str:
        return read_pr_num(self.gr_id)
