"""Handoff stage: runs the handoff agent once per boss loop iteration."""

from __future__ import annotations

import argparse
import json
import logging
import os
import pathlib
import shutil
import subprocess
from typing import Any

from gremlins import handoff as handoff_mod
from gremlins.clients import ClientSpec
from gremlins.stages import Stage, RunCmdFailed, register_stage
from gremlins.state import emit_bail, read_state_str, resolve_state_file, set_stage

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

        boss_spec = session_dir / "boss-spec.md"
        plan_md = session_dir / "plan.md"

        if not plan_md.is_file():
            raise RuntimeError(f"handoff stage: plan file not found: {plan_md}")

        if not boss_spec.exists():
            shutil.copyfile(plan_md, boss_spec)

        sf = resolve_state_file(gr_id)
        base_ref = read_state_str(sf, "base_ref_name") or self._resolve_base_ref()
        handoff_n = self._next_handoff_index(session_dir)

        prev_rolling = (
            session_dir / f"handoff-{handoff_n - 1:03d}.md" if handoff_n > 1 else None
        )
        current_plan = (
            str(prev_rolling)
            if prev_rolling and prev_rolling.exists()
            else str(plan_md)
        )

        set_stage(gr_id, "handoff")
        exit_state, sig = self._run_handoff(
            handoff_n=handoff_n,
            current_plan=current_plan,
            original_plan=str(boss_spec),
            base_ref=base_ref,
            session_dir=session_dir,
            client=client,
        )

        if exit_state == "chain-done":
            logger.info("chain complete after %d handoff(s)", handoff_n)
            shutil.copyfile(boss_spec, plan_md)
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
        shutil.copyfile(child_plan_path, plan_md)
        raise RunCmdFailed(f"next-plan: handoff {handoff_n}")

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

        forward_spec = (
            pathlib.Path(original_plan).read_bytes()
            != pathlib.Path(current_plan).read_bytes()
        )
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

    @staticmethod
    def _next_handoff_index(session_dir: pathlib.Path) -> int:
        indices: list[int] = []
        for p in session_dir.glob("handoff-*.state.json"):
            try:
                indices.append(int(p.stem.split(".")[0].split("-")[1]))
            except (IndexError, ValueError):
                pass
        return 1 + max(indices, default=0)


register_stage("handoff", Handoff)
