from __future__ import annotations

import pathlib

from agents import Agent, RunConfig, Runner, Usage

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
    "o1": (15.00, 60.00),
    "o1-mini": (1.10, 4.40),
    "o3": (10.00, 40.00),
    "o3-mini": (1.10, 4.40),
    "o4-mini": (1.10, 4.40),
}
_DEFAULT_PRICING = (2.50, 10.00)


def _compute_cost(model: str, usage: Usage) -> float:
    input_price, output_price = _PRICING.get(model, _DEFAULT_PRICING)
    return (
        usage.input_tokens * input_price + usage.output_tokens * output_price
    ) / 1_000_000


class OpenAIAgentsClient:
    """ClaudeClient implementation backed by the openai-agents SDK."""

    def __init__(self, model: str | None) -> None:
        self._model = model or "gpt-4o"
        self._total_cost_usd = 0.0

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
        effective_model = model or self._model
        agent = Agent(
            name=f"gremlins-{label}",
            instructions="You are a software engineering assistant.",
            tools=GREMLINS_TOOLS,
            model=effective_model,
        )
        ctx: dict[str, str | None] = {"cwd": str(cwd) if cwd is not None else None}
        result = Runner.run_sync(
            agent,
            prompt,
            context=ctx,
            run_config=RunConfig(tracing_disabled=True),
        )
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


def make_openai_client(model: str | None) -> OpenAIAgentsClient:
    return OpenAIAgentsClient(model)
