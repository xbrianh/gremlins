from __future__ import annotations

import asyncio
import contextvars
import os
import pathlib
import re
import signal
import threading
from typing import Any

from gremlins.clients.protocol import CompletedRun
from gremlins.utils.decorators import swallow
from gremlins.utils.proc import terminate_with_grace

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

    def __init__(
        self,
        bypass: bool = False,
        native_block: dict[str, Any] | None = None,
    ) -> None:
        self._lock = threading.RLock()
        self._children: list[asyncio.subprocess.Process] = []
        self._bypass = bypass
        self._native_block: dict[str, Any] = (
            native_block if native_block is not None else {}
        )
        # Per-task storage — see comment in SubprocessClaudeClient.__init__.
        self._ctx: contextvars.ContextVar[dict[str, Any] | None] = (
            contextvars.ContextVar("copilot_ctx", default=None)
        )

    def _track(self, p: asyncio.subprocess.Process) -> None:
        with self._lock:
            self._children.append(p)

    @swallow(ValueError)
    def _untrack(self, p: asyncio.subprocess.Process) -> None:
        with self._lock:
            self._children.remove(p)

    def reap_all(self) -> None:
        with self._lock:
            procs = list(self._children)
        for p in procs:
            try:
                os.killpg(p.pid, signal.SIGTERM)
            except Exception:
                pass
        # asyncio.subprocess.Process.wait() is async so we cannot give processes a
        # window to handle SIGTERM before escalating. SIGTERM above is a courtesy;
        # the SIGKILL below is what actually reclaims them.
        for p in procs:
            try:
                os.killpg(p.pid, signal.SIGKILL)
            except Exception:
                pass

    @property
    def total_cost_usd(self) -> float:
        return 0.0

    def _build_argv(self, model: str | None, prompt: str) -> list[str]:
        cmd = ["copilot"]
        if self._bypass:
            # --allow-all grants file-path and URL access that --allow-all-tools alone doesn't.
            cmd += ["--allow-all"]
        if model is not None:
            cmd += ["--model", model]
        cmd += ["-p", prompt]
        return cmd

    async def _spawn(
        self,
        argv: list[str],
        cwd: pathlib.Path | None = None,
        extra_env: dict[str, str] | None = None,
    ) -> asyncio.subprocess.Process:
        env = os.environ.copy()
        if extra_env:
            env.update(extra_env)
        p = await asyncio.create_subprocess_exec(
            *argv,
            stdin=asyncio.subprocess.DEVNULL,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
            env=env,
            cwd=str(cwd) if cwd is not None else None,
            start_new_session=True,
        )
        self._track(p)
        return p

    async def run(
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
    ) -> CompletedRun:
        del (
            idle_timeout,
            on_timeout_prompt,
            max_retries,
        )  # copilot reads stdout to EOF; no streaming idle concept
        self._ctx.set(
            {
                "prompt": prompt,
                "label": label,
                "model": model,
                "raw_path": raw_path,
                "capture_events": capture_events,
                "cwd": cwd,
                "extra_env": extra_env,
            }
        )
        argv = self._build_argv(model, prompt)
        p = await self._spawn(argv, cwd=cwd, extra_env=extra_env)
        try:
            assert p.stdout is not None
            assert p.stderr is not None
            raw_out, raw_err = await p.communicate()
            rc = p.returncode
        finally:
            self._untrack(p)
            if p.returncode is None:
                # cancellation path: communicate() was interrupted before the process exited
                await terminate_with_grace(p, grace_s=5.0)

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

    async def resume(self) -> CompletedRun:
        ctx = self._ctx.get()
        if ctx is None:
            raise RuntimeError("resume() called before run()")
        return await self.run(
            ctx["prompt"],
            label=ctx["label"],
            model=ctx["model"],
            raw_path=ctx["raw_path"],
            capture_events=ctx["capture_events"],
            cwd=ctx["cwd"],
            extra_env=ctx["extra_env"],
        )
