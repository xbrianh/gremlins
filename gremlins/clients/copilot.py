from __future__ import annotations

import os
import pathlib
import re
import subprocess
import threading
import time

from gremlins.clients.protocol import CompletedRun

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

    def _untrack(self, p: subprocess.Popen[bytes]) -> None:
        with self._lock:
            try:
                self._children.remove(p)
            except ValueError:
                pass

    def reap_all(self) -> None:
        with self._lock:
            procs = list(self._children)
        for p in procs:
            try:
                p.terminate()
            except Exception:
                pass
        deadline = time.time() + 2.0
        for p in procs:
            remaining = max(0.0, deadline - time.time())
            try:
                p.wait(timeout=remaining)
            except Exception:
                pass
        for p in procs:
            if p.poll() is None:
                try:
                    p.kill()
                except Exception:
                    pass

    @property
    def total_cost_usd(self) -> float:
        return 0.0

    def _build_argv(self, model: str | None, prompt: str) -> list[str]:
        cmd = ["copilot", "--allow-all-tools"]
        if model is not None:
            cmd += ["--model", model]
        cmd += ["-p", prompt]
        return cmd

    def _spawn(
        self, argv: list[str], cwd: pathlib.Path | None = None
    ) -> subprocess.Popen[bytes]:
        p = subprocess.Popen(
            argv,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=False,
            env=os.environ.copy(),
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
        backoff: list[float] | None = None,
        cwd: pathlib.Path | None = None,
        idle_timeout: float | None = None,
    ) -> CompletedRun:
        del capture_events, on_timeout_prompt, idle_timeout, backoff
        argv = self._build_argv(model, prompt)
        p = self._spawn(argv, cwd=cwd)
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
