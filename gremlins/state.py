"""Stage and bail bookkeeping for gremlin state.json.

Both helpers write state.json atomically in pure Python. Both are no-ops
outside a gremlin context (no ``gr_id`` or missing state.json) and never
raise — stage/bail bookkeeping must not break a running gremlin.
"""

from __future__ import annotations

import datetime
import json
import os
import pathlib
import re
import secrets

_GR_ID_RE = re.compile(r"^[A-Za-z0-9._-]+$")


def validate_gr_id(gr_id: str) -> None:
    """Raise ValueError if gr_id is not a safe, non-path-traversing identifier."""
    if not gr_id:
        raise ValueError("gr_id must be non-empty")
    if "/" in gr_id or "\\" in gr_id or ".." in gr_id:
        raise ValueError(f"gr_id contains illegal characters: {gr_id!r}")
    if not _GR_ID_RE.match(gr_id):
        raise ValueError(f"gr_id contains illegal characters: {gr_id!r}")


# The four bail-class strings written to state.json.bail_class. Byte-stable
# across the migration — these strings appear in state.json files written by
# the old code that the new code must continue to read.
BAIL_CLASS_REVIEWER_REQUESTED_CHANGES = "reviewer_requested_changes"
BAIL_CLASS_SECURITY = "security"
BAIL_CLASS_SECRETS = "secrets"
BAIL_CLASS_OTHER = "other"


def set_stage(gr_id: str | None, stage: str, sub_stage: object = None) -> None:
    """Write stage and stage_updated_at to state.json. No-op without gr_id, empty stage, or missing state.json."""
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
    state_root = (
        pathlib.Path(
            os.environ.get("XDG_STATE_HOME")
            or os.path.join(os.path.expanduser("~"), ".local", "state")
        )
        / "claude-gremlins"
    )
    if gr_id:
        session_dir = state_root / gr_id / "artifacts"
    else:
        ts = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
        rand = secrets.token_hex(3)  # 6 hex chars
        session_dir = state_root / "direct" / f"{ts}-{rand}" / "artifacts"
    session_dir.mkdir(parents=True, exist_ok=True)
    return session_dir


def emit_bail(gr_id: str | None, bail_class: str, bail_detail: str = "") -> None:
    """Write bail_class (and optional bail_detail) to state.json. No-op without gr_id, empty bail_class, or missing state.json."""
    try:
        if not gr_id or not bail_class:
            return
        sf = resolve_state_file(gr_id)
        if sf is None or not sf.exists():
            return
        if bail_detail:
            patch_state(gr_id, bail_class=bail_class, bail_detail=bail_detail)
        else:
            patch_state(gr_id, _delete=("bail_detail",), bail_class=bail_class)
    except Exception:
        pass


def resolve_state_file(gr_id: str | None) -> pathlib.Path | None:
    """Return path to state.json for gr_id, or None when gr_id is absent."""
    if not gr_id:
        return None
    state_root = (
        pathlib.Path(
            os.environ.get("XDG_STATE_HOME")
            or os.path.join(os.path.expanduser("~"), ".local", "state")
        )
        / "claude-gremlins"
    )
    return state_root / gr_id / "state.json"


def patch_state(
    gr_id: str | None, _delete: tuple[str, ...] = (), **fields: object
) -> None:
    """Merge keyword fields into state.json atomically, deleting any keys in _delete.

    No-op when gr_id is unset, when state.json doesn't exist, or when the
    write fails — stage bookkeeping must not crash a running gremlin.
    """
    sf = resolve_state_file(gr_id)
    if sf is None or not sf.exists():
        return
    try:
        data = json.loads(sf.read_text(encoding="utf-8"))
        for key in _delete:
            data.pop(key, None)
        data.update(fields)
        tmp = sf.with_name(f"{sf.name}.{os.getpid()}.{secrets.token_hex(8)}.tmp")
        tmp.write_text(json.dumps(data), encoding="utf-8")
        os.replace(tmp, sf)
    except Exception:
        pass


def check_bail(gr_id: str | None, label: str = "stage") -> None:
    """Raise RuntimeError if a bail_class was written to state.json by the
    just-completed stage.  No-op without gr_id or when state.json is absent."""
    sf = resolve_state_file(gr_id)
    if sf is None or not sf.exists():
        return
    try:
        data = json.loads(sf.read_text(encoding="utf-8"))
        bail_class = data.get("bail_class", "")
        if bail_class:
            raise RuntimeError(
                f"{label} bailed: bail_class={bail_class} (see state.json bail_detail)"
            )
    except RuntimeError:
        raise
    except Exception:
        pass
