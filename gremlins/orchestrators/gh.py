"""Orchestrator entry point for the gh pipeline."""

from __future__ import annotations

import argparse
import json
import logging
import os
import pathlib
import re
import shutil
import subprocess
import sys
from typing import NoReturn

import yaml

from gremlins.clients import ClientSpec, to_client
from gremlins.clients.protocol import ClaudeClient
from gremlins.clients.resolve import (
    PACKAGE_DEFAULT,
    collect_stage_specs,
    load_stage_specs_from_state,
    validate_stage_specs,
)
from gremlins.env_file import load_env_file
from gremlins.gh_utils import get_repo, parse_issue_ref, view_issue
from gremlins.logging_setup import configure_logging
from gremlins.orchestrators.pipeline import GHPipeline, read_state_field
from gremlins.pipeline import (
    load_pipeline,
    resolve_pipeline_path,
)
from gremlins.runner import install_signal_handlers
from gremlins.state import (
    patch_state,
    resolve_session_dir,
    resolve_state_file,
)

logger = logging.getLogger(__name__)

REF_RE = re.compile(r"^[A-Za-z0-9._/#-]+$")


def die(msg: str) -> NoReturn:
    sys.stderr.write(f"error: {msg}\n")
    sys.stderr.flush()
    sys.exit(1)


def _parse_gh_args(argv: list[str]) -> argparse.Namespace:
    usage = (
        "usage: gremlins.cli gh [-r <ref>] [--resume-from <stage>] "
        "[--plan <path|issue-ref>] [--spec <path>] "
        '[--pipeline <name-or-path>] [--client <provider:model>] "<instructions>"'
    )
    parser = argparse.ArgumentParser(add_help=False, usage=usage)
    parser.add_argument("-r", dest="ref", default="")
    parser.add_argument("--resume-from", dest="resume_from", default=None)
    parser.add_argument("--plan", dest="plan_source", default=None)
    parser.add_argument("--spec", dest="spec_path", default=None)
    parser.add_argument("--pipeline", dest="pipeline", default=None)
    parser.add_argument("--client", dest="client", default=None)
    parser.add_argument("instructions", nargs="*")
    args = parser.parse_args(argv)

    if args.plan_source:
        if args.instructions:
            die("--plan and positional instructions are mutually exclusive")
    else:
        if args.resume_from is None and not args.instructions:
            die(usage)

    if args.ref and not REF_RE.match(args.ref):
        die(f"invalid -r ref: {args.ref} (allowed: A-Z a-z 0-9 . _ / # -)")

    return args


_parse_issue_ref = parse_issue_ref


def _fetch_issue_body(issue_num: str, repo: str) -> str:
    try:
        issue_data = view_issue(issue_num, repo)
    except RuntimeError as exc:
        die(f"could not fetch issue #{issue_num} body: {exc}")
    body = (issue_data.get("body") or "").strip()
    if not body:
        die(f"issue #{issue_num} has an empty body")
    return body


def _update_description_from_plan(
    plan_md: pathlib.Path,
    state_file: pathlib.Path | None,
    gr_id: str | None = None,
    *,
    issue_title: str = "",
) -> None:
    if state_file is None or not state_file.exists():
        return
    try:
        data = json.loads(state_file.read_text(encoding="utf-8"))
        if data.get("description_explicit"):
            return
        if issue_title:
            patch_state(gr_id, description=issue_title[:60])
            return
        lines = plan_md.read_text(encoding="utf-8").splitlines()[:50]
        h1 = ""
        for line in lines:
            m = re.match(r"^#\s+(.+)", line)
            if m:
                h1 = m.group(1)[:60]
                break
        if h1:
            patch_state(gr_id, description=h1)
    except Exception:
        pass


def _resolve_plan_source(
    *,
    plan_source: str,
    repo: str,
    plan_md: pathlib.Path,
    model: str | None,
    client: ClaudeClient,
    state_file: pathlib.Path | None,
    gr_id: str | None = None,
) -> tuple[str, str, str]:
    """Resolve --plan <source> into (issue_url, issue_num, issue_body)."""
    if plan_md.exists() and plan_md.stat().st_size > 0:
        issue_url = read_state_field(state_file, "issue_url")
        issue_num = read_state_field(state_file, "issue_num")
        issue_body = plan_md.read_text(encoding="utf-8")
        label = f" (issue #{issue_num})" if issue_num else ""
        logger.info("[1/8] plan resumed from snapshot: %s%s", plan_md, label)
        return issue_url, issue_num, issue_body

    if pathlib.Path(plan_source).is_file():
        src = pathlib.Path(plan_source)
        if src.stat().st_size == 0:
            die(f"--plan: file is empty: {plan_source}")
        issue_body = src.read_text(encoding="utf-8")
        logger.info(
            "[1/8] plan supplied via --plan (file): %s — posting as GitHub issue",
            plan_source,
        )

        title_prompt = (
            "Produce a concise GitHub issue title (under 80 characters) "
            "summarizing the spec below. Output ONLY the title, nothing else."
            f"\n\n{issue_body}"
        )
        completed = client.run(
            title_prompt,
            label="plan-title",
            model=model,
        )
        parts = (completed.text_result or "").strip().splitlines()
        issue_title = parts[0][:80] if parts else ""
        if not issue_title:
            die("--plan: title agent returned empty output")

        r = subprocess.run(
            [
                "gh",
                "issue",
                "create",
                "--repo",
                repo,
                "--title",
                issue_title,
                "--body-file",
                plan_source,
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        if r.returncode != 0:
            die(f"--plan: failed to create GitHub issue: {r.stderr.strip()}")
        create_out = r.stdout + r.stderr
        m = re.search(r"https://github\.com/[^ )]+/issues/[0-9]+", create_out)
        if not m:
            die(
                f"--plan: could not extract issue URL from gh output: {create_out.strip()}"
            )
        issue_url = m.group(0)
        issue_num = issue_url.split("/")[-1]

        patch_state(gr_id, issue_url=issue_url, issue_num=issue_num)
        logger.info("issue: %s", issue_url)
        shutil.copyfile(src, plan_md)
    else:
        target_repo, issue_ref = _parse_issue_ref(plan_source, repo)
        if target_repo is None or issue_ref is None:
            die(
                f"--plan: not a readable file or recognized issue reference: {plan_source}"
            )

        try:
            issue_data = view_issue(issue_ref, target_repo)
        except RuntimeError as exc:
            die(f"--plan: {exc}")
        issue_body = issue_data.get("body") or ""
        if not issue_body:
            die(f"--plan: issue {plan_source} has an empty body")

        resolved_url = issue_data.get("url") or ""
        resolved_num = str(issue_data.get("number") or "")
        issue_title = (issue_data.get("title") or "")[:60]

        if target_repo == repo:
            issue_url = resolved_url
            issue_num = resolved_num
        else:
            issue_url = ""
            issue_num = ""

        plan_md.write_text(issue_body + "\n", encoding="utf-8")
        logger.info(
            "[1/8] plan supplied via --plan (issue %s#%s)", target_repo, issue_ref
        )

    patch_state(gr_id, issue_url=issue_url, issue_num=issue_num)
    _update_description_from_plan(
        plan_md, state_file, gr_id=gr_id, issue_title=issue_title
    )
    return issue_url, issue_num, issue_body


def gh_main(
    argv: list[str], *, client: ClaudeClient | None = None, gr_id: str | None = None
) -> int:
    configure_logging()
    args = _parse_gh_args(argv)
    if os.environ.get("GREMLINS_TEST_NOOP_PIPELINE"):
        return 0

    env_file = pathlib.Path(".gremlins/env")
    if env_file.is_file():
        try:
            os.environ.update(load_env_file(env_file))
        except RuntimeError as exc:
            die(str(exc))

    if shutil.which("claude") is None:
        die("claude CLI not found")
    if shutil.which("gh") is None:
        die("gh CLI not found")

    cli_spec: ClientSpec | None = None
    if args.client:
        try:
            cli_spec = ClientSpec.parse(args.client)
        except ValueError as exc:
            die(str(exc))

    try:
        pipeline = load_pipeline(
            resolve_pipeline_path(args.pipeline or "gh", pathlib.Path.cwd())
        )
    except (FileNotFoundError, ValueError, yaml.YAMLError) as exc:
        die(str(exc))

    # Load or resolve stage specs; state.json is authoritative on resume
    stage_specs: dict[str, ClientSpec] = {}
    state_file = resolve_state_file(gr_id)
    if args.resume_from and gr_id:
        try:
            stage_specs = load_stage_specs_from_state(gr_id)
        except Exception as exc:
            die(f"--resume-from: corrupt state.json stage_clients: {exc}")
        if not stage_specs:
            die(
                "--resume-from: stage_clients not found in state.json (rerun from scratch?)"
            )
    if not stage_specs:
        stage_specs = collect_stage_specs(pipeline, cli_spec)
        if gr_id:
            patch_state(
                gr_id, stage_clients={k: str(v) for k, v in stage_specs.items()}
            )

    # Create one client instance per unique spec (or reuse injected test client)
    _spec_clients: dict[str, ClaudeClient] = {}

    def _client_for_spec(spec: ClientSpec) -> ClaudeClient:
        if client is not None:
            return client
        key = str(spec)
        if key not in _spec_clients:
            _spec_clients[key] = to_client(spec)
        return _spec_clients[key]

    for spec in stage_specs.values():
        _client_for_spec(spec)

    # Determine the overall effective client for plan-title and signal handlers
    default_spec = cli_spec or pipeline.default_client or PACKAGE_DEFAULT
    effective_client = client if client is not None else _client_for_spec(default_spec)
    default_model = default_spec.model

    session_dir = resolve_session_dir(gr_id)

    if client is not None:
        _signal_clients = [client]
    elif _spec_clients:
        _signal_clients = list(_spec_clients.values())
    else:
        _signal_clients = [effective_client]

    try:
        validate_stage_specs(stage_specs, pipeline)
    except ValueError as exc:
        die(str(exc))

    repo = get_repo()
    plan_md = session_dir / "plan.md"
    spec_file = session_dir / "spec.md"

    logger.info("session: %s", session_dir)

    if args.spec_path and not spec_file.exists():
        spec_src = pathlib.Path(args.spec_path)
        if not spec_src.is_file():
            die(f"--spec: file not found: {args.spec_path}")
        if spec_src.stat().st_size == 0:
            die(f"--spec: file is empty: {args.spec_path}")
        shutil.copyfile(spec_src, spec_file)

    try:
        pipe = GHPipeline(
            pipeline.stages,
            args=args,
            session_dir=session_dir,
            gr_id=gr_id,
            pipeline_data=pipeline,
            repo=repo,
            state_file=state_file,
            spec_clients=_spec_clients,
            stage_specs=stage_specs,
            test_client=client,
        )
        pipe.validate_resume_target()
    except ValueError as exc:
        die(str(exc))

    _expanded_stage_names = [s.name for s in pipe.stages]
    _plan_stage_name = next((s.name for s in pipe.stages if s.type == "plan"), None)
    plan_idx = (
        _expanded_stage_names.index(_plan_stage_name)
        if _plan_stage_name and _plan_stage_name in _expanded_stage_names
        else 0
    )
    resume_idx = (
        _expanded_stage_names.index(args.resume_from)
        if args.resume_from and args.resume_from in _expanded_stage_names
        else 0
    )

    # Install signal handlers before any pre-pipeline Claude calls (e.g.
    # `_resolve_plan_source` invokes the model to generate an issue title);
    # otherwise Ctrl-C during that call leaks the child process.
    install_signal_handlers(*_signal_clients)

    if args.plan_source:
        pipe.issue_url, pipe.issue_num, pipe.issue_body = _resolve_plan_source(
            plan_source=args.plan_source,
            repo=repo,
            plan_md=plan_md,
            model=default_model,
            client=effective_client,
            state_file=state_file,
            gr_id=gr_id,
        )
    elif resume_idx > plan_idx:
        pipe.issue_url = read_state_field(state_file, "issue_url")
        if not pipe.issue_url:
            die(
                f"--resume-from {args.resume_from}: no issue_url in state.json "
                "(rewind to plan?)"
            )
        pipe.issue_num = pipe.issue_url.split("/")[-1]
        logger.info("resumed issue: %s", pipe.issue_url)
        pipe.issue_body = _fetch_issue_body(pipe.issue_num, repo)

    try:
        pipe.run(*_signal_clients)
    except ValueError as exc:
        die(str(exc))

    total_cost = 0.0
    for c in _spec_clients.values() if _spec_clients else [client] if client else []:
        total_cost += getattr(c, "total_cost_usd", 0.0) or 0.0
    if total_cost > 0:
        patch_state(gr_id, total_cost_usd=total_cost)

    logger.info("done. PR: %s", pipe.pr_url or "(unknown)")
    if total_cost > 0:
        logger.info("total cost: $%.4f", total_cost)
    return 0
