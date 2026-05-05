from __future__ import annotations

import os
import pathlib
import subprocess
import sys
import threading
import time

from .protocol import CompletedRun
from .stream import stream_events


class StreamTimeoutError(RuntimeError):
    pass


CLAUDE_FLAGS_BASE = [
    "--permission-mode",
    "bypassPermissions",
    "--verbose",
]


class SubprocessClaudeClient:
    """Production ClaudeClient: spawns ``claude -p`` subprocesses.

    Owns the live-children list so ``reap_all()`` (called from the SIGINT/
    SIGTERM handlers installed by ``runner.install_signal_handlers``) can
    terminate every concurrently-running ``claude -p`` before the orchestrator
    exits.
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
        return self._total_cost_usd

    def _build_argv(self, model: str | None) -> list[str]:
        cmd = ["claude", "-p"]
        if model is not None:
            cmd += ["--model", model]
        cmd += list(CLAUDE_FLAGS_BASE)
        cmd += ["--output-format", "stream-json"]
        return cmd

    def _spawn(self, argv: list[str], prompt: str) -> subprocess.Popen[bytes]:
        env = os.environ.copy()
        env["GREMLIN_SKIP_SUMMARY"] = "1"
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
        )
        self._track(p)
        try:
            assert p.stdin is not None
            p.stdin.write(prompt.encode())
            p.stdin.close()
        except Exception:
            self._untrack(p)
            raise
        return p

    def _consume(
        self,
        p: subprocess.Popen[bytes],
        prefix: str,
        raw_path: pathlib.Path | None,
        capture_events: bool,
    ) -> CompletedRun:
        try:
            assert p.stdout is not None
            session_id, cost_usd, result_text, events, timed_out = stream_events(
                p.stdout,
                prefix=prefix,
                raw_path=raw_path,
                capture=capture_events,
            )
            if cost_usd is not None:
                with self._lock:
                    self._total_cost_usd += cost_usd
            p.stdout.close()
            rc = p.wait()
        finally:
            self._untrack(p)

        if rc != 0 and timed_out:
            raise StreamTimeoutError("claude -p stream idle timeout")

        return CompletedRun(
            exit_code=rc,
            session_id=session_id,
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
        max_retries: int = 2,
    ) -> CompletedRun:
        if max_retries < 0:
            raise ValueError(f"max_retries must be >= 0, got {max_retries}")
        argv = self._build_argv(model)
        prefix = f"[{label}] " if label else ""
        active_prompt = prompt
        for attempt in range(max_retries + 1):
            p = self._spawn(argv, active_prompt)
            try:
                result = self._consume(p, prefix, raw_path, capture_events)
            except StreamTimeoutError:
                if attempt == max_retries:
                    raise
                sys.stderr.write(
                    f"{prefix}stream idle timeout, retrying"
                    f" ({attempt + 1}/{max_retries})...\n"
                )
                time.sleep(5)
                if on_timeout_prompt is not None:
                    active_prompt = on_timeout_prompt
                continue
            if result.exit_code != 0:
                raise RuntimeError(
                    f"claude -p (model={model}, label={label}) exited {result.exit_code}"
                )
            return result
        raise RuntimeError("unreachable")
