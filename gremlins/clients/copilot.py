from __future__ import annotations

import os
import pathlib
import re
import subprocess
import threading

from gremlins.clients.protocol import CompletedRun
from gremlins.clients.subprocess_utils import reap_processes
from gremlins.utils.decorators import swallow

# Copilot appends a stats footer like "⏺ Cost: $0.01 | Duration: 3.2s | ..."
# after the response text. Strip it so text_result contains only the response.
_FOOTER_RE = re.compile(r"\n*⏺ Cost:.*$", re.DOTALL)


def _strip_footer(text: str) -> str:
    return _FOOTER_RE.sub("", text).rstrip()


class SubprocessCopilotClient:
    """Production ClaudeClient that delegates to ``copilot -p``.

    Implements the same ``ClaudeClient`` protocol as ``SubprocessClaudeClient``
    so it can be swapped in via pipeline YAML ``provider: copilot``.
    """

    def __init__(self) -> None:
        self._lock = threading.RLock()
        self._children: list[subprocess.Popen[bytes]] = []

    def _track(self, p: subprocess.Popen[bytes]) -> None:
        with self._lock:
            self._children.append(p)

    @swallow(ValueError)
    def _untrack(self, p: subprocess.Popen[bytes]) -> None:
        with self._lock:
            self._children.remove(p)

    def reap_all(self) -> None:
        with self._lock:
            procs = list(self._children)
        reap_processes(procs)

    @property
    def total_cost_usd(self) -> float:
        return 0.0

    def _build_argv(self, model: str | None, prompt: str) -> list[str]:
        # --allow-all is required: --allow-all-tools alone doesn't grant file-path
        # or URL access, so the agent falls back to chatting instead of editing files.
        cmd = ["copilot", "--allow-all"]
        if model is not None:
            cmd += ["--model", model]
        cmd += ["-p", prompt]
        return cmd

    def _spawn(
        self,
        argv: list[str],
        cwd: pathlib.Path | None = None,
        extra_env: dict[str, str] | None = None,
    ) -> subprocess.Popen[bytes]:
        env = os.environ.copy()
        if extra_env:
            env.update(extra_env)
        p = subprocess.Popen(
            argv,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=False,
            env=env,
            cwd=str(cwd) if cwd is not None else None,
        )
        self._track(p)
        return p

    def run(
        self,
        prompt: str,
        *,
        label: str,
        model: str | None = None,
        raw_path: pathlib.Path | None = None,
        capture_events: bool = False,
        on_timeout_prompt: str | None = None,
        max_retries: int = 2,
        cwd: pathlib.Path | None = None,
        idle_timeout: float | None = None,
        extra_env: dict[str, str] | None = None,
        bypass: bool = True,
        audit_log: pathlib.Path | None = None,
    ) -> CompletedRun:
        del (
            idle_timeout,
            bypass,
            audit_log,
        )  # copilot reads stdout to EOF; no streaming idle concept; bypass/audit_log for tools only
        argv = self._build_argv(model, prompt)
        p = self._spawn(argv, cwd=cwd, extra_env=extra_env)
        try:
            raw_out, raw_err = p.communicate()
            rc = p.returncode
        finally:
            self._untrack(p)

        stdout = raw_out.decode(errors="replace")
        stderr = raw_err.decode(errors="replace")

        if raw_path is not None:
            raw_path.parent.mkdir(parents=True, exist_ok=True)
            with raw_path.open("w", encoding="utf-8") as f:
                f.write(stdout)
                if stderr:
                    f.write("\n--- stderr ---\n")
                    f.write(stderr)

        if rc != 0:
            detail = f"\nstderr: {stderr[:500]}" if stderr else ""
            raise RuntimeError(
                f"copilot -p (model={model}, label={label}) exited {rc}{detail}"
            )
        return CompletedRun(
            exit_code=rc,
            text_result=_strip_footer(stdout),
            events=None,
            cost_usd=None,
        )
