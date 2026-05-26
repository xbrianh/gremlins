"""Handoff stage: runs the handoff agent once per boss loop iteration."""

from __future__ import annotations

import argparse
import json
import logging
import os
import pathlib
import re
import shutil
import sys
import threading
from collections.abc import Awaitable, Callable
from typing import Any, TypeVar, cast

from gremlins.clients.client import Client
from gremlins.executor.state import State
from gremlins.stages.agent import Agent
from gremlins.stages.base import Stage, get_client_from_dict
from gremlins.stages.outcome import Bail, Done, NeedsFix, Outcome
from gremlins.utils import proc
from gremlins.utils.yaml_io import load_bundled_prompt, render_bundled_prompt

logger = logging.getLogger(__name__)

CLAUDE_SANITIZE_MODEL = "haiku"

T = TypeVar("T")

HANDOFF_TIMEOUT = int(
    os.environ.get(
        "CHAIN_HANDOFF_TIMEOUT",
        os.environ.get("BOSSGREMLIN_HANDOFF_TIMEOUT", "3600"),
    )
)


def sanitize_model_for(spec: Client) -> str:
    return CLAUDE_SANITIZE_MODEL if spec.provider == "claude" else spec.model


def with_reap_after(client: Client, timeout: int | None, fn: Callable[[], T]) -> T:
    """Run fn, reaping the client's subprocesses if it doesn't return in time."""
    if timeout is None:
        return fn()
    timer = threading.Timer(timeout, client.reap_all)
    timer.daemon = True
    timer.start()
    try:
        return fn()
    finally:
        timer.cancel()


async def with_reap_after_async(
    client: Client, timeout: int | None, coro: Awaitable[T]
) -> T:
    """Await coro, reaping the client's subprocesses if it doesn't return in time."""
    if timeout is None:
        return await coro
    timer = threading.Timer(timeout, client.reap_all)
    timer.daemon = True
    timer.start()
    try:
        return await coro
    finally:
        timer.cancel()


def auto_name_out(plan_path: pathlib.Path) -> pathlib.Path:
    """Given plan.md → plan-001.md; given plan-001.md → plan-002.md, etc."""
    base = re.sub(r"-\d{3}$", "", plan_path.stem) or plan_path.stem
    parent = plan_path.parent
    n = 1
    while True:
        candidate = parent / f"{base}-{n:03d}.md"
        if not candidate.exists():
            return candidate
        n += 1


async def collect_git_context(
    base_ref: str | None, rev: str | None = None
) -> tuple[str, str, str]:
    """Return (branch_name, git_log, git_diff) since merge-base with base_ref."""
    target = base_ref or "main"
    inspect_rev = rev or "HEAD"

    result = await proc.run_async(["git", "rev-parse", "--verify", target])
    if result.returncode != 0:
        raise RuntimeError(f"--base ref not found in repo: {target!r}")

    if rev is not None:
        result = await proc.run_async(["git", "rev-parse", "--verify", rev])
        if result.returncode != 0:
            raise RuntimeError(f"--rev ref not found in repo: {rev!r}")

    result = await proc.run_async(["git", "rev-parse", "--abbrev-ref", inspect_rev])
    branch = result.stdout.strip() if result.returncode == 0 else inspect_rev
    if branch == "HEAD":
        sha = (await proc.run_async(["git", "rev-parse", inspect_rev])).stdout.strip()
        branch = f"(detached at {sha[:12]})" if sha else "(detached)"

    result = await proc.run_async(["git", "merge-base", inspect_rev, target])
    if result.returncode != 0:
        raise RuntimeError(
            f"could not compute merge-base between {inspect_rev!r} and {target!r}"
        )
    merge_base = result.stdout.strip()

    result = await proc.run_async(
        ["git", "log", f"{merge_base}..{inspect_rev}", "--oneline"], check=True
    )
    git_log = result.stdout.strip()

    result = await proc.run_async(
        ["git", "diff", f"{merge_base}..{inspect_rev}"], check=True
    )
    git_diff = result.stdout

    return branch, git_log, git_diff


def _load_spec_section(spec_text: str) -> str:
    spec_body = spec_text[:50000]
    spec_trunc = (
        f"\n(spec truncated to 50000 chars; {len(spec_text)} chars total)"
        if len(spec_text) > 50000
        else ""
    )
    return render_bundled_prompt(
        "handoff_spec_section.md", spec_body=spec_body, spec_trunc=spec_trunc
    )


def build_prompt(
    plan_text: str,
    branch: str,
    git_log: str,
    git_diff: str,
    out_path: pathlib.Path,
    child_plan_path: pathlib.Path,
    signal_path: pathlib.Path,
    spec_text: str | None = None,
) -> str:
    diff_body = git_diff[:50000] if git_diff else "(empty — no changes yet)"
    diff_trunc = (
        f"\n(diff truncated to 50000 chars; {len(git_diff)} chars total)"
        if len(git_diff) > 50000
        else ""
    )
    log_body = git_log if git_log else "(no commits yet — branch just started)"
    spec_section = _load_spec_section(spec_text) if spec_text is not None else ""
    style_section = render_bundled_prompt(
        "handoff_style_section.md",
        code_style=load_bundled_prompt("code_style.md").rstrip(),
    )
    return render_bundled_prompt(
        "handoff.md",
        spec_section=spec_section,
        style_section=style_section,
        plan_text=plan_text,
        branch=branch,
        log_body=log_body,
        diff_body=diff_body,
        diff_trunc=diff_trunc,
        out_path=out_path,
        child_plan_path=child_plan_path,
        signal_path=signal_path,
    ).rstrip()


def build_sanitize_prompt(rolling_plan_text: str, out_path: pathlib.Path) -> str:
    return render_bundled_prompt(
        "handoff_sanitize.md", rolling_plan_text=rolling_plan_text, out_path=out_path
    ).rstrip()


def _restore_rolling_plan(
    out_path: pathlib.Path, original_text: str, reason: str
) -> None:
    try:
        out_path.write_text(original_text, encoding="utf-8")
    except OSError as exc:
        sys.stderr.write(
            f"warning: {reason} — failed to restore original rolling plan: {exc}\n"
        )
        return
    sys.stderr.write(f"warning: {reason} — restored original rolling plan\n")


def _read_rolling_plan_for_sanitize(out_path: pathlib.Path) -> str | None:
    if not out_path.exists():
        sys.stderr.write(
            f"warning: sanitize skipped — rolling plan not found: {out_path}\n"
        )
        return None
    try:
        return out_path.read_text(encoding="utf-8")
    except OSError as exc:
        sys.stderr.write(
            f"warning: sanitize skipped — could not read rolling plan: {exc}\n"
        )
        return None


async def sanitize_rolling_plan(
    client: Client,
    out_path: pathlib.Path,
    spec: Client,
    *,
    timeout: int | None = None,
) -> None:
    plan_text = _read_rolling_plan_for_sanitize(out_path)
    if plan_text is None:
        return
    prompt = build_sanitize_prompt(plan_text, out_path)
    model = sanitize_model_for(spec)
    logger.info("sanitizing rolling plan (model: %s)", model)
    try:
        await with_reap_after_async(
            client,
            timeout,
            client.run(prompt, label="handoff:sanitize", model=model),
        )
    except Exception as exc:
        _restore_rolling_plan(out_path, plan_text, f"sanitize pass failed: {exc}")
        return
    try:
        sanitized_text = out_path.read_text(encoding="utf-8")
    except OSError as exc:
        _restore_rolling_plan(
            out_path,
            plan_text,
            f"sanitize pass completed but output could not be read: {exc}",
        )
        return
    if not sanitized_text.strip():
        _restore_rolling_plan(
            out_path, plan_text, "sanitize pass completed but output was empty"
        )


def _read_optional_spec(spec_arg: str | None) -> str | None:
    if spec_arg is None:
        return None

    spec_path = pathlib.Path(spec_arg).resolve()
    if not spec_path.exists():
        sys.stderr.write(
            f"warning: --spec does not exist, continuing without north-star context: {spec_path}\n"
        )
        return None
    if not spec_path.is_file():
        sys.stderr.write(
            f"warning: --spec is not a file, continuing without north-star context: {spec_path}\n"
        )
        return None
    if spec_path.stat().st_size == 0:
        sys.stderr.write(
            f"warning: --spec is empty, continuing without north-star context: {spec_path}\n"
        )
        return None

    try:
        return spec_path.read_text(encoding="utf-8")
    except UnicodeDecodeError:
        sys.stderr.write(
            f"warning: --spec is not valid UTF-8, continuing without north-star context: {spec_path}\n"
        )
    except OSError as exc:
        sys.stderr.write(
            f"warning: failed to read --spec, continuing without north-star context: {spec_path}: {exc}\n"
        )
    return None


def _parse_client_spec(client_arg: str) -> Client:
    try:
        return Client.parse(client_arg)
    except ValueError as exc:
        raise RuntimeError(str(exc)) from exc


async def run(
    client: Client,
    args: argparse.Namespace,
    *,
    run_fn: Callable[[str], Awaitable[None]] | None = None,
) -> int:
    plan_path = pathlib.Path(args.plan).resolve()
    if not plan_path.exists():
        sys.stderr.write(f"error: --plan does not exist: {plan_path}\n")
        return 1
    if not plan_path.is_file():
        sys.stderr.write(f"error: --plan is not a file: {plan_path}\n")
        return 1
    if plan_path.stat().st_size == 0:
        sys.stderr.write(f"error: --plan is empty: {plan_path}\n")
        return 1
    try:
        plan_text = plan_path.read_text(encoding="utf-8")
    except UnicodeDecodeError:
        sys.stderr.write(f"error: --plan is not valid UTF-8: {plan_path}\n")
        return 1
    except OSError as exc:
        sys.stderr.write(f"error: failed to read --plan {plan_path}: {exc}\n")
        return 1

    spec_text = _read_optional_spec(args.spec)

    if args.out:
        out_path = pathlib.Path(args.out).resolve()
        if not out_path.parent.exists():
            sys.stderr.write(
                f"error: --out parent directory does not exist: {out_path.parent}\n"
            )
            return 1
        if not out_path.parent.is_dir():
            sys.stderr.write(
                f"error: --out parent path is not a directory: {out_path.parent}\n"
            )
            return 1
    else:
        out_path = auto_name_out(plan_path)

    child_plan_path = out_path.parent / (out_path.stem + "-child" + out_path.suffix)
    signal_path = out_path.parent / (out_path.stem + ".state.json")

    try:
        branch, git_log, git_diff = await collect_git_context(args.base, rev=args.rev)
    except Exception as exc:
        sys.stderr.write(f"error: git context collection failed: {exc}\n")
        return 1

    try:
        prompt = build_prompt(
            plan_text=plan_text,
            branch=branch,
            git_log=git_log,
            git_diff=git_diff,
            out_path=out_path,
            child_plan_path=child_plan_path,
            signal_path=signal_path,
            spec_text=spec_text,
        )
    except RuntimeError as exc:
        sys.stderr.write(f"error: {exc}\n")
        return 1

    try:
        client_spec = _parse_client_spec(args.client)
    except RuntimeError as exc:
        sys.stderr.write(f"error: {exc}\n")
        return 1
    logger.info("running handoff agent (client: %s)", client_spec)
    try:
        if run_fn is not None:
            await run_fn(prompt)
        else:
            await with_reap_after_async(
                client,
                args.timeout,
                client.run(prompt, label="handoff", model=client_spec.model),
            )
    except Bail:
        raise
    except Exception as exc:
        sys.stderr.write(f"error: handoff agent failed: {exc}\n")
        return 1

    if not signal_path.exists():
        sys.stderr.write(f"error: signal file not written by agent: {signal_path}\n")
        return 1

    try:
        state = json.loads(signal_path.read_text(encoding="utf-8"))
    except Exception as exc:
        sys.stderr.write(f"error: could not parse signal file {signal_path}: {exc}\n")
        return 1

    exit_state = state.get("exit_state")
    if exit_state not in ("next-plan", "chain-done", "bail"):
        sys.stderr.write(
            f"error: signal file has unrecognized exit_state: {exit_state!r}\n"
        )
        return 1

    logger.info("handoff complete: %s", exit_state)
    if exit_state == "next-plan":
        child_plan = state.get("child_plan")
        if not child_plan:
            sys.stderr.write(
                "error: signal file exit_state is next-plan but child_plan is null\n"
            )
            return 1
        if not pathlib.Path(child_plan).exists():
            sys.stderr.write(
                f"error: child plan path in signal file does not exist: {child_plan}\n"
            )
            return 1
        logger.info("updated plan: %s", out_path)
        logger.info("child plan:   %s", child_plan)
        logger.info("signal file:  %s", signal_path)
    elif exit_state == "chain-done":
        logger.info("updated plan: %s", out_path)
        logger.info("signal file:  %s", signal_path)
    elif exit_state == "bail":
        reason = state.get("reason") or "(no reason given)"
        logger.info("bail reason:  %s", reason)
        logger.info("updated plan: %s", out_path)
        logger.info("signal file:  %s", signal_path)

    raw_followups = state.get("operator_followups")
    followups = (
        [str(item) for item in cast(list[Any], raw_followups) if str(item).strip()]
        if isinstance(raw_followups, list)
        else []
    )
    if followups:
        logger.info("operator follow-ups (%d):", len(followups))
        for item in followups:
            logger.info("  - %s", item)

    await sanitize_rolling_plan(
        client,
        out_path,
        spec=client_spec,
        timeout=min(args.timeout, 60) if args.timeout is not None else None,
    )
    return 0


class Handoff(Stage):
    type = "handoff"

    @classmethod
    def with_dict(cls, d: dict[str, Any], depth: int = 0) -> Handoff:
        stage = cls(d["name"])
        stage.client = get_client_from_dict(d)
        return stage

    def __init__(self, name: str) -> None:
        super().__init__(name)

    async def run(self, state: State) -> Outcome:
        session_dir = state.session_dir

        boss_spec = session_dir / "boss-spec.md"
        plan_md = session_dir / "plan.md"

        if not plan_md.is_file():
            raise RuntimeError(f"handoff stage: plan file not found: {plan_md}")

        if not boss_spec.exists():
            shutil.copyfile(plan_md, boss_spec)

        base_ref = (
            state.artifacts.resolve("base_ref").path.removeprefix("ref/")
            if state.artifacts.produced("base_ref")
            else await self._resolve_base_ref(state)
        )
        handoff_n = self._next_handoff_index(session_dir)

        prev_rolling = (
            session_dir / f"handoff-{handoff_n - 1:03d}.md" if handoff_n > 1 else None
        )
        current_plan = (
            str(prev_rolling)
            if prev_rolling and prev_rolling.exists()
            else str(plan_md)
        )

        state.record_stage_progress("handoff", parent_stage=state.parent_stage)
        exit_state, sig = await self._run_handoff(
            handoff_n=handoff_n,
            current_plan=current_plan,
            original_plan=str(boss_spec),
            base_ref=base_ref,
            session_dir=session_dir,
            state=state,
        )

        if exit_state == "chain-done":
            logger.info("chain complete after %d handoff(s)", handoff_n)
            shutil.copyfile(boss_spec, plan_md)
            return Done()

        if exit_state == "bail":
            reason = sig.get("reason") or "(no reason given)"
            logger.info("handoff bailed: %s", reason)
            state.record_bail(f"handoff bail: {reason}"[:200])
            raise Bail(f"chain halted by handoff: {reason}")

        # exit_state == "next-plan"
        child_plan_path = sig.get("child_plan") or ""
        if not child_plan_path or not os.path.isfile(child_plan_path):
            raise RuntimeError(
                f"handoff returned next-plan but child_plan not found: {child_plan_path!r}"
            )
        shutil.copyfile(child_plan_path, plan_md)
        return NeedsFix(f"next-plan: handoff {handoff_n}")

    async def _run_handoff(
        self,
        *,
        handoff_n: int,
        current_plan: str,
        original_plan: str,
        base_ref: str,
        session_dir: pathlib.Path,
        state: State,
    ) -> tuple[str, dict[str, Any]]:
        out_path = session_dir / f"handoff-{handoff_n:03d}.md"
        child_plan_path = session_dir / f"handoff-{handoff_n:03d}-child.md"
        signal_path = session_dir / f"handoff-{handoff_n:03d}.state.json"

        plan_text = pathlib.Path(current_plan).read_text(encoding="utf-8")
        forward_spec = (
            pathlib.Path(original_plan).read_bytes()
            != pathlib.Path(current_plan).read_bytes()
        )
        spec_text = (
            pathlib.Path(original_plan).read_text(encoding="utf-8")
            if forward_spec
            else None
        )

        logger.info(
            "handoff %d: plan=%s, spec=%s, base=%s",
            handoff_n,
            current_plan,
            original_plan if forward_spec else "(none)",
            base_ref[:12] if len(base_ref) >= 12 else base_ref,
        )

        branch, git_log, git_diff = await collect_git_context(base_ref)
        prompt = build_prompt(
            plan_text=plan_text,
            branch=branch,
            git_log=git_log,
            git_diff=git_diff,
            out_path=out_path,
            child_plan_path=child_plan_path,
            signal_path=signal_path,
            spec_text=spec_text,
        )

        agent = Agent(
            "handoff",
            [prompt],
            {},
            out_map={
                f"handoff-{handoff_n:03d}": f"file://session/handoff-{handoff_n:03d}.md"
            },
        )
        await with_reap_after_async(state.client, HANDOFF_TIMEOUT, agent.run(state))

        if not signal_path.exists():
            raise RuntimeError(f"handoff signal file not written: {signal_path}")

        try:
            sig_data: dict[str, Any] = json.loads(
                signal_path.read_text(encoding="utf-8")
            )
        except Exception as exc:
            raise RuntimeError(
                f"could not parse handoff signal file {signal_path}: {exc}"
            ) from exc

        exit_state = sig_data.get("exit_state", "")
        if exit_state not in ("next-plan", "chain-done", "bail"):
            raise RuntimeError(
                f"handoff signal file has unrecognized exit_state: {exit_state!r}"
            )

        sig_data["out_path"] = str(out_path)
        sig_data["signal_path"] = str(signal_path)
        logger.info("handoff %d result: %s", handoff_n, exit_state)

        await sanitize_rolling_plan(
            state.client, out_path, spec=state.client, timeout=60
        )

        return exit_state, sig_data

    async def _resolve_base_ref(self, state: State) -> str:
        r = await proc.run_async(["git", "rev-parse", "HEAD"], cwd=state.engine_ctx.cwd)
        return r.stdout.strip() if r.returncode == 0 else "HEAD"

    @staticmethod
    def _next_handoff_index(session_dir: pathlib.Path) -> int:
        indices: list[int] = []
        for p in session_dir.glob("handoff-*.state.json"):
            try:
                indices.append(int(p.stem.split(".")[0].split("-")[1]))
            except (IndexError, ValueError):
                pass
        return 1 + max(indices, default=0)
