"""Orchestrator entry point for the gh pipeline.

Drives the gh pipeline: plan → implement → commit-pr → request-copilot → ghreview → wait-copilot → ghaddress.

Stage sequence (names byte-stable for --resume-from):
  plan → implement → commit-pr → request-copilot → ghreview → wait-copilot → ghaddress

Arg contract preserved from ghgremlin.sh:
  -r <ref>              git ref forwarded to /ghplan
  --plan <path|ref>     plan source (mutually exclusive with instructions)
  --model <model>       claude model
  --resume-from <stage> resume at named stage
  instructions          positional (mutually exclusive with --plan)
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import shutil
import subprocess
import sys

from ..clients.claude import ClaudeClient, SubprocessClaudeClient
from ..gh_utils import extract_gh_url, get_repo, parse_issue_ref, view_issue
from ..git import DirtyOnly, HeadAdvanced
from ..prompts import load_code_style
from ..runner import install_signal_handlers, run_stages
from ..stages.commit_pr import run_commit_pr_stage
from ..stages.ghaddress import run_ghaddress_stage
from ..stages.ghreview import run_ghreview_stage
from ..stages.implement import ImplStageResult, run_implement_stage
from ..stages.wait_copilot import run_request_copilot_stage, run_wait_copilot_stage
from ..state import patch_state, resolve_session_dir, resolve_state_file, set_stage

MODEL_RE = re.compile(r"^[A-Za-z0-9._-]+$")
REF_RE = re.compile(r"^[A-Za-z0-9._/#-]+$")

VALID_STAGES = [
    "plan",
    "implement",
    "commit-pr",
    "request-copilot",
    "ghreview",
    "wait-copilot",
    "ghaddress",
]


def die(msg: str) -> None:
    sys.stderr.write(f"error: {msg}\n")
    sys.stderr.flush()
    sys.exit(1)


def _parse_gh_args(argv: list[str]) -> argparse.Namespace:
    usage = (
        'usage: gremlins.cli gh [-r <ref>] [--resume-from <stage>] '
        '[--plan <path|issue-ref>] [--spec <path>] [--model <model>] "<instructions>"'
    )
    parser = argparse.ArgumentParser(add_help=False, usage=usage)
    parser.add_argument("-r", dest="ref", default="")
    parser.add_argument("--resume-from", dest="resume_from", default=None)
    parser.add_argument("--plan", dest="plan_source", default=None)
    parser.add_argument("--spec", dest="spec_path", default=None)
    parser.add_argument("--model", dest="model", default=None)
    parser.add_argument("instructions", nargs="*")
    args = parser.parse_args(argv)

    if args.resume_from is not None and args.resume_from not in VALID_STAGES:
        die(
            f"invalid --resume-from: {args.resume_from} "
            f"(allowed: {' '.join(VALID_STAGES)})"
        )

    if args.plan_source:
        if args.instructions:
            die("--plan and positional instructions are mutually exclusive")
    else:
        if args.resume_from is None and not args.instructions:
            die(usage)

    if args.model and not MODEL_RE.match(args.model):
        die(f"invalid model: {args.model}")
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


# Test compatibility shim — tests import _parse_issue_ref from this module
# directly (and patch _fetch_issue_body below). The canonical implementation
# lives in gremlins.gh_utils.parse_issue_ref so the boss orchestrator can
# share it.
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
    plan_md: pathlib.Path, state_file: pathlib.Path | None
) -> None:
    """Update state.json .description from the plan's first H1, if not explicitly set."""
    if state_file is None or not state_file.exists():
        return
    try:
        data = json.loads(state_file.read_text(encoding="utf-8"))
        if data.get("description_explicit"):
            return
        lines = plan_md.read_text(encoding="utf-8").splitlines()[:50]
        h1 = ""
        for line in lines:
            m = re.match(r"^#+\s+(.+)", line)
            if m:
                h1 = m.group(1)[:60]
                break
        if h1:
            patch_state(description=h1)
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
) -> tuple:
    """Resolve --plan <source> into (issue_url, issue_num, issue_body).

    On resume (plan_md already exists): reload from snapshot + state.json.
    Fresh launch: classify source shape, fetch/create issue, write plan.md.
    Returns (issue_url, issue_num, issue_body).
    """
    if plan_md.exists() and plan_md.stat().st_size > 0:
        # Resume path: reload from snapshot
        issue_url = _read_state_field(state_file, "issue_url")
        issue_num = _read_state_field(state_file, "issue_num")
        issue_body = plan_md.read_text(encoding="utf-8")
        label = f" (issue #{issue_num})" if issue_num else ""
        print(f"==> [1/6] plan resumed from snapshot: {plan_md}{label}", flush=True)
        return issue_url, issue_num, issue_body

    # Fresh launch: classify source shape
    if pathlib.Path(plan_source).is_file():
        # Local file: post as GitHub issue
        src = pathlib.Path(plan_source)
        if src.stat().st_size == 0:
            die(f"--plan: file is empty: {plan_source}")
        issue_body = src.read_text(encoding="utf-8")
        print(
            f"==> [1/6] plan supplied via --plan (file): {plan_source}"
            " — posting as GitHub issue",
            flush=True,
        )

        # Generate issue title via claude
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
            ["gh", "issue", "create", "--repo", repo, "--title", issue_title, "--body-file", plan_source],
            capture_output=True, text=True, check=False,
        )
        if r.returncode != 0:
            die(f"--plan: failed to create GitHub issue: {r.stderr.strip()}")
        create_out = r.stdout + r.stderr
        m = re.search(r"https://github\.com/[^ )]+/issues/[0-9]+", create_out)
        if not m:
            die(f"--plan: could not extract issue URL from gh output: {create_out.strip()}")
        issue_url = m.group(0)
        issue_num = issue_url.split("/")[-1]

        patch_state(issue_url=issue_url, issue_num=issue_num)
        print(f"    issue: {issue_url}", flush=True)
        shutil.copyfile(src, plan_md)
    else:
        # Issue reference
        target_repo, issue_ref = _parse_issue_ref(plan_source, repo)
        if target_repo is None:
            die(f"--plan: not a readable file or recognized issue reference: {plan_source}")

        try:
            issue_data = view_issue(issue_ref, target_repo)
        except RuntimeError as exc:
            die(f"--plan: {exc}")
        issue_body = issue_data.get("body") or ""
        if not issue_body:
            die(f"--plan: issue {plan_source} has an empty body")

        resolved_url = issue_data.get("url") or ""
        resolved_num = str(issue_data.get("number") or "")

        # Only set issue_url/issue_num (which drive the `Closes #N` link) when
        # the resolved issue's repo matches the PR's target repo.
        if target_repo == repo:
            issue_url = resolved_url
            issue_num = resolved_num
        else:
            issue_url = ""
            issue_num = ""

        # Use issue_body + newline to match the `cp` semantics of the file path above.
        plan_md.write_text(issue_body + "\n", encoding="utf-8")
        print(f"==> [1/6] plan supplied via --plan (issue {target_repo}#{issue_ref})", flush=True)

    patch_state(issue_url=issue_url, issue_num=issue_num)
    _update_description_from_plan(plan_md, state_file)
    return issue_url, issue_num, issue_body


def gh_main(argv: list[str], *, client: ClaudeClient | None = None) -> int:
    args = _parse_gh_args(argv)

    if shutil.which("claude") is None:
        die("claude CLI not found")
    if shutil.which("gh") is None:
        die("gh CLI not found")

    if client is None:
        client = SubprocessClaudeClient()
    install_signal_handlers(client)

    repo = get_repo()
    session_dir = resolve_session_dir()
    state_file = resolve_state_file()
    plan_md = session_dir / "plan.md"
    spec_file = session_dir / "spec.md"

    print(f"==> session: {session_dir}", flush=True)

    # --spec staging: snapshot into session_dir/spec.md on first launch.
    # On resume, reuse the existing snapshot (rescue-determinism).
    # launcher.py normalizes spec_path before spawning the subprocess, so the
    # is_file / size checks below are only reachable on a direct (non-launcher)
    # invocation of the orchestrator — they guard that path.
    if args.spec_path and not spec_file.exists():
        spec_src = pathlib.Path(args.spec_path)
        if not spec_src.is_file():
            die(f"--spec: file not found: {args.spec_path}")
        if spec_src.stat().st_size == 0:
            die(f"--spec: file is empty: {args.spec_path}")
        shutil.copyfile(spec_src, spec_file)

    # Restore model from state.json when --model not supplied (resume path),
    # then fall back to "sonnet" for fresh launches without an explicit --model.
    # Argparse keeps default=None so the resume-restore step can detect "user
    # didn't pass --model" and prefer the persisted value over the fresh default.
    model = args.model
    if model is None:
        model = _read_state_field(state_file, "model") or "sonnet"
    if model:
        patch_state(model=model)

    instructions = " ".join(args.instructions) if args.instructions else ""

    # Inter-stage state (populated before the loop or by stage callables).
    issue_url: str = ""
    issue_num: str = ""
    issue_body: str = ""

    plan_stage_idx = VALID_STAGES.index("plan")
    resume_idx = VALID_STAGES.index(args.resume_from) if args.resume_from else 0

    if args.plan_source:
        # Resolve plan source before the loop (handles both fresh and resume).
        issue_url, issue_num, issue_body = _resolve_plan_source(
            plan_source=args.plan_source,
            repo=repo,
            plan_md=plan_md,
            model=model,
            client=client,
            state_file=state_file,
        )
    elif resume_idx > plan_stage_idx:
        # Resuming past plan without --plan: reload issue from state.json.
        issue_url = _read_state_field(state_file, "issue_url")
        if not issue_url:
            die(
                f"--resume-from {args.resume_from}: no issue_url in state.json "
                "(rewind to plan?)"
            )
        issue_num = issue_url.split("/")[-1]
        print(f"    resumed issue: {issue_url}", flush=True)
        issue_body = _fetch_issue_body(issue_num, repo)

    code_style = load_code_style()

    # pr_url populated by stage_commit_pr; later stages read it here.
    pr_url_holder: dict[str, str] = {}
    # impl_result populated by stage_implement; stage_commit_pr reads it.
    impl_result_holder: dict[str, object] = {}

    def _ensure_pr_url() -> None:
        """Populate pr_url_holder from state.json when resuming past commit-pr."""
        if pr_url_holder.get("url"):
            return
        saved = _read_state_field(state_file, "pr_url")
        if not saved:
            die(
                f"--resume-from {args.resume_from}: no pr_url in state.json "
                "(rewind to implement?)"
            )
        pr_url_holder["url"] = saved
        pr_url_holder["num"] = saved.split("/")[-1]
        print(f"    resumed PR: {saved}", flush=True)

    def stage_plan() -> None:
        nonlocal issue_url, issue_num, issue_body
        if args.plan_source:
            # Already resolved before the loop.
            return
        set_stage("plan")
        print("==> [1/6] running /ghplan", flush=True)
        plan_prompt = f"/ghplan {args.ref + ' ' if args.ref else ''}{instructions}"
        completed = client.run(
            plan_prompt,
            label="plan",
            model=model,
            raw_path=session_dir / "ghplan-out.jsonl",
            capture_events=True,
        )
        events = completed.events or []
        issue_url = extract_gh_url(
            events,
            url_pattern=r"https://github\.com/[^ )]+/issues/[0-9]+",
            cmd_pattern=r"gh issue create",
            label="issue",
        )
        issue_num = issue_url.split("/")[-1]
        print(f"    issue: {issue_url}", flush=True)
        patch_state(issue_url=issue_url, issue_num=issue_num)
        issue_body = _fetch_issue_body(issue_num, repo)

    def stage_implement() -> None:
        set_stage("implement")
        print("==> [2a/6] implementing plan", flush=True)
        spec_text = ""
        if spec_file.exists():
            try:
                spec_text = spec_file.read_text(encoding="utf-8")
            except (OSError, UnicodeDecodeError) as exc:
                print(f"warning: could not read spec.md ({exc}); proceeding without north-star context", flush=True, file=sys.stderr)
        result = run_implement_stage(
            client=client,
            impl_model=model,
            plan_text=issue_body,
            code_style=code_style,
            session_dir=session_dir,
            is_git=True,
            kind="gh",
            issue_num=issue_num,
            spec_text=spec_text,
        )
        impl_result_holder["result"] = result
        # Persist for commit-pr resume: base_ref and handoff branch are all
        # that's needed to reconstruct the diff and outcome on a fresh process.
        patch_state(
            impl_handoff_branch=result.handoff_branch,
            impl_base_ref=result.pre_state.head,
        )

    def stage_commit_pr() -> None:
        set_stage("commit-pr")
        print("==> [2b/6] committing + opening PR", flush=True)

        if "result" in impl_result_holder:
            result: ImplStageResult = impl_result_holder["result"]  # type: ignore[assignment]
            impl_outcome = result.outcome
            impl_handoff_branch = result.handoff_branch
            base_ref = result.pre_state.head
        else:
            # Resuming at commit-pr: reconstruct from state.json
            impl_handoff_branch = _read_state_field(state_file, "impl_handoff_branch")
            base_ref = _read_state_field(state_file, "impl_base_ref")
            if not base_ref:
                die(
                    "--resume-from commit-pr: no impl_base_ref in state.json "
                    "(rewind to implement?)"
                )
            if impl_handoff_branch:
                count_r = subprocess.run(
                    ["git", "rev-list", "--count", f"{base_ref}..{impl_handoff_branch}"],
                    capture_output=True, text=True, check=False,
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

        pr_url = run_commit_pr_stage(
            client=client,
            model=model,
            impl_outcome=impl_outcome,
            impl_handoff_branch=impl_handoff_branch,
            base_ref=base_ref,
            issue_url=issue_url,
            cwd=None,
            session_dir=session_dir,
        )
        pr_url_holder["url"] = pr_url
        pr_url_holder["num"] = pr_url.split("/")[-1]
        print(f"    PR: {pr_url}", flush=True)
        patch_state(pr_url=pr_url)

    def stage_request_copilot() -> None:
        _ensure_pr_url()
        set_stage("request-copilot")
        print("==> [3/6] requesting Copilot review", flush=True)
        run_request_copilot_stage(repo=repo, pr_num=pr_url_holder["num"])

    def stage_ghreview() -> None:
        _ensure_pr_url()
        set_stage("ghreview")
        print("==> [4/6] running /ghreview", flush=True)
        run_ghreview_stage(
            client=client,
            model=model,
            pr_url=pr_url_holder["url"],
            artifacts_dir=session_dir,
            code_style=code_style,
        )

    def stage_wait_copilot() -> None:
        _ensure_pr_url()
        set_stage("wait-copilot")
        print("==> [5/6] waiting for Copilot review (20s interval, 10min timeout)", flush=True)
        state = run_wait_copilot_stage(
            repo=repo,
            pr_num=pr_url_holder["num"],
        )
        print(f"    Copilot review: {state}", flush=True)

    def stage_ghaddress() -> None:
        _ensure_pr_url()
        set_stage("ghaddress")
        print("==> [6/6] running /ghaddress", flush=True)
        run_ghaddress_stage(
            client=client,
            model=model,
            pr_url=pr_url_holder["url"],
            artifacts_dir=session_dir,
            code_style=code_style,
        )

    stages = [
        ("plan", stage_plan),
        ("implement", stage_implement),
        ("commit-pr", stage_commit_pr),
        ("request-copilot", stage_request_copilot),
        ("ghreview", stage_ghreview),
        ("wait-copilot", stage_wait_copilot),
        ("ghaddress", stage_ghaddress),
    ]
    run_stages(stages, resume_from=args.resume_from)

    total_cost = getattr(client, "total_cost_usd", 0.0)
    if total_cost is not None and total_cost > 0:
        patch_state(total_cost_usd=total_cost)

    print("", flush=True)
    print(f"done. PR: {pr_url_holder.get('url', '(unknown)')}", flush=True)
    if total_cost is not None and total_cost > 0:
        print(f"total cost: ${total_cost:.4f}", flush=True)
    return 0
