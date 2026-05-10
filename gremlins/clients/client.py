from __future__ import annotations

import pathlib
from typing import Any

from gremlins.clients.protocol import CompletedRun
from gremlins.clients.registry import CLIENT_FACTORIES


class Client:
    def __init__(self, provider: str, model: str) -> None:
        self.provider = provider
        self.model = model
        self._impl: Any = None

    @classmethod
    def parse(cls, s: str) -> Client:
        if ":" not in s:
            raise ValueError(
                f"invalid client specifier {s!r}: expected 'provider:model'"
            )
        provider, _, model = s.partition(":")
        if not provider:
            raise ValueError(
                f"invalid client specifier {s!r}: expected 'provider:model'"
            )
        if not model:
            raise ValueError(f"invalid client specifier {s!r}: model must not be empty")
        if provider not in CLIENT_FACTORIES:
            raise ValueError(f"unknown provider {provider!r} in client specifier {s!r}")
        return cls(provider=provider, model=model)

    def __str__(self) -> str:
        return f"{self.provider}:{self.model}"

    def __repr__(self) -> str:
        return f"Client({self.provider!r}, {self.model!r})"

    def __eq__(self, other: object) -> bool:
        if not isinstance(other, Client):
            return NotImplemented
        return self.provider == other.provider and self.model == other.model

    def __hash__(self) -> int:
        return hash((self.provider, self.model))

    def _get_impl(self) -> Any:
        if self._impl is None:
            if self.provider not in CLIENT_FACTORIES:
                raise ValueError(f"unknown provider {self.provider!r}")
            self._impl = CLIENT_FACTORIES[self.provider](self.model or None)
        return self._impl

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
    ) -> CompletedRun:
        return self._get_impl().run(
            prompt,
            label=label,
            model=self.model,
            raw_path=raw_path,
            capture_events=capture_events,
            on_timeout_prompt=on_timeout_prompt,
            max_retries=max_retries,
            cwd=cwd,
            idle_timeout=idle_timeout,
        )

    def reap_all(self) -> None:
        if self._impl is not None:
            self._impl.reap_all()

    @property
    def total_cost_usd(self) -> float:
        if self._impl is None:
            return 0.0
        return self._impl.total_cost_usd


PACKAGE_DEFAULT = Client("claude", "sonnet")
