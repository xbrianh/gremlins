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


def _locked_update(sf: pathlib.Path, fn: object) -> None:
    """Acquire an exclusive lock on sf.lock, read sf, apply fn(data), write sf atomically.

    Callers are responsible for the outer try/except — this may raise.
    """
    lock_path = sf.with_name(sf.name + ".lock")
    with open(lock_path, "a") as lock_f:
        fcntl.flock(lock_f, fcntl.LOCK_EX)
        data = json.loads(sf.read_text(encoding="utf-8"))
        fn(data)  # type: ignore[operator]
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

            def _merge(data: dict) -> None:  # type: ignore[type-arg]
                shards = dict(data.get("parallel_bails") or {})
                shards[child_key] = shard
                data["parallel_bails"] = shards

            _locked_update(sf, _merge)
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
    """Merge keyword fields into state.json atomically under an exclusive file lock.

    No-op when gr_id is unset, when state.json doesn't exist, or when the
    write fails — stage bookkeeping must not crash a running gremlin.
    """
    sf = resolve_state_file(gr_id)
    if sf is None or not sf.exists():
        return
    try:
        def _apply(data: dict) -> None:  # type: ignore[type-arg]
            for key in _delete:
                data.pop(key, None)
            data.update(fields)

        _locked_update(sf, _apply)
    except Exception:
        pass


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
        data = json.loads(sf.read_text(encoding="utf-8"))
        if child_key is None:
            bail_class = data.get("bail_class", "")
        else:
            bail_class = (
                (data.get("parallel_bails") or {})
                .get(child_key, {})
                .get("bail_class", "")
            )
        if bail_class:
            raise RuntimeError(
                f"{label} bailed: bail_class={bail_class} (see state.json bail_detail)"
            )
    except RuntimeError:
        raise
    except Exception:
        pass
