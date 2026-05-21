from __future__ import annotations

import os
import pathlib

from gremlins.clients.protocol import CompletedRun

_DEFAULT_MODEL = "claude-sonnet-4-6"


class AnthropicSdkClient:
    def __init__(self, model: str | None) -> None:
        api_key = os.environ.get("ANTHROPIC_API_KEY")
        if not api_key:
            raise RuntimeError("ANTHROPIC_API_KEY environment variable is not set")
        self._model = model or _DEFAULT_MODEL
        self._api_key = api_key

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
        raise NotImplementedError

    def reap_all(self) -> None:
        pass

    @property
    def total_cost_usd(self) -> float | None:
        return None


def make_anthropic_client(model: str | None) -> AnthropicSdkClient:
    return AnthropicSdkClient(model)
