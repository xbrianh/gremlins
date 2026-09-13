"""``gremlins analyze`` — analyze a gremlin's log with an LLM."""

from __future__ import annotations

import argparse
import asyncio
import json
import pathlib
import sys
from typing import Any

from _gremlins_core.clients import Client
from _gremlins_core.config import get_config, state_root

from gremlins.fleet.resolve import resolve_gremlin
from gremlins.fleet.state import load_state
from gremlins.utils.yaml_io import render_bundled_prompt

_LOG_MAX_BYTES = 50_000
_ARTIFACT_MAX_BYTES = 20_000


def _read_log(log_path: pathlib.Path) -> str:
    """Read the log file, truncating large logs to the tail."""
    if not log_path.is_file():
        return "(no log file)"

    try:
        raw = log_path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return "(log unreadable)"

    if not raw:
        return "(empty log)"

    if len(raw) > _LOG_MAX_BYTES:
        raw = "(log truncated — showing tail)\n\n…\n\n" + raw[-_LOG_MAX_BYTES:]

    return raw


def _read_artifacts(artifacts_dir: pathlib.Path) -> str:
    """Read artifact files from the artifacts directory, inlining content."""
    if not artifacts_dir.is_dir():
        return "(no artifacts directory)"

    entries = sorted(artifacts_dir.iterdir())
    if not entries:
        return ""

    parts: list[str] = []
    for entry in entries:
        if not entry.is_file():
            continue
        try:
            content = entry.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue

        if len(content) > _ARTIFACT_MAX_BYTES:
            content = content[:_ARTIFACT_MAX_BYTES] + "\n… (truncated)"

        parts.append(f"--- {entry.name} ---\n{content}")

    return "\n\n".join(parts)


def _resolve_client(client_spec: str | None, state: dict[str, Any]) -> Client:
    """Resolve the Client to use for analysis.

    Precedence: --client flag > state.json client > global config default.
    """
    if client_spec:
        return Client.parse(client_spec)

    state_client = str(state.get("client") or "")
    if state_client:
        try:
            return Client.parse(state_client)
        except Exception as exc:
            raise ValueError(
                f"cannot parse client {state_client!r} from state.json ({exc}) — "
                "pass --client SPEC to override it"
            ) from exc

    global_client = get_config().default_client
    if global_client:
        return Client.parse(global_client)

    raise ValueError(
        "no client available for analysis — pass --client or configure a default-client"
    )


async def _run_analysis(client: Client, prompt: str) -> str:
    """Run the analysis prompt through the client and return the text result."""
    completed = await client.run(
        prompt,
        label="analyze",
        model=None,
        capture_events=False,
        max_retries=1,
    )
    text = completed.text_result
    if text is None:
        return "(no output from model)"
    if not text.strip():
        turns = completed.token_usage.turns if completed.token_usage else 0
        return f"(empty response — stream ended after {turns} turn(s) with no text)"
    return text


def analyze_main(argv: list[str]) -> int:
    p = argparse.ArgumentParser(
        prog="gremlins analyze",
        description="Analyze a gremlin's log with an LLM.",
    )
    p.add_argument("gremlin_id", metavar="gremlin-id", help="Gremlin to analyze.")
    p.add_argument(
        "--client",
        metavar="SPEC",
        help="Client specifier (e.g. 'openai:gpt-4o'). "
        "Overrides the gremlin's own client.",
    )
    args = p.parse_args(argv)

    state_root_dir = pathlib.Path(state_root())
    if not state_root_dir.is_dir():
        print("No gremlins state root — nothing to analyze.", file=sys.stderr)
        return 1

    resolved = resolve_gremlin(args.gremlin_id)
    if resolved is None:
        return 1

    gremlin_id, sf, wdir = resolved
    state = load_state(sf)
    if not state:
        print(f"error: could not read state for {gremlin_id}", file=sys.stderr)
        return 1

    # Build the prompt data.
    state_json = json.dumps(state, indent=2, default=str)
    log_path = pathlib.Path(wdir) / "log"
    log_text = _read_log(log_path)

    artifacts_text = _read_artifacts(pathlib.Path(wdir) / "artifacts")

    prompt = render_bundled_prompt(
        "analyze.md",
        state_json=state_json,
        log_text=log_text,
    )

    if artifacts_text:
        prompt += (
            "\n\nHere are the artifacts produced by the gremlin run:\n\n"
            + artifacts_text
        )

    try:
        client = _resolve_client(args.client, state)
    except ValueError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1

    print(f"Analyzing gremlin {gremlin_id} with {client}...\n", file=sys.stderr)

    try:
        result = asyncio.run(_run_analysis(client, prompt))
    except Exception as exc:
        print(f"error: analysis failed: {exc}", file=sys.stderr)
        return 1

    print(result)
    return 0
