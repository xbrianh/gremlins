from __future__ import annotations

import contextlib
import os
import pathlib
import subprocess
import sys
import threading
from collections.abc import Generator
from typing import IO, cast

from gremlins.clients.config import (
    STREAM_IDLE_BACKOFF,
    STREAM_IDLE_TIMEOUT,
    retry,
    validate_max_retries,
)
from gremlins.clients.protocol import CompletedRun
from gremlins.clients.stream import stream_events
from gremlins.clients.subprocess_utils import (
    reap_processes,
    terminate_and_kill,
)
from gremlins.utils.decorators import swallow


class StreamTimeoutError(RuntimeError):
    pass


CLAUDE_FLAGS_BASE = [
    "--permission-mode",
    "bypassPermissions",
    "--verbose",
]


class SubprocessClaudeClient:
    """Production ClaudeClient: spawns ``claude -p`` subprocesses.

    Owns the live-children list so ``reap_all()`` (called from the executor's
    SIGINT/SIGTERM handlers) can terminate every concurrently-running
    ``claude -p`` before the orchestrator exits.
    """

    def __init__(self) -> None:
        # Reentrant lock: signal handlers run on the main thread and may land
        # while _track/_untrack already hold it. A plain Lock would deadlock
        # in that narrow window.
        self._lock = threading.RLock()
        self._children: list[subprocess.Popen[bytes]] = []
        self._total_cost_usd: float = 0.0

    def _track(self, p: subprocess.Popen[bytes]) -> None:
        with self._lock:
            self._children.append(p)

    @swallow(ValueError)
    def _untrack(self, p: subprocess.Popen[bytes]) -> None:
        with self._lock:
            self._children.remove(p)

    @contextlib.contextmanager
    def _tracked(
        self, p: subprocess.Popen[bytes]
    ) -> Generator[subprocess.Popen[bytes], None, None]:
        self._track(p)
        try:
            yield p
        except Exception:
            self._untrack(p)
            raise

    def reap_all(self) -> None:
        with self._lock:
            procs = list(self._children)
        reap_processes(procs)

    @property
    def total_cost_usd(self) -> float:
        return self._total_cost_usd

    def _build_argv(self, model: str | None) -> list[str]:
        cmd = ["claude", "-p"]
        if model is not None:
            cmd += ["--model", model]
        cmd += list(CLAUDE_FLAGS_BASE)
        cmd += ["--output-format", "stream-json"]
        return cmd

    def _spawn(
        self,
        argv: list[str],
        prompt: str,
        cwd: pathlib.Path | None = None,
        extra_env: dict[str, str] | None = None,
    ) -> subprocess.Popen[bytes]:
        env = os.environ.copy()
        env["GREMLIN_SKIP_SUMMARY"] = "1"
        if extra_env:
            env.update(extra_env)
        # Default bufsize (-1) gives a BufferedReader with 8 KiB reads, so
        # readline() scans for '\n' in-buffer instead of doing one os.read()
        # per byte. Streaming latency is preserved and throughput on large
        # stream-json traces jumps by orders of magnitude.
        p = subprocess.Popen(
            argv,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=None,
            start_new_session=False,
            env=env,
            cwd=str(cwd) if cwd is not None else None,
        )
        with self._tracked(p):
            stdin = cast(IO[bytes], p.stdin)
            stdin.write(prompt.encode())
            stdin.close()
        return p

    def _consume(
        self,
        p: subprocess.Popen[bytes],
        prefix: str,
        raw_path: pathlib.Path | None,
        capture_events: bool,
        idle_timeout: float = STREAM_IDLE_TIMEOUT,
    ) -> CompletedRun:
        try:
            stdout = cast(IO[bytes], p.stdout)
            cost_usd, result_text, events, timed_out = stream_events(
                stdout,
                prefix=prefix,
                raw_path=raw_path,
                capture=capture_events,
                idle_timeout=idle_timeout,
            )
            if cost_usd is not None:
                with self._lock:
                    self._total_cost_usd += cost_usd
            if timed_out:
                terminate_and_kill(p, 5.0)
                stdout.close()
                raise StreamTimeoutError("claude -p stream idle timeout")
            stdout.close()
            rc = p.wait()
        finally:
            self._untrack(p)

        return CompletedRun(
            exit_code=rc,
            text_result=result_text,
            events=events,
            cost_usd=cost_usd,
        )

    def run(
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
                f"{prefix}stream idle timeout, retrying in {wait}s"
                f" ({attempt + 1}/{max_retries})...\n"
            )
            if on_timeout_prompt is not None:
                active_prompt = on_timeout_prompt

        @retry(
            StreamTimeoutError,
            backoff=STREAM_IDLE_BACKOFF[:max_retries],
            on_retry=_on_retry,
        )
        def _run_once() -> CompletedRun:
            p = self._spawn(argv, active_prompt, cwd=cwd, extra_env=extra_env)
            return self._consume(p, prefix, raw_path, capture_events, idle_timeout)

        result = _run_once()
        if result.exit_code != 0:
            raise RuntimeError(
                f"claude -p (model={model}, label={label}) exited {result.exit_code}"
            )
        return result
