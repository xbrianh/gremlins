"""Handoff stage: runs the handoff agent once per boss loop iteration."""

from __future__ import annotations

import argparse
import json
import logging
import os
import pathlib
import shutil
import subprocess
from typing import Any, cast

from gremlins import handoff as handoff_mod
from gremlins.clients import ClientSpec
from gremlins.stages.base import Stage
from gremlins.stages.loop import RunCmdFailed
from gremlins.stages.registry import register_stage
from gremlins.state import emit_bail, patch_state, set_stage

logger = logging.getLogger(__name__)

HANDOFF_TIMEOUT = int(
    os.environ.get(
        "CHAIN_HANDOFF_TIMEOUT",
        os.environ.get("BOSSGREMLIN_HANDOFF_TIMEOUT", "3600"),
    )
)


class Handoff(Stage):
    def __init__(self, name: str, client_spec: ClientSpec) -> None:
        super().__init__(name, client_spec.model, [], {})
        self._client_spec = client_spec

    def run(self, pipe: Any) -> None:  # noqa: ARG002
        session_dir = self.state.session_dir
        gr_id = self.state.gr_id
        client = self.state.client

        chain_st = self._load_chain_state()
        if chain_st is None:
            spec_path = session_dir / "plan.md"
            if not spec_path.is_file():
                raise RuntimeError(f"handoff stage: plan file not found: {spec_path}")
            boss_spec = session_dir / "boss-spec.md"
            shutil.copyfile(spec_path, boss_spec)
            base_ref = self._resolve_base_ref()
            chain_st = cast(
                dict[str, Any],
                {
                    "original_plan": str(boss_spec),
                    "base_ref": base_ref,
                    "handoff_count": 0,
                    "handoff_records": [],
                    "current_plan": str(boss_spec),
                },
            )
            self._save_chain_state(chain_st)
            patch_state(gr_id, original_plan=str(spec_path))

        current_plan: str = str(chain_st["current_plan"])
        base_ref: str = str(chain_st["base_ref"])
        original_plan: str = str(chain_st["original_plan"])
        handoff_count: int = int(chain_st.get("handoff_count", 0))  # type: ignore[arg-type]

        handoff_count += 1
        chain_st["handoff_count"] = handoff_count
        self._save_chain_state(chain_st)

        set_stage(gr_id, "handoff")
        exit_state, sig = self._run_handoff(
            handoff_n=handoff_count,
            current_plan=current_plan,
            original_plan=original_plan,
            base_ref=base_ref,
            session_dir=session_dir,
            client=client,
        )

        pre_update_plan = current_plan
        if os.path.isfile(sig.get("out_path", "") or ""):
            chain_st["current_plan"] = sig["out_path"]

        chain_st["handoff_records"].append(
            {
                "n": handoff_count,
                "plan_in": pre_update_plan,
                "exit_state": exit_state,
                "signal_file": sig.get("signal_path", ""),
            }
        )
        self._save_chain_state(chain_st)

        if exit_state == "chain-done":
            logger.info("chain complete after %d handoff(s)", handoff_count)
            boss_spec = session_dir / "boss-spec.md"
            if boss_spec.exists():
                shutil.copyfile(boss_spec, session_dir / "plan.md")
            return

        if exit_state == "bail":
            reason = sig.get("reason") or "(no reason given)"
            logger.info("handoff bailed: %s", reason)
            emit_bail(gr_id, "other", f"handoff bail: {reason}"[:200])
            raise RuntimeError(f"chain halted by handoff: {reason}")

        # exit_state == "next-plan"
        child_plan_path = sig.get("child_plan") or ""
        if not child_plan_path or not os.path.isfile(child_plan_path):
            raise RuntimeError(
                f"handoff returned next-plan but child_plan not found: {child_plan_path!r}"
            )
        shutil.copyfile(child_plan_path, session_dir / "plan.md")
        # RunCmdFailed signals the boss loop to execute the child pipeline runners.
        raise RunCmdFailed(f"next-plan: handoff {handoff_count}")

    def _run_handoff(
        self,
        *,
        handoff_n: int,
        current_plan: str,
        original_plan: str,
        base_ref: str,
        session_dir: pathlib.Path,
        client: Any,
    ) -> tuple[str, dict[str, Any]]:
        out_path = session_dir / f"handoff-{handoff_n:03d}.md"
        signal_path = session_dir / f"handoff-{handoff_n:03d}.state.json"

        forward_spec = original_plan != current_plan
        model_str = str(self._client_spec)

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
        return r.stdout.strip() if r.returncode == 0 else "HEAD"

    def _load_chain_state(self) -> dict[str, Any] | None:
        from gremlins.state import resolve_state_file

        sf = resolve_state_file(self.state.gr_id)
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


register_stage("handoff", Handoff)
