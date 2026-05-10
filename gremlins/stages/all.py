"""Import all stage modules so each self-registers into the stage registry."""

from __future__ import annotations

import logging
import pathlib
from typing import Any

import gremlins.stages.address_code as address_code
import gremlins.stages.claude_prompt as claude_prompt
import gremlins.stages.commit as commit
import gremlins.stages.commit_pr as commit_pr
import gremlins.stages.handoff as handoff_stage_mod
import gremlins.stages.implement as implement
import gremlins.stages.loop as loop
import gremlins.stages.materialize_to_branch as materialize_to_branch_mod
import gremlins.stages.open_github_pr as open_github_pr
import gremlins.stages.plan as plan
import gremlins.stages.request_copilot as request_copilot
import gremlins.stages.review_code as review_code
import gremlins.stages.run_cmd as run_cmd
import gremlins.stages.sequence as sequence
import gremlins.stages.verify as verify
import gremlins.stages.wait_ci as wait_ci
import gremlins.stages.wait_copilot as wait_copilot
from gremlins.clients.client import Client
from gremlins.errors import die
from gremlins.schema import StageEntry
from gremlins.stage_clients import require_stage_spec
from gremlins.stages.base import StageRunner
from gremlins.stages.registry import register_stage_builder
from gremlins.state import read_state_str

logger = logging.getLogger(__name__)


def _review_stage_info(
    runner: StageRunner,
) -> tuple[list[str], dict[str, pathlib.Path]]:
    names: list[str] = []
    dirs: dict[str, pathlib.Path] = {}
    scope = runner.current_scope or list(runner.pipeline_data.stages)
    for s in scope:
        if s.type == "parallel":
            for child in s.body:
                if child.type == "review-code":
                    names.append(child.name)
                    dirs[child.name] = runner.session_dir / s.name / child.name
        elif s.type == "review-code":
            names.append(s.name)
            dirs[s.name] = runner.session_dir
    return names, dirs


def _build_plan(entry: StageEntry, spec: Client, runner: StageRunner) -> Any:
    plan_val = getattr(runner.args, "plan", None)
    if not entry.prompts and not plan_val:
        die(
            f"stage {entry.name!r}: type 'plan' requires a 'prompt' field in the pipeline YAML"
        )
    if not runner.repo:
        logger.info(
            "planning (model: %s) -> %s", spec.model, runner.session_dir / "plan.md"
        )
    return plan.Plan(
        entry.name,
        spec.model,
        entry.prompts,
        entry.options,
        plan=plan_val,
        instructions=runner.instructions,
        repo=runner.repo,
    )


def _build_implement(entry: StageEntry, spec: Client, runner: StageRunner) -> Any:
    spec_text = ""
    spec_file = runner.session_dir / "spec.md"
    if spec_file.exists():
        try:
            spec_text = spec_file.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError) as exc:
            logger.warning(
                "could not read spec.md (%s); proceeding without north-star context",
                exc,
            )
    if not runner.repo:
        logger.info(
            "implementing (model: %s, from %s)",
            spec.model,
            runner.session_dir / "plan.md",
        )
    return implement.Implement(
        entry.name,
        spec.model,
        entry.prompts,
        entry.options,
        is_git=runner.is_git,
        spec_text=spec_text,
    )


def _build_materialize_to_branch(
    entry: StageEntry, spec: Client, _runner: StageRunner
) -> Any:
    return materialize_to_branch_mod.MaterializeToBranch(
        entry.name, spec.model, entry.prompts, entry.options
    )


def _build_verify(entry: StageEntry, spec: Client, runner: StageRunner) -> Any:
    options = dict(entry.options)
    if not runner.repo:
        cmds = getattr(runner.args, "cmds", None)
        if cmds is not None:
            options["cmds"] = cmds
        options.setdefault("max_attempts", getattr(runner.args, "test_max_attempts", 3))
        resolved_cmds = options.get("cmds", [])
        if resolved_cmds:
            logger.info(
                "running verify (cmds: %r, max-attempts: %s, model: %s)",
                resolved_cmds,
                options.get("max_attempts"),
                spec.model,
            )
    return verify.Verify(
        entry.name, spec.model, entry.prompts, options, is_git=runner.is_git
    )


def _build_commit(entry: StageEntry, spec: Client, _runner: StageRunner) -> Any:
    return commit.Commit(entry.name, spec.model, entry.prompts, entry.options)


def _build_open_github_pr(
    entry: StageEntry, spec: Client, runner: StageRunner
) -> Any:
    return open_github_pr.OpenGitHubPR(
        entry.name,
        spec.model,
        entry.prompts,
        entry.options,
        issue_url=read_state_str(runner.state_file, "issue_url"),
        base_ref=(entry.options.get("base_ref") or "").strip() or None,
    )


def _build_request_copilot(
    entry: StageEntry, spec: Client, runner: StageRunner
) -> Any:
    return request_copilot.RequestCopilot(
        entry.name, spec.model, entry.prompts, entry.options, repo=runner.repo
    )


def _build_ghreview(entry: StageEntry, spec: Client, _runner: StageRunner) -> Any:
    if not entry.prompts:
        die(
            f"stage {entry.name!r}: type 'ghreview' requires a 'prompt' field in the pipeline YAML"
        )
    return review_code.ReviewCode(
        entry.name, spec.model, entry.prompts, entry.options, plan_text="", is_git=True
    )


def _build_wait_copilot(
    entry: StageEntry, spec: Client, runner: StageRunner
) -> Any:
    return wait_copilot.WaitCopilot(
        entry.name, spec.model, entry.prompts, entry.options, repo=runner.repo
    )


def _build_ghaddress(entry: StageEntry, spec: Client, _runner: StageRunner) -> Any:
    if not entry.prompts:
        die(
            f"stage {entry.name!r}: type 'ghaddress' requires a 'prompt' field in the pipeline YAML"
        )
    return address_code.AddressCode(
        entry.name, spec.model, entry.prompts, entry.options, is_git=True
    )


def _build_wait_ci(entry: StageEntry, spec: Client, _runner: StageRunner) -> Any:
    return wait_ci.WaitCI(entry.name, spec.model, entry.prompts, entry.options)


def _build_review_code(entry: StageEntry, spec: Client, runner: StageRunner) -> Any:
    plan_file = runner.session_dir / "plan.md"
    plan_text = plan_file.read_text(encoding="utf-8")
    logger.info("reviewing code (model: %s)", spec.model)
    return review_code.ReviewCode(
        entry.name,
        spec.model,
        entry.prompts,
        entry.options,
        plan_text=plan_text,
        is_git=runner.is_git,
    )


def _build_address_code(
    entry: StageEntry, spec: Client, runner: StageRunner
) -> Any:
    names, dirs = _review_stage_info(runner)
    logger.info("addressing code reviews (model: %s)", spec.model)
    return address_code.AddressCode(
        entry.name,
        spec.model,
        entry.prompts,
        entry.options,
        is_git=runner.is_git,
        review_stage_names=names,
        review_stage_dirs=dirs,
    )


def _build_loop(entry: StageEntry, spec: Client, runner: StageRunner) -> Any:
    from gremlins.stages.base import StageContext
    from gremlins.stages.loop import LoopStage

    max_iterations = entry.options.get("max_iterations", 3)
    body_runners: list[Any] = []
    for child in entry.body:
        child_spec = runner.stage_specs.get(child.name, spec)
        child_ctx = StageContext(
            client=runner.get_client(child_spec),
            session_dir=runner.session_dir,
            gr_id=runner.gr_id,
        )
        body_runners.append(
            runner.make_runner(child, child_ctx, child_spec, scope=entry.body)
        )
    return LoopStage(
        entry.name, body_runners=body_runners, max_iterations=max_iterations
    )


def _build_sequence(entry: StageEntry, spec: Client, runner: StageRunner) -> Any:
    from gremlins.stages.base import StageContext
    from gremlins.stages.sequence import SequenceStage

    body: list[Any] = []
    for child in entry.body:
        child_spec = runner.stage_specs.get(child.name, spec)
        child_ctx = StageContext(
            client=runner.get_client(child_spec),
            session_dir=runner.session_dir,
            gr_id=runner.gr_id,
        )
        child_runner = runner.make_runner(
            child, child_ctx, child_spec, scope=entry.body
        )
        body.append((child_ctx, child_runner))
    return SequenceStage(entry.name, body=body)


def _build_run_cmd(entry: StageEntry, spec: Client, _runner: StageRunner) -> Any:
    return run_cmd.RunCmd(entry.name, spec.model, entry.prompts, entry.options)


def _build_claude_prompt(
    entry: StageEntry, spec: Client, _runner: StageRunner
) -> Any:
    if not entry.prompts:
        die(f"stage {entry.name!r}: type 'claude-prompt' requires a 'prompt' field")
    return claude_prompt.ClaudePrompt(
        entry.name, spec.model, entry.prompts, entry.options
    )


def _build_handoff(entry: StageEntry, spec: Client, _runner: StageRunner) -> Any:
    from gremlins.stages.handoff import Handoff

    return Handoff(entry.name, spec)


def _build_parallel(entry: StageEntry, spec: Client, runner: StageRunner) -> Any:
    from gremlins.stages.base import StageContext
    from gremlins.stages.parallel import ParallelStage
    from gremlins.state import set_stage

    group_dir = runner.session_dir / entry.name
    group_dir.mkdir(parents=True, exist_ok=True)
    child_runners: list[tuple[str, Any, Any]] = []
    for child in entry.body:
        child_spec = require_stage_spec(runner.stage_specs, child.name)
        child_dir = group_dir / child.name
        child_dir.mkdir(parents=True, exist_ok=True)
        child_ctx = StageContext(
            client=runner.get_client(child_spec),
            session_dir=child_dir,
            gr_id=runner.gr_id,
            child_key=child.name,
        )
        child_runners.append(
            (
                child.name,
                child_ctx,
                runner.make_runner(child, child_ctx, child_spec, scope=entry.body),
            )
        )
    gr_id = runner.gr_id
    return ParallelStage(
        entry.name,
        child_runners,
        max_concurrent=entry.max_concurrent,
        cancel_on_bail=entry.cancel_on_bail,
        bail_policy=entry.bail_policy,
        gr_id=gr_id,
        project_root=pathlib.Path.cwd(),
        set_stage_fn=lambda n: set_stage(gr_id, n),
    )


register_stage_builder("plan", _build_plan, needs_pipe=False)
register_stage_builder("implement", _build_implement, needs_pipe=True)
register_stage_builder(
    "materialize-to-branch", _build_materialize_to_branch, needs_pipe=True
)
register_stage_builder("verify", _build_verify, needs_pipe=False)
register_stage_builder("commit", _build_commit, needs_pipe=False)
register_stage_builder("open-github-pr", _build_open_github_pr, needs_pipe=False)
register_stage_builder("request-copilot", _build_request_copilot, needs_pipe=False)
register_stage_builder("ghreview", _build_ghreview, needs_pipe=True)
register_stage_builder("wait-copilot", _build_wait_copilot, needs_pipe=False)
register_stage_builder("ghaddress", _build_ghaddress, needs_pipe=True)
register_stage_builder("wait-ci", _build_wait_ci, needs_pipe=False)
register_stage_builder("review-code", _build_review_code, needs_pipe=False)
register_stage_builder("address-code", _build_address_code, needs_pipe=False)
register_stage_builder("loop", _build_loop, needs_pipe=True)
register_stage_builder("sequence", _build_sequence, needs_pipe=True)
register_stage_builder("run-cmd", _build_run_cmd, needs_pipe=False)
register_stage_builder("claude-prompt", _build_claude_prompt, needs_pipe=False)
register_stage_builder("handoff", _build_handoff, needs_pipe=False)
register_stage_builder("parallel", _build_parallel, needs_pipe=True)


__all__ = [
    "address_code",
    "claude_prompt",
    "commit",
    "commit_pr",
    "handoff_stage_mod",
    "implement",
    "loop",
    "open_github_pr",
    "plan",
    "request_copilot",
    "review_code",
    "run_cmd",
    "sequence",
    "verify",
    "wait_ci",
    "wait_copilot",
]
