"""Recording test double for the Client."""

from __future__ import annotations

import contextvars
import json
import pathlib
import types
from typing import Any, cast

from _gremlins_core.clients import PyCompletedRun as CompletedRun
from _gremlins_core.clients import PyUsageStats as UsageStats


class FakeClient:
    """Test double — never spawns subprocesses.

    Construct with ``fixtures={label: <path-to-jsonl-or-list-of-events>}``;
    ``run(label=…)`` looks up the canned events and returns a CompletedRun
    derived from them. ``raw_path`` is written so any post-stage code that
    reads that file (e.g. the implement stage's empty-output check) sees a
    realistic on-disk shape.
    """

    def __init__(
        self,
        *,
        fixtures: dict[str, object] | None = None,
        model: str = "fake",
    ) -> None:
        self.provider = "fake"
        self.model = model
        self.extra_params: dict[str, str] = {}
        self.calls: list[Any] = []
        self._fixtures: dict[str, object] = dict(fixtures or {})
        self._total_cost_usd: float = 0.0
        self._ctx: contextvars.ContextVar[dict[str, Any] | None] = (
            contextvars.ContextVar("fake_ctx", default=None)
        )

    def __str__(self) -> str:
        return f"{self.provider}:{self.model}"

    def __repr__(self) -> str:
        return f"FakeClient(model={self.model!r})"

    def __eq__(self, other: object) -> bool:
        if not isinstance(other, FakeClient):
            return NotImplemented
        return self.provider == other.provider and self.model == other.model

    def __hash__(self) -> int:
        return hash((self.provider, self.model))

    @property
    def total_cost_usd(self) -> float:
        return self._total_cost_usd

    def reap_all(self) -> None:
        pass

    def _load_events(self, fixture: object) -> list[dict[str, Any]]:
        if isinstance(fixture, (list, tuple)):
            return [dict(cast(dict[str, Any], e)) for e in cast(list[Any], fixture)]
        if isinstance(fixture, (str, pathlib.Path)):
            path = pathlib.Path(fixture)
            events: list[dict[str, Any]] = []
            with path.open("r", encoding="utf-8") as f:
                for line in f:
                    line = line.strip()
                    if not line:
                        continue
                    events.append(cast(dict[str, Any], json.loads(line)))
            return events
        raise TypeError(f"unsupported fixture type: {type(fixture).__name__}")

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
        artifact_dir: pathlib.Path | None = None,
        idle_timeout: float | None = None,
        extra_env: dict[str, str] | None = None,
        expected_artifact_paths: list[pathlib.Path] | None = None,
        artifact_reminder_count: int = 0,
        system_prompt: str | None = None,
    ) -> CompletedRun:
        del on_timeout_prompt, max_retries, idle_timeout, extra_env, artifact_dir
        del expected_artifact_paths, artifact_reminder_count, system_prompt
        self._ctx.set(
            {
                "prompt": prompt,
                "label": label,
                "model": model,
                "raw_path": raw_path,
                "capture_events": capture_events,
                "cwd": cwd,
            }
        )
        self.calls.append(
            types.SimpleNamespace(
                prompt=prompt,
                label=label,
                model=model,
                raw_path=pathlib.Path(raw_path) if raw_path is not None else None,
                capture_events=capture_events,
                cwd=pathlib.Path(cwd) if cwd is not None else None,
            )
        )

        if label not in self._fixtures:
            raise KeyError(f"FakeClient: no fixture for label {label!r}")
        events = self._load_events(self._fixtures[label])

        if raw_path is not None:
            raw_path = pathlib.Path(raw_path)
            raw_path.parent.mkdir(parents=True, exist_ok=True)
            with raw_path.open("ab") as f:
                for evt in events:
                    f.write((json.dumps(evt) + "\n").encode("utf-8"))

        cost_usd: float | None = None
        result_text: str | None = None
        token_usage: UsageStats | None = None
        for evt in events:
            if evt.get("type") == "result":
                raw_cost = evt.get("total_cost_usd", evt.get("cost_usd"))
                if isinstance(raw_cost, (int, float)):
                    cost_usd = float(raw_cost)
                    self._total_cost_usd += cost_usd
                raw_result = evt.get("result")
                if isinstance(raw_result, str):
                    result_text = raw_result
            raw_usage = evt.get("token_usage")
            if isinstance(raw_usage, dict):
                token_usage = UsageStats(
                    prompt_tokens=raw_usage.get("prompt_tokens", 0),
                    completion_tokens=raw_usage.get("completion_tokens", 0),
                    cached_input_tokens=raw_usage.get("cached_input_tokens", 0),
                    cache_creation_input_tokens=raw_usage.get(
                        "cache_creation_input_tokens", 0
                    ),
                    reasoning_tokens=raw_usage.get("reasoning_tokens", 0),
                    turns=raw_usage.get("turns", 0),
                )

        return CompletedRun(
            exit_code=0,
            text_result=result_text,
            cost_usd=cost_usd,
            token_usage=token_usage,
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
        )
