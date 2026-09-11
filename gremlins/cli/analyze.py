"""``gremlins analyze`` — analyze a gremlin's log and artifacts with an LLM."""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import pathlib
import sys
from typing import Any

from _gremlins_core.clients import Client
from _gremlins_core.config import get_config, state_root

from gremlins.fleet.resolve import resolve_gremlin
from gremlins.fleet.state import load_state
from gremlins.utils.yaml_io import render_bundled_prompt

_LOG_MAX_BYTES = 50_000
_LOG_TAIL_LINES = 2_000
_ARTIFACT_MAX_SIZE = 4_000
_ARTIFACT_MAX_COUNT = 20


def _read_log_tail(log_path: pathlib.Path) -> str:
    """Read the tail of the log file, up to _LOG_MAX_BYTES."""
    if not log_path.is_file():
        return "(no log file)"

    try:
        size = log_path.stat().st_size
    except OSError:
        return "(log unreadable)"

    if size == 0:
        return "(empty log)"

    if size <= _LOG_MAX_BYTES:
        try:
            return log_path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            return "(log unreadable)"

    try:
        with open(log_path, "rb") as f:
            f.seek(-_LOG_MAX_BYTES, os.SEEK_END)
            chunk = f.read(_LOG_MAX_BYTES)
    except OSError:
        # Log may have been truncated/rotated between stat and seek —
        # fall back to reading from the start.
        try:
            with open(log_path, "rb") as f:
                chunk = f.read(_LOG_MAX_BYTES)
        except OSError:
            return "(log unreadable)"

    text = chunk.decode("utf-8", errors="replace")
    # Drop the partial first line if we started mid-line.
    if text.startswith("\n"):
        text = text[1:]
    else:
        nl = text.find("\n")
        if nl > 0:
            text = text[nl + 1 :]
    lines = text.splitlines()
    if len(lines) > _LOG_TAIL_LINES:
        text = "\n".join(lines[-_LOG_TAIL_LINES:])
    return text


def _read_artifact_listing(wdir: str) -> str:
    """Build a listing of artifacts with truncated content previews."""
    artifacts_dir = pathlib.Path(wdir) / "artifacts"
    if not artifacts_dir.is_dir():
        return "(no artifacts directory)"

    entries = sorted(p for p in artifacts_dir.iterdir() if p.is_file())
    if not entries:
        return "(no artifact files)"

    if len(entries) > _ARTIFACT_MAX_COUNT:
        entries = entries[:_ARTIFACT_MAX_COUNT]
        truncated = True
    else:
        truncated = False

    parts: list[str] = []
    for fpath in entries:
        try:
            size = fpath.stat().st_size
        except OSError:
            content = "(binary or unreadable)"
        else:
            if size > _ARTIFACT_MAX_SIZE:
                try:
                    with open(fpath, "rb") as f:
                        content = f.read(_ARTIFACT_MAX_SIZE).decode(
                            "utf-8", errors="replace"
                        )
                except OSError:
                    content = "(binary or unreadable)"
                else:
                    content += "\n... [truncated]"
            else:
                try:
                    content = fpath.read_text(encoding="utf-8", errors="replace")
                except OSError:
                    content = "(binary or unreadable)"
        parts.append(f"### {fpath.name}\n\n```\n{content}\n```")

    result = "\n\n".join(parts)
    if truncated:
        result += f"\n\n... (showing first {_ARTIFACT_MAX_COUNT} of more artifacts)"
    return result


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
        description="Analyze a gremlin's log and artifacts with an LLM.",
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
    log_tail = _read_log_tail(log_path)
    artifact_listing = _read_artifact_listing(wdir)

    prompt = render_bundled_prompt(
        "analyze.md",
        state_json=state_json,
        log_tail=log_tail,
        artifact_listing=artifact_listing,
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
