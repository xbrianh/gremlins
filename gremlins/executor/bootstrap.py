"""Run bootstrap commands in a worktree before pipeline stages begin.

Used at gremlin launch and in parallel child subprocesses so that
every fresh worktree gets its dev environment (venv, etc.) set up.

Supports gremlins: DSL commands in launch_cmds:
  gremlins:bind_artifact(<source_key>, <artifact_key>, <uri_template>)
    Resolves a bootstrap source value (filepath or inline text)
    and binds it as an artifact in the registry.
"""

from __future__ import annotations

import logging
import os
import pathlib
import re
import shutil
from collections.abc import Mapping
from typing import TYPE_CHECKING, Any

from _gremlins_core.artifacts import Uri
from _gremlins_core.schemas import (
    Bootstrap,
    source_env,
    substitute_bootstrap_vars,
)

from gremlins.utils import proc

if TYPE_CHECKING:
    from gremlins.executor.gremlin import Gremlin

logger = logging.getLogger(__name__)


# ---------------------------------------------------------------------------
# gremlins: DSL command infrastructure
# ---------------------------------------------------------------------------

_GREMLINS_CMD_RE = re.compile(r"gremlins:([a-z_]+)\(([^)]*)\)")


async def run_bootstrap(
    cmds: list[str],
    cwd: pathlib.Path,
    extra_env: dict[str, str] | None = None,
) -> None:
    """Run shell commands in cwd. Non-zero exit raises RuntimeError."""
    cmds = [c.rstrip() for c in cmds if c.strip()]
    if not cmds:
        return
    env = dict(os.environ)
    env["GREMLINS_BOOTSTRAP_CWD"] = str(cwd)
    if extra_env:
        env.update(extra_env)
    result = await proc.run_shell_async(" && ".join(cmds), cwd=cwd, env=env)
    if result.returncode != 0:
        err = (result.stderr or result.stdout).strip()
        logger.error("bootstrap failed (exit %d): %s", result.returncode, err[:2000])
        raise RuntimeError(f"bootstrap failed (exit {result.returncode}): {err[:500]}")
    logger.info("bootstrap ok")


def _parse_gremlins_command(raw: str) -> tuple[str, list[str]] | None:
    """Parse a gremlins: DSL command from a launch_cmd string.

    Returns (command_name, [arg1, arg2, ...]) on success, None for plain shell commands.

    Syntax: gremlins:<name>(<arg1>, <arg2>, ...)
    Arguments are split on commas (outside parens) and stripped.  Quoted strings
    have their surrounding quotes removed.
    """
    match = _GREMLINS_CMD_RE.match(raw)
    if not match:
        return None
    cmd_name = match.group(1)
    args_raw = match.group(2)
    rest = raw[match.end() :]
    if rest.strip():
        # Trailing content after the closing paren — not a valid DSL command
        return None
    args = _split_dsl_args(args_raw)
    return cmd_name, args


def _split_dsl_args(args_raw: str) -> list[str]:
    """Split comma-separated DSL arguments, handling quotes."""
    parts: list[str] = []
    current: list[str] = []
    in_quote: str | None = None
    for ch in args_raw:
        if in_quote:
            if ch == in_quote:
                in_quote = None
            else:
                current.append(ch)
        elif ch in ('"', "'"):
            in_quote = ch
        elif ch == ",":
            parts.append("".join(current).strip())
            current = []
        else:
            current.append(ch)
    tail = "".join(current).strip()
    if tail:
        parts.append(tail)
    return parts


def _parse_bind_artifact_args(
    args: list[str],
) -> tuple[str, str, str]:
    """Validate and unpack bind_artifact arguments.

    Returns (source_key, artifact_key, uri).

    Accepts either 2 or 3 arguments:
      - 2-arg form (current): bind_artifact(uri, source_key)
        Artifact key is derived from the URI.
      - 3-arg form (legacy): bind_artifact(source_key, artifact_key, uri)
    """
    if len(args) == 2:
        uri_str, source_key = args
        if not uri_str:
            raise ValueError("bind_artifact: uri must be non-empty")
        if not source_key:
            raise ValueError("bind_artifact: source_key must be non-empty")
        if "://" not in uri_str:
            raise ValueError(
                f"bind_artifact: first argument {uri_str!r} does not look like a URI "
                f"(expected 'artifact://...'); use the 2-arg form: "
                f"bind_artifact(uri, source_key) or the 3-arg form: "
                f"bind_artifact(source_key, artifact_key, uri)"
            )
        return source_key, uri_str, uri_str
    if len(args) != 3:
        raise ValueError(
            f"bind_artifact requires 2 or 3 arguments (uri, source_key) "
            f"or (source_key, artifact_key, uri), got {len(args)}"
        )
    source_key = args[0]
    artifact_key = args[1]
    uri = args[2]
    if not source_key:
        raise ValueError("bind_artifact: source_key must be non-empty")
    if not artifact_key:
        raise ValueError("bind_artifact: artifact_key must be non-empty")
    if not uri:
        raise ValueError("bind_artifact: uri must be non-empty")
    return source_key, artifact_key, uri


async def _execute_bind_artifact(
    source_key: str,
    artifact_key: str,
    uri_str: str,
    *,
    stage_inputs: Mapping[str, Any],
    gremlin: Gremlin,
) -> None:
    """Resolve a source value, write it to the artifact dir, and register it.

    Source value resolution:
    - Empty / missing → no-op (optional source)
    - Existing filepath → copies the file to artifact_dir
    - Inline text → written directly

    After writing, the artifact is registered via register().
    """
    value = stage_inputs.get(source_key)
    if value is None or value == "":
        return  # optional source, nothing to bind
    value_str = str(value)

    uri = Uri.parse(uri_str)
    dest_path = pathlib.Path(gremlin.registry.register(uri))
    dest_path.parent.mkdir(parents=True, exist_ok=True)

    if os.path.isfile(value_str):
        shutil.copy2(value_str, dest_path)
    else:
        project_root = getattr(gremlin, "project_root", None) or ""
        project_path = os.path.join(project_root, value_str) if project_root else None
        if project_path and os.path.isfile(project_path):
            shutil.copy2(project_path, dest_path)
        else:
            dest_path.write_text(value_str, encoding="utf-8")

    # Also register under the artifact_key so it can be looked up by name
    gremlin.registry._set(artifact_key, str(dest_path))


_DSL_DISPATCH: dict[str, object] = {
    "bind_artifact": _execute_bind_artifact,
}


async def _run_dsl_command(
    cmd_name: str,
    args: list[str],
    *,
    stage_inputs: Mapping[str, Any],
    gremlin: Gremlin,
) -> None:
    """Dispatch a parsed gremlins: DSL command to its handler."""
    handler = _DSL_DISPATCH.get(cmd_name)
    if handler is None:
        raise ValueError(
            f"unknown gremlins: command {cmd_name!r}; "
            f"known: {', '.join(sorted(_DSL_DISPATCH))}"
        )
    if cmd_name == "bind_artifact":
        source_key, artifact_key, uri_str = _parse_bind_artifact_args(args)
        await _execute_bind_artifact(
            source_key,
            artifact_key,
            uri_str,
            stage_inputs=stage_inputs,
            gremlin=gremlin,
        )
    else:
        raise ValueError(f"unhandled DSL command: {cmd_name!r}")


async def run_pipeline_bootstrap(
    bootstrap: Bootstrap,
    *,
    cwd: pathlib.Path,
    stage_inputs: Mapping[str, Any],
    gremlin: Gremlin,
    include_launch: bool,
) -> None:
    """Run worktree cmds, then (main first start only) launch_cmds and cli_out.

    Launch_cmds entries starting with ``gremlins:`` are parsed as DSL commands
    and executed inline; everything else runs as shell commands joined with ``&&``.
    """
    if bootstrap.cmds:
        await run_bootstrap(bootstrap.cmds, cwd)
    if not include_launch:
        return
    if bootstrap.launch_cmds:
        logger.info("running %d launch command(s)", len(bootstrap.launch_cmds))
        env = source_env(bootstrap.source, stage_inputs)
        shell_cmds: list[str] = []
        for c in bootstrap.launch_cmds:
            parsed = _parse_gremlins_command(c)
            if parsed:
                cmd_name, args = parsed
                logger.info("launch DSL: %s(%s)", cmd_name, ", ".join(args))
                await _run_dsl_command(
                    cmd_name,
                    args,
                    stage_inputs=stage_inputs,
                    gremlin=gremlin,
                )
            else:
                shell_cmds.append(substitute_bootstrap_vars(c, cwd=cwd))
        if shell_cmds:
            await run_bootstrap(shell_cmds, cwd, extra_env=env)
    if bootstrap.cli_out:
        from _gremlins_core.stages import Exec

        binder = Exec("bootstrap", {}, bind_map=dict(bootstrap.cli_out))
        await binder.run(gremlin)
