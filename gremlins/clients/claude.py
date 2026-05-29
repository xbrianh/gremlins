from __future__ import annotations

import asyncio
import contextvars
import os
import pathlib
import signal
import sys
import threading
from typing import Any

from gremlins.clients.config import (
    STREAM_IDLE_BACKOFF,
    STREAM_IDLE_TIMEOUT,
    retry,
    validate_max_retries,
)
from gremlins.clients.protocol import CompletedRun
from gremlins.clients.stream import decode_line, emit_event, extract_state, ts
from gremlins.utils.decorators import swallow
from gremlins.utils.proc import iter_lines, terminate_with_grace


class StreamTimeoutError(RuntimeError):
    def __init__(self, msg: str, *, session_id: str | None = None) -> None:
        super().__init__(msg)
        self.session_id = session_id


class ApiServerError(RuntimeError):
    def __init__(self, msg: str, *, session_id: str | None = None) -> None:
        super().__init__(msg)
        self.session_id = session_id


class SubprocessClaudeClient:
    """Production ClaudeClient: spawns ``claude -p`` subprocesses.

    Owns the live-children list so ``reap_all()`` (called from the executor's
    SIGINT/SIGTERM handlers) can terminate every concurrently-running
    ``claude -p`` before the orchestrator exits.

    Reads the operator's ambient ``~/.claude/`` config: settings, MCP servers,
    and credentials follow whatever the user has configured for interactive
    use. There is no per-gremlin config isolation on this backend — for that,
    use the ``anthropic:`` SDK provider.
    """

    def __init__(
        self,
        bypass: bool = False,
        native_block: dict[str, Any] | None = None,
    ) -> None:
        # Reentrant lock: signal handlers run on the main thread and may land
        # while _track/_untrack already hold it. A plain Lock would deadlock
        # in that narrow window.
        self._lock = threading.RLock()
        self._children: list[asyncio.subprocess.Process] = []
        self._total_cost_usd: float = 0.0
        self._bypass = bypass
        # accepted for factory shape parity with SubprocessCopilotClient; inert
        # on this backend — the claude CLI reads ~/.claude/settings.json directly.
        self._native_block: dict[str, Any] = (
            native_block if native_block is not None else {}
        )
        # Per-task storage so parallel stages sharing one client instance
        # (gremlins/stages/composite.py shares parent.client across child
        # states) don't race on resume context. ContextVars are copied per
        # asyncio.Task at creation, so each fan-out task has its own slot.
        self._ctx: contextvars.ContextVar[dict[str, Any] | None] = (
            contextvars.ContextVar("claude_ctx", default=None)
        )
        self._last_session_id: contextvars.ContextVar[str | None] = (
            contextvars.ContextVar("claude_last_session_id", default=None)
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
        return self._total_cost_usd

    def _build_argv(
        self, model: str | None, session_id: str | None = None
    ) -> list[str]:
        cmd = ["claude", "-p"]
        if model is not None:
            cmd += ["--model", model]
        if session_id is not None:
            cmd += ["--resume", session_id]
        mode = "bypassPermissions" if self._bypass else "default"
        cmd += ["--permission-mode", mode, "--verbose"]
        cmd += ["--output-format", "stream-json"]
        return cmd

    async def _spawn(
        self,
        argv: list[str],
        prompt: str,
        cwd: pathlib.Path | None = None,
        extra_env: dict[str, str] | None = None,
    ) -> asyncio.subprocess.Process:
        env = os.environ.copy()
        env["GREMLIN_SKIP_SUMMARY"] = "1"
        if extra_env:
            env.update(extra_env)
        p = await asyncio.create_subprocess_exec(
            *argv,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=None,
            env=env,
            cwd=str(cwd) if cwd is not None else None,
            start_new_session=True,
        )
        self._track(p)
        try:
            assert p.stdin is not None
            p.stdin.write(prompt.encode())
            await p.stdin.drain()
            p.stdin.close()
        except Exception:
            self._untrack(p)
            raise
        return p

    async def _read_lines(
        self,
        p: asyncio.subprocess.Process,
        prefix: str,
        raw_path: pathlib.Path | None,
        capture_events: bool,
        idle_timeout: float,
    ) -> tuple[dict[str, Any], list[dict[str, Any]] | None, bool, str | None]:
        assert p.stdout is not None
        state: dict[str, Any] = {"cost_usd": None, "result_text": None}
        events: list[dict[str, Any]] | None = [] if capture_events else None
        timed_out = False
        session_id: str | None = None
        raw = open(raw_path, "ab") if raw_path is not None else None
        try:
            try:
                async for line in iter_lines(p.stdout, idle_timeout=idle_timeout):
                    if raw is not None:
                        raw.write(line)
                        raw.flush()
                    evt = decode_line(line)
                    if b"Stream idle timeout" in line and evt is None:
                        timed_out = True
                    if evt is None:
                        continue
                    if evt.get("type") == "system" and evt.get("subtype") == "init":
                        session_id = evt.get("session_id")
                    extract_state(evt, state)
                    if events is not None:
                        events.append(evt)
                    try:
                        emit_event(prefix, evt)
                    except Exception:
                        pass
            except TimeoutError:
                timed_out = True
        finally:
            if raw is not None:
                raw.close()
        return state, events, timed_out, session_id

    async def _consume(
        self,
        p: asyncio.subprocess.Process,
        prefix: str,
        raw_path: pathlib.Path | None,
        capture_events: bool,
        idle_timeout: float,
    ) -> CompletedRun:
        try:
            state, events, timed_out, session_id = await self._read_lines(
                p, prefix, raw_path, capture_events, idle_timeout
            )
        finally:
            self._untrack(p)
        if session_id is not None:
            self._last_session_id.set(session_id)
        if timed_out:
            await terminate_with_grace(p, grace_s=5.0)
            raise StreamTimeoutError(
                "claude -p stream idle timeout", session_id=session_id
            )
        rc = await p.wait()
        cost_usd = state["cost_usd"]
        if cost_usd is not None:
            with self._lock:
                self._total_cost_usd += cost_usd
        status = state.get("api_error_status")
        if state.get("is_error") and isinstance(status, int) and 500 <= status <= 599:
            raise ApiServerError(
                f"claude -p api server error {status}", session_id=session_id
            )
        return CompletedRun(
            exit_code=rc,
            text_result=state["result_text"],
            events=events,
            cost_usd=cost_usd,
        )

    async def _attempt(self, prompt: str, session_id: str | None) -> CompletedRun:
        ctx = self._ctx.get()
        if ctx is None:
            raise RuntimeError("_attempt() called before run()")
        argv = self._build_argv(ctx["model"], session_id=session_id)
        p = await self._spawn(argv, prompt, cwd=ctx["cwd"], extra_env=ctx["extra_env"])
        return await self._consume(
            p,
            ctx["prefix"],
            ctx["raw_path"],
            ctx["capture_events"],
            ctx["idle_timeout"],
        )

    def _continue_prompt(self) -> str:
        ctx = self._ctx.get()
        if ctx is None:
            raise RuntimeError("_continue_prompt() called before run()")
        return (
            ctx["on_timeout_prompt"]
            if ctx["on_timeout_prompt"] is not None
            else ctx["prompt"]
        )

    async def resume(self) -> CompletedRun:
        ctx = self._ctx.get()
        if ctx is None:
            raise RuntimeError("resume() called before run()")
        # max_retries is the caller-visible budget (initial attempt + N retries =
        # N+1 total tries). When resume() is invoked from run() after the
        # initial attempt timed out, one attempt has already been spent, so the
        # remaining budget is max_retries - 1. The max(0, …) guards against
        # max_retries=0 (no retries at all).
        backoff = STREAM_IDLE_BACKOFF[: max(0, ctx["max_retries"] - 1)]

        def _on_retry(attempt: int, exc: BaseException, wait: float) -> None:
            cause = (
                "stream idle timeout"
                if isinstance(exc, StreamTimeoutError)
                else "api server error"
            )
            sys.stderr.write(
                f"{ts()} {ctx['prefix']}{cause}, resuming in {wait}s"
                f" ({attempt + 1}/{ctx['max_retries']})...\n"
            )

        @retry(StreamTimeoutError, ApiServerError, backoff=backoff, on_retry=_on_retry)
        async def _attempt_resume() -> CompletedRun:
            return await self._attempt(
                self._continue_prompt(), session_id=self._last_session_id.get()
            )

        return await _attempt_resume()

    async def run(
        self,
        prompt: str,
        *,
        label: str,
        model: str | None = None,
        raw_path: pathlib.Path | None = None,
        capture_events: bool = False,
        on_timeout_prompt: str | None = None,
        max_retries: int = 3,
        cwd: pathlib.Path | None = None,
        idle_timeout: float | None = None,
        extra_env: dict[str, str] | None = None,
    ) -> CompletedRun:
        """Spawn ``claude -p`` and stream its output.

        ``max_retries`` is the caller-visible budget: total attempts on idle
        timeout = ``max_retries + 1`` (one initial run, then up to
        ``max_retries`` ``--resume <session-id>`` retries via ``resume()``).
        """
        validate_max_retries(max_retries)
        if idle_timeout is None:
            idle_timeout = STREAM_IDLE_TIMEOUT
        prefix = f"[{label}] " if label else ""
        self._ctx.set(
            {
                "prompt": prompt,
                "on_timeout_prompt": on_timeout_prompt,
                "label": label,
                "model": model,
                "raw_path": raw_path,
                "capture_events": capture_events,
                "idle_timeout": idle_timeout,
                "cwd": cwd,
                "extra_env": extra_env,
                "prefix": prefix,
                "max_retries": max_retries,
            }
        )
        self._last_session_id.set(None)

        try:
            result = await self._attempt(prompt, session_id=None)
        except (StreamTimeoutError, ApiServerError):
            result = await self.resume()

        if result.exit_code != 0:
            raise RuntimeError(
                f"claude -p (model={model}, label={label}) exited {result.exit_code}"
            )
        return result
