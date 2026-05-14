from __future__ import annotations

import os
import pathlib

from agents import Agent, RunConfig, Runner, Usage
from agents.models.openai_provider import OpenAIProvider

from gremlins.clients.protocol import CompletedRun
from gremlins.clients.tools import GREMLINS_TOOLS

# USD per 1M tokens: (input, output)
_PRICING: dict[str, tuple[float, float]] = {
    "gpt-4o": (2.50, 10.00),
    "gpt-4o-mini": (0.15, 0.60),
    "gpt-4o-2024-11-20": (2.50, 10.00),
    "gpt-4o-2024-08-06": (2.50, 10.00),
    "gpt-4.1": (2.00, 8.00),
    "gpt-4.1-mini": (0.40, 1.60),
    "gpt-4.1-nano": (0.10, 0.40),
    "gpt-4-turbo": (10.00, 30.00),
    # o1/o3: Usage.output_tokens from openai-agents SDK bundles reasoning tokens in;
    # these prices cover the blended output rate (reasoning tokens billed at same rate).
    "o1": (15.00, 60.00),
    "o1-mini": (1.10, 4.40),
    "o3": (10.00, 40.00),
    "o3-mini": (1.10, 4.40),
    "o4-mini": (1.10, 4.40),
    # xAI Grok models
    "grok-3": (3.00, 15.00),
    "grok-3-fast": (0.60, 4.00),
    "grok-3-mini": (0.30, 0.50),
    "grok-3-mini-fast": (0.06, 0.40),
    "grok-4": (3.00, 15.00),
}
_DEFAULT_PRICING = (2.50, 10.00)


def _compute_cost(model: str, usage: Usage) -> float:
    input_price, output_price = _PRICING.get(model, _DEFAULT_PRICING)
    return (
        usage.input_tokens * input_price + usage.output_tokens * output_price
    ) / 1_000_000


class OpenAIAgentsClient:
    """ClaudeClient implementation backed by the openai-agents SDK."""

    def __init__(
        self,
        model: str | None,
        *,
        base_url: str | None = None,
        api_key: str | None = None,
    ) -> None:
        self._model = model or "gpt-4o"
        self._total_cost_usd = 0.0
        self._base_url = base_url
        self._api_key = api_key
        self._provider: OpenAIProvider | None = (
            OpenAIProvider(base_url=base_url, api_key=api_key)
            if base_url or api_key
            else None
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
        cwd: pathlib.Path | None = None,
        idle_timeout: float | None = None,
        extra_env: dict[str, str] | None = None,
    ) -> CompletedRun:
        effective_model = model or self._model
        agent = Agent(
            name=f"gremlins-{label}",
            instructions="You are a software engineering assistant.",
            tools=GREMLINS_TOOLS,
            model=effective_model,
        )
        ctx: dict[str, object] = {
            "cwd": str(cwd) if cwd is not None else None,
            "extra_env": extra_env,
        }
        if self._provider is not None:
            run_config = RunConfig(tracing_disabled=True, model_provider=self._provider)
        else:
            run_config = RunConfig(tracing_disabled=True)
        result = Runner.run_sync(agent, prompt, context=ctx, run_config=run_config)
        usage = result.context_wrapper.usage
        cost = _compute_cost(effective_model, usage)
        self._total_cost_usd += cost
        text = str(result.final_output) if result.final_output is not None else None
        return CompletedRun(exit_code=0, text_result=text, cost_usd=cost)

    def reap_all(self) -> None:
        pass

    @property
    def total_cost_usd(self) -> float:
        return self._total_cost_usd

    @property
    def base_url(self) -> str | None:
        return self._base_url

    @property
    def api_key(self) -> str | None:
        return self._api_key


def make_openai_client(model: str | None) -> OpenAIAgentsClient:
    api_key = os.environ.get("OPENAI_API_KEY")
    if not api_key:
        raise RuntimeError("OPENAI_API_KEY environment variable is not set")
    return OpenAIAgentsClient(model, api_key=api_key)


def make_xai_client(model: str | None) -> OpenAIAgentsClient:
    api_key = os.environ.get("XAI_API_KEY")
    if not api_key:
        raise RuntimeError("XAI_API_KEY environment variable is not set")
    return OpenAIAgentsClient(
        model or "grok-4",
        base_url="https://api.x.ai/v1",
        api_key=api_key,
    )
