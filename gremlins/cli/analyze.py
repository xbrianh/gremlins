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

def _read_log(log_path: pathlib.Path) -> str:
    """Read the entire log file."""
    if not log_path.is_file():
        return "(no log file)"

    try:
        return log_path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return "(log unreadable)"


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
    return completed.text_result or "(no output from model)"


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

    prompt = render_bundled_prompt(
        "analyze.md",
        state_json=state_json,
        log_text=log_text,
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
