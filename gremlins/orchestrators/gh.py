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
from collections.abc import Callable
from typing import Any, NoReturn

import yaml

from gremlins.clients import ClientSpec, to_client
from gremlins.clients.protocol import ClaudeClient
from gremlins.clients.resolve import (
    PACKAGE_DEFAULT,
    collect_stage_specs,
    load_stage_specs_from_state,
    require_stage_spec,
    validate_stage_specs,
)
from gremlins.env_file import load_env_file
from gremlins.gh_utils import get_repo, parse_issue_ref, view_issue
from gremlins.git import DirtyOnly, HeadAdvanced
from gremlins.logging_setup import configure_logging
from gremlins.orchestrators.pipeline import GHPipeline
from gremlins.pipeline import (
    StageEntry,
    load_pipeline,
    resolve_pipeline_path,
)
from gremlins.runner import build_parallel_stages, run_stages
from gremlins.stages import (
    commit_pr,
    ghaddress,
    ghplan,
    ghreview,
    implement,
    request_copilot,
    verify,
    wait_ci,
    wait_copilot,
)
from gremlins.stages.context import StageContext
from gremlins.stages.implement import ImplStageResult
from gremlins.state import (
    patch_state,
    resolve_session_dir,
    resolve_state_file,
    set_stage,
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


def _read_state_field(sf: pathlib.Path | None, field: str) -> str:
    if sf is None or not sf.exists():
        return ""
    try:
        data = json.loads(sf.read_text(encoding="utf-8"))
        return data.get(field) or ""
    except Exception:
        return ""


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
        issue_url = _read_state_field(state_file, "issue_url")
        issue_num = _read_state_field(state_file, "issue_num")
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


def _ensure_pr_url(
    gh_state: dict[str, Any],
    state_file: pathlib.Path | None,
    resume_from: str | None,
) -> None:
    if gh_state["pr_url"]:
        return
    saved = _read_state_field(state_file, "pr_url")
    if not saved:
        die(
            f"--resume-from {resume_from}: no pr_url in state.json "
            "(rewind to implement?)"
        )
    gh_state["pr_url"] = saved
    gh_state["pr_num"] = saved.split("/")[-1]
    logger.info("resumed PR: %s", saved)


def _build_stage_runner(
    entry: StageEntry,
    ctx: StageContext,
    model: str,
    *,
    args: argparse.Namespace,
    repo: str,
    session_dir: pathlib.Path,
    state_file: pathlib.Path | None,
    gr_id: str | None,
    gh_state: dict[str, Any],
) -> Callable[[], None]:
    if entry.type == "plan":

        def _plan() -> None:
            if args.plan_source:
                return
            if not entry.prompt_paths:
                die(
                    f"stage {entry.name!r}: type 'plan' requires a 'prompt' field in the pipeline YAML"
                )
            set_stage(gr_id, entry.name)
            logger.info("[1/8] running ghplan")
            stage = ghplan.GHPlan(
                entry,
                model,
                ref=args.ref or "",
                instructions=gh_state["instructions"],
                repo=repo,
            )
            stage.bind(ctx)
            result = stage.run(None)
            gh_state["issue_url"] = result.issue_url
            gh_state["issue_num"] = result.issue_num
            gh_state["issue_body"] = result.issue_body

        return _plan

    if entry.type == "implement":

        def _implement() -> None:
            set_stage(gr_id, entry.name)
            logger.info("[2a/8] implementing plan")
            spec_file = session_dir / "spec.md"
            spec_text = ""
            if spec_file.exists():
                try:
                    spec_text = spec_file.read_text(encoding="utf-8")
                except (OSError, UnicodeDecodeError) as exc:
                    logger.warning(
                        "could not read spec.md (%s); proceeding without north-star context",
                        exc,
                    )
            stage = implement.Implement(
                entry,
                model,
                plan_text=gh_state["issue_body"],
                is_git=True,
                kind="gh",
                issue_num=gh_state["issue_num"],
                spec_text=spec_text,
            )
            stage.bind(ctx)
            impl_result = stage.run(None)
            if impl_result is None:
                die("implement stage did not produce a result")
            gh_state["impl_result"] = impl_result
            patch_state(
                gr_id,
                impl_handoff_branch=impl_result.handoff_branch,
                impl_base_ref=impl_result.pre_state.head,
            )

        return _implement

    if entry.type == "verify":

        def _verify() -> None:
            set_stage(gr_id, entry.name)
            logger.info("[2b/8] verifying implementation")
            stage = verify.Verify(entry, model, is_git=True, commit_after_fix=False)
            stage.bind(ctx)
            stage.run(None)

        return _verify

    if entry.type == "commit-pr":

        def _commit_pr() -> None:
            set_stage(gr_id, entry.name)
            logger.info("[2c/8] committing + opening PR")
            impl_result: ImplStageResult | None = gh_state["impl_result"]
            if impl_result is not None:
                impl_outcome = impl_result.outcome
                impl_handoff_branch = impl_result.handoff_branch
                base_ref = impl_result.pre_state.head
            else:
                impl_handoff_branch = _read_state_field(
                    state_file, "impl_handoff_branch"
                )
                base_ref = _read_state_field(state_file, "impl_base_ref")
                if not base_ref:
                    die(
                        "--resume-from commit-pr: no impl_base_ref in state.json "
                        "(rewind to implement?)"
                    )
                if impl_handoff_branch:
                    count_r = subprocess.run(
                        [
                            "git",
                            "rev-list",
                            "--count",
                            f"{base_ref}..{impl_handoff_branch}",
                        ],
                        capture_output=True,
                        text=True,
                        check=False,
                    )
                    if count_r.returncode != 0:
                        die(
                            f"--resume-from commit-pr: impl_handoff_branch '{impl_handoff_branch}' "
                            f"not found or base_ref invalid (rewind to implement?)\n"
                            f"{count_r.stderr.strip()}"
                        )
                    commit_count = int(count_r.stdout.strip())
                    impl_outcome = HeadAdvanced(commit_count=commit_count)
                else:
                    impl_outcome = DirtyOnly()

            stage = commit_pr.CommitPR(
                entry,
                model,
                impl_outcome=impl_outcome,
                impl_handoff_branch=impl_handoff_branch,
                base_ref=base_ref,
                issue_url=gh_state["issue_url"],
                cwd=None,
            )
            stage.bind(ctx)
            pr_url = stage.run(None)
            pr_num = pr_url.split("/")[-1]
            logger.info("PR: %s", pr_url)
            patch_state(gr_id, pr_url=pr_url)
            gh_state["pr_url"] = pr_url
            gh_state["pr_num"] = pr_num

        return _commit_pr

    if entry.type == "request-copilot":

        def _request_copilot() -> None:
            _ensure_pr_url(gh_state, state_file, args.resume_from)
            set_stage(gr_id, entry.name)
            logger.info("[3/8] requesting Copilot review")
            stage = request_copilot.RequestCopilot(
                entry, model, repo=repo, pr_num=gh_state["pr_num"]
            )
            stage.bind(ctx)
            stage.run(None)

        return _request_copilot

    if entry.type == "ghreview":

        def _ghreview() -> None:
            if not entry.prompt_paths:
                die(
                    f"stage {entry.name!r}: type 'ghreview' requires a 'prompt' field in the pipeline YAML"
                )
            _ensure_pr_url(gh_state, state_file, args.resume_from)
            set_stage(gr_id, entry.name)
            logger.info("[4/8] running /ghreview")
            stage = ghreview.GHReview(entry, model, pr_url=gh_state["pr_url"])
            stage.bind(ctx)
            stage.run(None)

        return _ghreview

    if entry.type == "wait-copilot":

        def _wait_copilot() -> None:
            _ensure_pr_url(gh_state, state_file, args.resume_from)
            set_stage(gr_id, entry.name)
            logger.info(
                "[5/8] waiting for Copilot review (20s interval, 10min timeout)"
            )
            stage = wait_copilot.WaitCopilot(
                entry, model, repo=repo, pr_num=gh_state["pr_num"]
            )
            stage.bind(ctx)
            state = stage.run(None)
            logger.info("Copilot review: %s", state)

        return _wait_copilot

    if entry.type == "ghaddress":

        def _ghaddress() -> None:
            if not entry.prompt_paths:
                die(
                    f"stage {entry.name!r}: type 'ghaddress' requires a 'prompt' field in the pipeline YAML"
                )
            _ensure_pr_url(gh_state, state_file, args.resume_from)
            set_stage(gr_id, entry.name)
            logger.info("[6/8] running /ghaddress")
            stage = ghaddress.GHAddress(entry, model, pr_url=gh_state["pr_url"])
            stage.bind(ctx)
            stage.run(None)

        return _ghaddress

    if entry.type == "wait-ci":

        def _wait_ci() -> None:
            _ensure_pr_url(gh_state, state_file, args.resume_from)
            set_stage(gr_id, entry.name)
            logger.info("[7/8] waiting for CI checks (up to 3 attempts, 20min each)")
            stage = wait_ci.WaitCI(entry, model, pr_url=gh_state["pr_url"])
            stage.bind(ctx)
            stage.run(None)

        return _wait_ci

    raise ValueError(f"unsupported stage type {entry.type!r} in gh pipeline")


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
    if client is not None:
        effective_client = client
    else:
        effective_client = _client_for_spec(default_spec)
    default_model = default_spec.model

    session_dir = resolve_session_dir(gr_id)

    if client is not None:
        _signal_clients = [client]
    elif _spec_clients:
        _signal_clients = list(_spec_clients.values())
    else:
        _signal_clients = [effective_client]
    try:
        pipe = GHPipeline(pipeline.stages, args=args, session_dir=session_dir, gr_id=gr_id)
        pipe.validate_resume_target()
        pipe.run(*_signal_clients)
    except ValueError as exc:
        die(str(exc))

    stage_names = [s.name for s in pipeline.stages]

    # Expand parallel groups to their three runtime stages: fanout, parallel, fanin.
    # Child names are not valid resume targets — resuming a parallel block always
    # restarts at one of the three group-level stages so prior shards/worktrees
    # don't bleed across runs.
    _expanded_stage_names: list[str] = []
    _child_names: set[str] = set()
    for _e in pipeline.stages:
        if _e.type == "parallel":
            _expanded_stage_names.extend(
                [f"{_e.name}-fanout", _e.name, f"{_e.name}-fanin"]
            )
            for _child in _e.children:
                if _child.name in _child_names or _child.name in stage_names:
                    die(f"duplicate child stage name {_child.name!r}")
                _child_names.add(_child.name)
        else:
            _expanded_stage_names.append(_e.name)

    seen: set[str] = set()
    for _n in _expanded_stage_names:
        if _n in seen:
            die(f"pipeline has duplicate stage name {_n!r}")
        seen.add(_n)

    run_resume_from = args.resume_from

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

    instructions = " ".join(args.instructions) if args.instructions else ""

    issue_url: str = ""
    issue_num: str = ""
    issue_body: str = ""

    # plan_idx: index of the plan stage in the expanded stage list.
    _plan_stage_name = next((s.name for s in pipeline.stages if s.type == "plan"), None)
    plan_idx = (
        _expanded_stage_names.index(_plan_stage_name)
        if _plan_stage_name and _plan_stage_name in _expanded_stage_names
        else 0
    )
    resume_idx = (
        _expanded_stage_names.index(run_resume_from)
        if run_resume_from and run_resume_from in _expanded_stage_names
        else 0
    )

    if args.plan_source:
        issue_url, issue_num, issue_body = _resolve_plan_source(
            plan_source=args.plan_source,
            repo=repo,
            plan_md=plan_md,
            model=default_model,
            client=effective_client,
            state_file=state_file,
            gr_id=gr_id,
        )
    elif resume_idx > plan_idx:
        issue_url = _read_state_field(state_file, "issue_url")
        if not issue_url:
            die(
                f"--resume-from {args.resume_from}: no issue_url in state.json "
                "(rewind to plan?)"
            )
        issue_num = issue_url.split("/")[-1]
        logger.info("resumed issue: %s", issue_url)
        issue_body = _fetch_issue_body(issue_num, repo)

    gh_state: dict[str, Any] = {
        "issue_url": issue_url,
        "issue_num": issue_num,
        "issue_body": issue_body,
        "impl_result": None,
        "pr_url": "",
        "pr_num": "",
        "instructions": instructions,
    }

    stages: list[tuple[str, Callable[[], None]]] = []
    for e in pipeline.stages:
        if e.type == "parallel":
            group_dir = session_dir / e.name
            group_dir.mkdir(parents=True, exist_ok=True)
            child_runners: list[tuple[str, StageContext, Callable[[], None]]] = []
            for child in e.children:
                child_spec = require_stage_spec(stage_specs, child.name)
                child_dir = group_dir / child.name
                child_dir.mkdir(parents=True, exist_ok=True)
                child_ctx = StageContext(
                    client=_client_for_spec(child_spec),
                    session_dir=child_dir,
                    gr_id=gr_id,
                    child_key=child.name,
                )
                try:
                    child_runner = _build_stage_runner(
                        child,
                        child_ctx,
                        child_spec.model,
                        args=args,
                        repo=repo,
                        session_dir=child_dir,
                        state_file=state_file,
                        gr_id=gr_id,
                        gh_state=gh_state,
                    )
                except ValueError as exc:
                    die(str(exc))
                child_runners.append((child.name, child_ctx, child_runner))
            group_name = e.name
            stages.extend(
                build_parallel_stages(
                    group_name,
                    child_runners,
                    max_concurrent=e.max_concurrent,
                    set_stage_fn=lambda n: set_stage(gr_id, n),
                    cancel_on_bail=e.cancel_on_bail,
                    bail_policy=e.bail_policy,
                    gr_id=gr_id,
                    project_root=pathlib.Path.cwd(),
                )
            )
        else:
            stage_spec = require_stage_spec(stage_specs, e.name)
            stage_ctx = StageContext(
                client=_client_for_spec(stage_spec),
                session_dir=session_dir,
                gr_id=gr_id,
            )
            try:
                runner = _build_stage_runner(
                    e,
                    stage_ctx,
                    stage_spec.model,
                    args=args,
                    repo=repo,
                    session_dir=session_dir,
                    state_file=state_file,
                    gr_id=gr_id,
                    gh_state=gh_state,
                )
            except ValueError as exc:
                die(str(exc))
            stages.append((e.name, runner))
    run_stages(stages, resume_from=run_resume_from)

    total_cost = 0.0
    for c in _spec_clients.values() if _spec_clients else [client] if client else []:
        total_cost += getattr(c, "total_cost_usd", 0.0) or 0.0
    if total_cost > 0:
        patch_state(gr_id, total_cost_usd=total_cost)

    logger.info("done. PR: %s", gh_state["pr_url"] or "(unknown)")
    if total_cost > 0:
        logger.info("total cost: $%.4f", total_cost)
    return 0
