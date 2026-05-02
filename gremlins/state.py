"""Stage and bail bookkeeping for gremlin state.json.

Both helpers write state.json atomically in pure Python. Both are no-ops
outside a gremlin context (no ``GR_ID`` or missing state.json) and never
raise — stage/bail bookkeeping must not break a running gremlin.
"""

from __future__ import annotations

import datetime
import json
import os
import pathlib
import re
import secrets

GR_ID_RE = re.compile(r"^[A-Za-z0-9._-]+$")

# The four bail-class strings written to state.json.bail_class. Byte-stable
# across the migration — these strings appear in state.json files written by
# the old code that the new code must continue to read.
BAIL_CLASS_REVIEWER_REQUESTED_CHANGES = "reviewer_requested_changes"
BAIL_CLASS_SECURITY = "security"
BAIL_CLASS_SECRETS = "secrets"
BAIL_CLASS_OTHER = "other"


def set_stage(stage: str, sub_stage: object = None) -> None:
    """Write stage and stage_updated_at to state.json. No-op without GR_ID, empty stage, or missing state.json."""
    try:
        if not stage or not os.environ.get("GR_ID"):
            return
        sf = resolve_state_file()
        if sf is None or not sf.exists():
            return
        now = datetime.datetime.now(datetime.UTC).strftime("%Y-%m-%dT%H:%M:%SZ")
        if sub_stage is not None:
            patch_state(stage=stage, stage_updated_at=now, sub_stage=sub_stage)
        else:
            patch_state(_delete=("sub_stage",), stage=stage, stage_updated_at=now)
    except Exception:
        pass


def resolve_session_dir() -> pathlib.Path:
    """Resolve the artifacts directory for the current run.

    Under a gremlin (``GR_ID`` set and valid), nests under
    ``$STATE_ROOT/<gr_id>/artifacts/`` so the launcher's state.json and the
    pipeline artifacts share a parent. Direct invocations (no ``GR_ID``, or a
    malformed ``GR_ID`` treated as absent rather than raising) nest under
    ``$STATE_ROOT/direct/<ts>-<rand>/artifacts/`` so they're visually separated
    from real gremlins and can be pruned on a simpler age-based heuristic.
    """
    state_root = (
        pathlib.Path(
            os.environ.get("XDG_STATE_HOME")
            or os.path.join(os.path.expanduser("~"), ".local", "state")
        )
        / "claude-gremlins"
    )
    gr_id = os.environ.get("GR_ID", "")
    if gr_id and not GR_ID_RE.match(gr_id):
        # Malformed GR_ID — treat as a direct invocation rather than raising a
        # raw Python traceback for malformed environment input.
        gr_id = ""
    if gr_id:
        session_dir = state_root / gr_id / "artifacts"
    else:
        ts = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
        rand = secrets.token_hex(3)  # 6 hex chars
        session_dir = state_root / "direct" / f"{ts}-{rand}" / "artifacts"
    session_dir.mkdir(parents=True, exist_ok=True)
    return session_dir


def emit_bail(bail_class: str, bail_detail: str = "") -> None:
    """Write bail_class (and optional bail_detail) to state.json. No-op without GR_ID, empty bail_class, or missing state.json."""
    try:
        if not os.environ.get("GR_ID") or not bail_class:
            return
        sf = resolve_state_file()
        if sf is None or not sf.exists():
            return
        if bail_detail:
            patch_state(bail_class=bail_class, bail_detail=bail_detail)
        else:
            patch_state(_delete=("bail_detail",), bail_class=bail_class)
    except Exception:
        pass


def resolve_state_file() -> pathlib.Path | None:
    """Return path to state.json for the current GR_ID, or None when no GR_ID is set."""
    gr_id = os.environ.get("GR_ID", "")
    if not gr_id or not GR_ID_RE.match(gr_id):
        return None
    state_root = (
        pathlib.Path(
            os.environ.get("XDG_STATE_HOME")
            or os.path.join(os.path.expanduser("~"), ".local", "state")
        )
        / "claude-gremlins"
    )
    return state_root / gr_id / "state.json"


def patch_state(_delete: tuple[str, ...] = (), **fields: object) -> None:
    """Merge keyword fields into state.json atomically, deleting any keys in _delete.

    No-op when GR_ID is unset, when state.json doesn't exist, or when the
    write fails — stage bookkeeping must not crash a running gremlin.
    """
    sf = resolve_state_file()
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


def check_bail(label: str = "stage") -> None:
    """Raise RuntimeError if a bail_class was written to state.json by the
    just-completed stage.  No-op without GR_ID or when state.json is absent."""
    sf = resolve_state_file()
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
