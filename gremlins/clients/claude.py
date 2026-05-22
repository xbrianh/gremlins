from __future__ import annotations

import asyncio
import os
import pathlib
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
    pass


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
        self._native_block: dict[str, Any] = (
            native_block if native_block is not None else {}
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
                p.terminate()
            except Exception:
                pass
        # asyncio.subprocess.Process.wait() is async so we cannot give processes a
        # window to handle SIGTERM before escalating. SIGTERM above is a courtesy;
        # the SIGKILL below is what actually reclaims them.
        for p in procs:
            try:
                p.kill()
            except Exception:
                pass

    @property
    def total_cost_usd(self) -> float:
        return self._total_cost_usd

    def _build_argv(self, model: str | None) -> list[str]:
        cmd = ["claude", "-p"]
        if model is not None:
            cmd += ["--model", model]
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
    ) -> tuple[dict[str, Any], list[dict[str, Any]] | None, bool]:
        assert p.stdout is not None
        state: dict[str, Any] = {"cost_usd": None, "result_text": None}
        events: list[dict[str, Any]] | None = [] if capture_events else None
        timed_out = False
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
        return state, events, timed_out

    async def _consume(
        self,
        p: asyncio.subprocess.Process,
        prefix: str,
        raw_path: pathlib.Path | None,
        capture_events: bool,
        idle_timeout: float,
    ) -> CompletedRun:
        try:
            state, events, timed_out = await self._read_lines(
                p, prefix, raw_path, capture_events, idle_timeout
            )
        finally:
            self._untrack(p)
        if timed_out:
            await terminate_with_grace(p, grace_s=5.0)
            raise StreamTimeoutError("claude -p stream idle timeout")
        rc = await p.wait()
        cost_usd = state["cost_usd"]
        if cost_usd is not None:
            with self._lock:
                self._total_cost_usd += cost_usd
        return CompletedRun(
            exit_code=rc,
            text_result=state["result_text"],
            events=events,
            cost_usd=cost_usd,
        )

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
        validate_max_retries(max_retries)
        if idle_timeout is None:
            idle_timeout = STREAM_IDLE_TIMEOUT
        argv = self._build_argv(model)
        prefix = f"[{label}] " if label else ""
        active_prompt = prompt

        def _on_retry(attempt: int, _exc: BaseException, wait: float) -> None:
            nonlocal active_prompt
            sys.stderr.write(
                f"{ts()} {prefix}stream idle timeout, retrying in {wait}s"
                f" ({attempt + 1}/{max_retries})...\n"
            )
            if on_timeout_prompt is not None:
                active_prompt = on_timeout_prompt

        @retry(
            StreamTimeoutError,
            backoff=STREAM_IDLE_BACKOFF[:max_retries],
            on_retry=_on_retry,
        )
        async def _run_once() -> CompletedRun:
            p = await self._spawn(argv, active_prompt, cwd=cwd, extra_env=extra_env)
            return await self._consume(
                p, prefix, raw_path, capture_events, idle_timeout
            )

        result = await _run_once()
        if result.exit_code != 0:
            raise RuntimeError(
                f"claude -p (model={model}, label={label}) exited {result.exit_code}"
            )
        return result
