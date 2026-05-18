"""Recording test double for the Claude client."""

from __future__ import annotations

import json
import pathlib
from dataclasses import dataclass
from typing import Any, cast

from gremlins.clients.client import Client
from gremlins.clients.protocol import CompletedRun


@dataclass
class RecordedCall:
    prompt: str
    label: str
    model: str | None
    raw_path: pathlib.Path | None
    capture_events: bool
    cwd: pathlib.Path | None = None


class FakeClaudeClient(Client):
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
    ) -> None:
        super().__init__("fake", "fake")
        self.calls: list[RecordedCall] = []
        self._fixtures: dict[str, object] = dict(fixtures or {})
        self._total_cost_usd: float = 0.0

    @property  # type: ignore[override]
    def total_cost_usd(self) -> float:
        return self._total_cost_usd

    def reap_all(self) -> None:
        pass  # Fake never spawns; nothing to reap.

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
        del on_timeout_prompt, max_retries, idle_timeout, extra_env, bypass, audit_log
        self.calls.append(
            RecordedCall(
                prompt=prompt,
                label=label,
                model=model,
                raw_path=pathlib.Path(raw_path) if raw_path is not None else None,
                capture_events=capture_events,
                cwd=pathlib.Path(cwd) if cwd is not None else None,
            )
        )

        if label not in self._fixtures:
            raise KeyError(f"FakeClaudeClient: no fixture for label {label!r}")
        events = self._load_events(self._fixtures[label])

        if raw_path is not None:
            raw_path = pathlib.Path(raw_path)
            raw_path.parent.mkdir(parents=True, exist_ok=True)
            with raw_path.open("ab") as f:
                for evt in events:
                    f.write((json.dumps(evt) + "\n").encode("utf-8"))

        cost_usd: float | None = None
        result_text: str | None = None
        for evt in events:
            if evt.get("type") == "result":
                raw_cost = evt.get("total_cost_usd", evt.get("cost_usd"))
                if isinstance(raw_cost, (int, float)):
                    cost_usd = float(raw_cost)
                    self._total_cost_usd += cost_usd
                raw_result = evt.get("result")
                if isinstance(raw_result, str):
                    result_text = raw_result

        return CompletedRun(
            exit_code=0,
            text_result=result_text,
            events=events if capture_events else None,
            cost_usd=cost_usd,
        )
