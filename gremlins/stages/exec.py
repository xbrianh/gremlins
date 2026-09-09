from __future__ import annotations

import logging
import os
import pathlib
import time
from typing import TYPE_CHECKING, Any, cast

from _gremlins_core.artifacts import Uri, resolve_interpolation_map
from _gremlins_core.stages import _BAIL_KEY, FRAMEWORK_KEYS, Bail, Done, Outcome

from gremlins.stages.base import Stage
from gremlins.utils import proc as _proc

logger = logging.getLogger(__name__)

if TYPE_CHECKING:
    from gremlins.executor.gremlin import Gremlin


def _is_bail_uri(uri_str: str, loop_iter: str = "") -> bool:
    """True when *uri_str* names a reserved bail artifact."""
    if uri_str == _BAIL_KEY:
        return True
    if not loop_iter:
        return False
    expected = f"artifact://{loop_iter}/bail"
    return uri_str == expected or uri_str == "artifact://{loop_iter}/bail"


class Exec(Stage):
    type = "exec"

    def __init__(
        self,
        name: str,
        options: dict[str, Any],
        *,
        interpolation_map: dict[str, str] | None = None,
        bind_map: dict[str, str] | None = None,
    ) -> None:
        super().__init__(name)
        self.options = options
        self.interpolation_map = interpolation_map or {}
        self.bind_map = bind_map or {}

    @classmethod
    def with_dict(cls, d: dict[str, Any], depth: int = 0) -> Exec:
        name = d.get("name") or ""
        raw_interpolation: object = d.get("interpolation") or {}
        raw_bind: object = d.get("bind") or {}
        if "in" in d or "out" in d:
            raise ValueError(
                f"stage {name!r}: 'in'/'out' keys are no longer supported; "
                f"use 'interpolation'/'bind' with URI values"
            )
        if not isinstance(raw_interpolation, dict):
            raise ValueError(f"stage {name!r}: 'interpolation' must be a mapping")
        if not isinstance(raw_bind, dict):
            raise ValueError(f"stage {name!r}: 'bind' must be a mapping")
        for k in cast(dict[str, Any], d.get("options") or {}):
            if k in FRAMEWORK_KEYS:
                raise ValueError(
                    f"stage {name!r}: option key {k!r} collides with framework substitution variable"
                )
        return cls(
            name,
            d.get("options") or {},
            interpolation_map=cast(dict[str, str], raw_interpolation),
            bind_map=cast(dict[str, str], raw_bind),
        )

    async def run(self, gremlin: Gremlin) -> Outcome:
        state = gremlin.state
        if state is None:
            raise RuntimeError("exec stage requires gremlin.state to be initialized")

        # Resolve interpolation vars (inputs consumed by commands)
        try:
            counter = state.loop_iter
            interpolation_map = resolve_interpolation_map(
                state.artifacts, self.interpolation_map, loop_iter=counter
            )
        except ValueError as exc:
            raise Bail(f"exec {self.name}: {exc}") from exc

        # Register bind URIs and collect output paths
        bind_paths: dict[str, str] = {}
        for raw_key, raw_uri_str in self.bind_map.items():
            key = self.substitute_vars(raw_key, state, interpolation_map)
            optional = key.endswith("?")
            if optional:
                key = key[:-1]
            uri_str = self.substitute_vars(raw_uri_str, state, interpolation_map)
            uri_str = uri_str.replace("{loop_iter}", counter)
            uri = Uri.parse(uri_str)
            bind_paths[key] = state.artifacts.register(uri)

        # Merge: bind output paths shadow interpolation keys on collision
        subst_vars = {**interpolation_map, **bind_paths}

        raw_cmds = [c.strip() for c in self.options.get("cmds", []) if c.strip()]
        cmds = [self.substitute_vars(c, state, subst_vars) for c in raw_cmds]
        bail_triggered = False
        shell_output = ""
        shell_rc = 0
        raw_timeout = self.options.get("timeout")
        timeout: float | None = float(raw_timeout) if raw_timeout is not None else None
        logger.info(
            "exec %s: entering stage, %d command(s), timeout=%s, cwd=%s",
            self.name,
            len(cmds),
            f"{timeout}s" if timeout is not None else "none",
            state.cwd,
        )
        if cmds:
            joined = " && ".join(cmds)
            _cmd_summary = " && ".join(c.replace("\n", "\\n") for c in raw_cmds)
            if len(_cmd_summary) > 400:
                _cmd_summary = _cmd_summary[:400] + "..."
            logger.info(
                "exec %s: running %d command(s): %s",
                self.name,
                len(cmds),
                _cmd_summary,
            )
            for i, c in enumerate(cmds):
                logger.debug(
                    "exec %s:   cmd[%d] %s", self.name, i, c.replace("\n", "\\n")
                )
            logger.debug(
                "exec %s: spawning subprocess, artifact_dir=%s",
                self.name,
                state.artifact_dir,
            )
            _t0 = time.monotonic()
            result = await _proc.run_shell_async(
                joined,
                cwd=pathlib.Path(state.cwd),
                env={
                    **os.environ,
                    "GREMLINS_ARTIFACT_DIR": str(state.artifact_dir),
                },
                timeout=timeout,
            )
            elapsed = time.monotonic() - _t0
            log_path = gremlin.state_dir / f"exec-{self.name}.log"
            raw_output = result.stdout + result.stderr
            log_path.write_text(raw_output or "(no output)\n", encoding="utf-8")
            shell_output = raw_output.strip()
            shell_rc = result.returncode
            out_summary = f" (output: {len(raw_output)} chars)" if raw_output else ""
            logger.info(
                "exec %s: done in %.2fs rc=%d%s",
                self.name,
                elapsed,
                shell_rc,
                out_summary,
            )
            if raw_output:
                logger.info(
                    "exec %s: output written to %s (%d chars)",
                    self.name,
                    log_path,
                    len(raw_output),
                )
            if shell_rc != 0:
                if shell_output:
                    tail = shell_output[-500:]
                    logger.warning(
                        "exec %s: rc=%d, last 500 chars of output:\n%s",
                        self.name,
                        shell_rc,
                        tail,
                    )
                if any(_is_bail_uri(v, counter) for v in self.bind_map.values()):
                    bail_triggered = True
                else:
                    raise Bail(f"exec {self.name}: exited {shell_rc}")

        # Post-command verification: confirm each registered artifact
        # was actually written to disk.
        for raw_key, raw_uri_str in self.bind_map.items():
            key = self.substitute_vars(raw_key, state, interpolation_map)
            optional = key.endswith("?")
            if optional:
                key = key[:-1]
            uri_str = self.substitute_vars(raw_uri_str, state, interpolation_map)
            uri_str = uri_str.replace("{loop_iter}", counter)
            if _is_bail_uri(uri_str, counter) and not bail_triggered:
                continue
            uri = Uri.parse(uri_str)
            if not state.artifacts.exists(str(uri)):
                if optional:
                    continue
                if _is_bail_uri(uri_str, counter):
                    if bail_triggered:
                        continue
                    msg = f"exec {self.name}: exited {shell_rc}"
                    if shell_output:
                        msg += f"\n{shell_output}"
                    raise Bail(msg) from None
                raise Bail(f"exec {self.name}: artifact {uri} was not produced")

        return Done()
