"""Chain stage: loops a named child pipeline in-process with handoff between iterations."""

from __future__ import annotations

import argparse
import json
import logging
import os
import pathlib
import shutil
import subprocess
from collections.abc import Callable
from typing import TYPE_CHECKING, Any, cast

from gremlins import handoff as handoff_mod
from gremlins.clients import PACKAGE_DEFAULT
from gremlins.runner import run_stages
from gremlins.stages.base import Stage
from gremlins.stages.registry import register_stage
from gremlins.state import emit_bail, patch_state, set_stage

if TYPE_CHECKING:
    from gremlins.pipeline import StageEntry

logger = logging.getLogger(__name__)

HANDOFF_TIMEOUT = int(os.environ.get("BOSSGREMLIN_HANDOFF_TIMEOUT", "3600"))


class Chain(Stage):
    def __init__(
        self,
        entry: StageEntry,
        model: str | None,
        *,
        pipeline_builder: Callable[
            [str, pathlib.Path, pathlib.Path, str | None],
            list[tuple[str, Callable[[], None]]],
        ],
    ) -> None:
        super().__init__(entry, model)
        self.child_pipeline_name: str = str(entry.options.get("child", "local"))
        self._pipeline_builder = pipeline_builder

    def run(self, pipe: Any) -> Any:  # noqa: ARG002
        gr_id = self.state.gr_id
        session_dir = self.state.session_dir
        client = self.state.client

        chain_st = self._load_chain_state()

        if chain_st is None:
            spec_path = str(session_dir / "plan.md")
            if not os.path.isfile(spec_path):
                raise RuntimeError(f"chain stage: plan file not found: {spec_path}")

            base_ref = self._resolve_base_ref()
            chain_st = cast(
                dict[str, Any],
                {
                    "original_plan": spec_path,
                    "base_ref": base_ref,
                    "handoff_count": 0,
                    "handoff_records": [],
                    "current_plan": spec_path,
                    "current_child_n": 0,
                    "current_child_stage": None,
                },
            )
            self._save_chain_state(chain_st)
            patch_state(gr_id, original_plan=spec_path)

        current_plan: str = str(chain_st.get("current_plan", ""))
        base_ref: str = str(chain_st.get("base_ref", "HEAD"))
        handoff_count: int = int(chain_st.get("handoff_count", 0))
        current_child_n: int = int(chain_st.get("current_child_n", 0))
        _rcs = chain_st.get("current_child_stage")
        resume_child_stage: str | None = str(_rcs) if _rcs is not None else None

        # Resume path: a child was in flight when we last stopped
        if resume_child_stage is not None:
            child_dir = session_dir / f"child-{current_child_n:03d}"
            child_plan_file = child_dir / "plan.md"
            if not child_plan_file.exists():
                raise RuntimeError(
                    f"chain resume: child plan file not found: {child_plan_file}"
                )
            self._run_child(
                child_dir=child_dir,
                child_n=current_child_n,
                chain_st=chain_st,
                resume_from=resume_child_stage,
            )
            chain_st["current_child_stage"] = None
            self._save_chain_state(chain_st)

        while True:
            handoff_count += 1
            chain_st["handoff_count"] = handoff_count
            self._save_chain_state(chain_st)

            set_stage(gr_id, "chain")
            exit_state, sig = self._run_handoff(
                handoff_n=handoff_count,
                current_plan=current_plan,
                base_ref=base_ref,
                session_dir=session_dir,
                client=client,
            )

            if os.path.isfile(sig.get("out_path", "") or ""):
                chain_st["current_plan"] = sig["out_path"]
                current_plan = sig["out_path"]

            record = {
                "n": handoff_count,
                "plan_in": chain_st["current_plan"],
                "exit_state": exit_state,
                "signal_file": sig.get("signal_path", ""),
            }
            chain_st["handoff_records"].append(record)
            self._save_chain_state(chain_st)

            if exit_state == "chain-done":
                logger.info("chain complete after %d handoff(s)", handoff_count)
                chain_st["current_child_stage"] = None
                self._save_chain_state(chain_st)
                break

            if exit_state == "bail":
                reason = sig.get("reason") or "(no reason given)"
                logger.info("handoff bailed: %s", reason)
                emit_bail(gr_id, "other", f"handoff bail: {reason}"[:200])
                raise RuntimeError(f"chain halted by handoff: {reason}")

            # exit_state == "next-plan": run child pipeline
            child_plan_path = sig.get("child_plan") or ""
            if not child_plan_path or not os.path.isfile(child_plan_path):
                raise RuntimeError(
                    f"handoff returned next-plan but child_plan not found: {child_plan_path!r}"
                )

            current_child_n += 1
            child_dir = session_dir / f"child-{current_child_n:03d}"
            child_dir.mkdir(parents=True, exist_ok=True)

            child_plan_file = child_dir / "plan.md"
            if not child_plan_file.exists():
                shutil.copyfile(child_plan_path, child_plan_file)

            chain_st["current_child_n"] = current_child_n
            chain_st["current_child_stage"] = None
            self._save_chain_state(chain_st)

            self._run_child(
                child_dir=child_dir,
                child_n=current_child_n,
                chain_st=chain_st,
                resume_from=None,
            )

            chain_st["current_child_stage"] = None
            self._save_chain_state(chain_st)

    def _run_child(
        self,
        *,
        child_dir: pathlib.Path,
        child_n: int,
        chain_st: dict[str, Any],
        resume_from: str | None,
    ) -> None:
        gr_id = self.state.gr_id

        logger.info(
            "running child pipeline %d (pipeline: %s, resume_from: %s)",
            child_n,
            self.child_pipeline_name,
            resume_from or "start",
        )

        child_plan_file = child_dir / "plan.md"
        child_stages = self._pipeline_builder(
            self.child_pipeline_name,
            child_plan_file,
            child_dir,
            resume_from,
        )

        def _wrap(stage_name: str, fn: Callable[[], None]) -> Callable[[], None]:
            def _wrapped() -> None:
                chain_st["current_child_stage"] = stage_name
                patch_state(gr_id, chain_current_child_stage=stage_name)
                fn()

            return _wrapped

        wrapped_stages = [(_name, _wrap(_name, _fn)) for _name, _fn in child_stages]

        try:
            run_stages(wrapped_stages, resume_from=resume_from)
        except (SystemExit, Exception) as exc:
            self._propagate_child_bail(child_n, exc)
            raise

    def _propagate_child_bail(self, child_n: int, exc: BaseException) -> None:
        gr_id = self.state.gr_id
        sf = _resolve_state_file(gr_id)
        bail_class = ""
        bail_detail = ""
        if sf is not None and sf.exists():
            try:
                data = json.loads(sf.read_text(encoding="utf-8"))
                bail_class = data.get("bail_class", "")
                bail_detail = data.get("bail_detail", "")
            except Exception:
                pass

        if not bail_class:
            bail_class = "other"
            bail_detail = str(exc)[:200]

        patch_state(
            gr_id,
            bail_source="child",
            child_bail_class=bail_class,
            child_bail_detail=bail_detail,
        )
        logger.info(
            "child %d bailed: %s — bail propagated to boss", child_n, bail_class
        )

    def _run_handoff(
        self,
        *,
        handoff_n: int,
        current_plan: str,
        base_ref: str,
        session_dir: pathlib.Path,
        client: Any,
    ) -> tuple[str, dict[str, Any]]:
        out_path = session_dir / f"handoff-{handoff_n:03d}.md"
        signal_path = session_dir / f"handoff-{handoff_n:03d}.state.json"

        original_plan = str(session_dir / "plan.md")
        forward_spec = original_plan != current_plan
        model_str = self.model or str(PACKAGE_DEFAULT)

        logger.info(
            "handoff %d: plan=%s, spec=%s, base=%s",
            handoff_n,
            current_plan,
            original_plan if forward_spec else "(none)",
            base_ref[:12] if len(base_ref) >= 12 else base_ref,
        )

        args = argparse.Namespace(
            plan=current_plan,
            spec=original_plan if forward_spec else None,
            out=str(out_path),
            base=base_ref,
            client=model_str,
            timeout=HANDOFF_TIMEOUT,
            rev=None,
        )

        rc = handoff_mod.run(client, args)
        if rc != 0:
            raise RuntimeError(f"handoff agent exited {rc}")

        if not signal_path.exists():
            raise RuntimeError(f"handoff signal file not written: {signal_path}")

        try:
            sig_data: dict[str, Any] = json.loads(
                signal_path.read_text(encoding="utf-8")
            )
        except Exception as exc:
            raise RuntimeError(
                f"could not parse handoff signal file {signal_path}: {exc}"
            ) from exc

        exit_state = sig_data.get("exit_state", "")
        if exit_state not in ("next-plan", "chain-done", "bail"):
            raise RuntimeError(
                f"handoff signal file has unrecognized exit_state: {exit_state!r}"
            )

        sig_data["out_path"] = str(out_path)
        sig_data["signal_path"] = str(signal_path)
        logger.info("handoff %d result: %s", handoff_n, exit_state)
        return exit_state, sig_data

    def _resolve_base_ref(self) -> str:
        r = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            capture_output=True,
            text=True,
            cwd=str(self.state.cwd),
            check=False,
        )
        if r.returncode == 0:
            return r.stdout.strip()
        return "HEAD"

    def _load_chain_state(self) -> dict[str, Any] | None:
        sf = _resolve_state_file(self.state.gr_id)
        if sf is None or not sf.exists():
            return None
        try:
            data: dict[str, Any] = json.loads(sf.read_text(encoding="utf-8"))
            st = data.get("chain_state")
            return dict(st) if st else None
        except Exception:
            return None

    def _save_chain_state(self, chain_st: dict[str, Any]) -> None:
        patch_state(self.state.gr_id, chain_state=chain_st)


def _resolve_state_file(gr_id: str | None) -> pathlib.Path | None:
    from gremlins.state import resolve_state_file

    return resolve_state_file(gr_id)


register_stage("chain", Chain)
