"""Stage and bail bookkeeping for gremlin state.json.

``set_stage`` and ``emit_bail`` write state.json atomically in pure Python.
Both are no-ops outside a gremlin context (no ``gr_id`` or missing
state.json) and never raise — stage/bail bookkeeping must not break a
running gremlin.
"""

from __future__ import annotations

import datetime
import fcntl
import json
import os
import pathlib
import re
import secrets
from collections.abc import Callable
from typing import Any

from gremlins import paths as _paths

_GR_ID_RE = re.compile(r"^[A-Za-z0-9_-]+$")


def validate_gr_id(gr_id: str) -> None:
    """Raise ValueError if gr_id is not a safe, non-path-traversing identifier."""
    if ".." in gr_id or not _GR_ID_RE.match(gr_id):
        raise ValueError(f"gr_id contains illegal characters: {gr_id!r}")


# The four bail-class strings written to state.json.bail_class. Byte-stable
# across the migration — these strings appear in state.json files written by
# the old code that the new code must continue to read.
BAIL_CLASS_REVIEWER_REQUESTED_CHANGES = "reviewer_requested_changes"
BAIL_CLASS_SECURITY = "security"
BAIL_CLASS_SECRETS = "secrets"
BAIL_CLASS_OTHER = "other"


def _locked_update(sf: pathlib.Path, fn: Callable[[dict[str, Any]], None]) -> None:
    """Acquire an exclusive lock on sf.lock, read sf, apply fn(data), write sf atomically.

    Callers are responsible for the outer try/except — this may raise.
    """
    lock_path = sf.with_name(sf.name + ".lock")
    with open(lock_path, "a") as lock_f:
        fcntl.flock(lock_f, fcntl.LOCK_EX)
        data = json.loads(sf.read_text(encoding="utf-8"))
        fn(data)
        tmp = sf.with_name(f"{sf.name}.{os.getpid()}.{secrets.token_hex(8)}.tmp")
        tmp.write_text(json.dumps(data), encoding="utf-8")
        os.replace(tmp, sf)


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


def resolve_session_dir(gr_id: str | None = None) -> pathlib.Path:
    """Resolve the artifacts directory for the current run.

    Under a gremlin (``gr_id`` set), nests under
    ``$STATE_ROOT/<gr_id>/artifacts/`` so the launcher's state.json and the
    pipeline artifacts share a parent. Direct invocations (no ``gr_id``) nest
    under ``$STATE_ROOT/direct/<ts>-<rand>/artifacts/`` so they're visually
    separated from real gremlins and can be pruned on a simpler age-based
    heuristic.
    """
    state_root = _paths.state_root()
    if gr_id:
        session_dir = state_root / gr_id / "artifacts"
    else:
        ts = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
        rand = secrets.token_hex(3)  # 6 hex chars
        session_dir = state_root / "direct" / f"{ts}-{rand}" / "artifacts"
    session_dir.mkdir(parents=True, exist_ok=True)
    return session_dir


def emit_bail(
    gr_id: str | None,
    bail_class: str,
    bail_detail: str = "",
    *,
    child_key: str | None = None,
) -> None:
    """Write bail_class (and optional bail_detail) to state.json.

    When ``child_key`` is set, writes into ``parallel_bails[child_key]``
    instead of the top-level fields, isolating child bails from each other
    and from the top-level bail slot.

    No-op without gr_id, empty bail_class, or missing state.json.
    """
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


def resolve_state_file(gr_id: str | None) -> pathlib.Path | None:
    """Return path to state.json for gr_id, or None when gr_id is absent."""
    if not gr_id:
        return None
    state_root = _paths.state_root()
    return state_root / gr_id / "state.json"


def patch_state(
    gr_id: str | None, _delete: tuple[str, ...] = (), **fields: object
) -> None:
    """Merge keyword fields into state.json atomically under an exclusive file lock.

    No-op when gr_id is unset, when state.json doesn't exist, or when the
    write fails — stage bookkeeping must not crash a running gremlin.
    """
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
    """Set or clear ``parallel_worktrees[group_name]`` in state.json.

    Pass ``None`` for both ``base_head`` and ``paths`` to remove the entry
    for ``group_name``. No-op without ``gr_id`` or when state.json is missing.
    """
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
        pass


def read_artifacts(gr_id: str | None) -> list[dict[str, Any]]:
    sf = resolve_state_file(gr_id)
    if sf is None or not sf.exists():
        return []
    try:
        data: dict[str, Any] = json.loads(sf.read_text(encoding="utf-8"))
        return list(data.get("artifacts") or [])
    except (json.JSONDecodeError, OSError):
        return []


def last_artifact_branch(gr_id: str | None) -> str:
    for art in reversed(read_artifacts(gr_id)):
        if art.get("type") == "branch":
            return str(art.get("name") or "")
        if art.get("type") == "pr":
            return str(art.get("branch") or "")
    return ""


def check_bail(
    gr_id: str | None,
    label: str = "stage",
    *,
    child_key: str | None = None,
) -> None:
    """Raise RuntimeError if a bail_class was written to state.json by the
    just-completed stage.

    When ``child_key`` is set, checks ``parallel_bails[child_key]`` instead
    of the top-level ``bail_class`` field, so parallel children don't see
    each other's bails.

    No-op without gr_id or when state.json is absent.
    """
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


