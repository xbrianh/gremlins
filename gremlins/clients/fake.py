"""Recording test double for the ClaudeClient Protocol.

``FakeClaudeClient`` records every ``run(...)`` call as a ``RecordedCall``
into ``self.calls`` and replays canned events from a fixture file selected
by the call's ``label``. The lookup is one-shot per label — a missing
fixture raises rather than silently replaying the previous one. Tests that
re-enter the same stage twice within one process must use distinct labels
per phase (e.g. ``"implement"`` and ``"implement_resume"``).
"""

from __future__ import annotations

import json
import pathlib
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from typing import Any, cast

from .claude import CompletedRun


@dataclass
class RecordedCall:
    prompt: str
    label: str
    model: str | None
    raw_path: pathlib.Path | None
    output_format: str
    resume_session: str | None
    extra_flags: list[str]
    capture_events: bool


class FakeClaudeClient:
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
        text_results: dict[str, str] | None = None,
    ) -> None:
        self.calls: list[RecordedCall] = []
        self._fixtures: dict[str, object] = dict(fixtures or {})
        self._text_results: dict[str, str] = dict(text_results or {})
        self.total_cost_usd: float = 0.0

    def reap_all(self) -> None:
        # Fake never spawns; nothing to reap.
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

    def run(
        self,
        prompt: str,
        *,
        label: str,
        model: str | None = None,
        raw_path: pathlib.Path | None = None,
        output_format: str = "stream-json",
        resume_session: str | None = None,
        extra_flags: Sequence[str] = (),
        capture_events: bool = False,
        on_event: Callable[[dict[str, Any]], None] | None = None,
    ) -> CompletedRun:
        self.calls.append(
            RecordedCall(
                prompt=prompt,
                label=label,
                model=model,
                raw_path=pathlib.Path(raw_path) if raw_path is not None else None,
                output_format=output_format,
                resume_session=resume_session,
                extra_flags=list(extra_flags),
                capture_events=capture_events,
            )
        )

        if output_format != "stream-json":
            text = self._text_results.get(label)
            if text is None:
                raise KeyError(f"FakeClaudeClient: no text fixture for label {label!r}")
            return CompletedRun(
                exit_code=0, session_id=None, text_result=text, events=None
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

        session_id: str | None = None
        cost_usd: float | None = None
        result_text: str | None = None
        for evt in events:
            if (
                session_id is None
                and evt.get("type") == "system"
                and evt.get("subtype") == "init"
                and isinstance(evt.get("session_id"), str)
            ):
                session_id = evt["session_id"]
            if evt.get("type") == "result":
                raw_cost = evt.get("total_cost_usd", evt.get("cost_usd"))
                if isinstance(raw_cost, (int, float)):
                    cost_usd = float(raw_cost)
                    self.total_cost_usd += cost_usd
                raw_result = evt.get("result")
                if isinstance(raw_result, str):
                    result_text = raw_result
            if on_event is not None:
                try:
                    on_event(evt)
                except Exception:
                    pass

        return CompletedRun(
            exit_code=0,
            session_id=session_id,
            text_result=result_text,
            events=events if capture_events else None,
            cost_usd=cost_usd,
        )
