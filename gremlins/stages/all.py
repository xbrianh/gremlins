"""Import all stage modules so each self-registers into the stage registry."""

from __future__ import annotations

import logging
import pathlib
from typing import TYPE_CHECKING, Any

from gremlins.clients import ClientSpec
from gremlins.errors import die
from gremlins.pipeline import StageEntry
from gremlins.stages import (
    address_code,
    chain,
    commit,
    commit_pr,
    implement,
    open_github_pr,
    plan,
    request_copilot,
    review_code,
    verify,
    wait_ci,
    wait_copilot,
)
from gremlins.stages import materialize_to_branch as materialize_to_branch_mod
from gremlins.clients.resolve import require_stage_spec
from gremlins.stages.registry import register_stage_builder
from gremlins.state import read_state_str

if TYPE_CHECKING:
    from gremlins.orchestrators.pipeline import StageRunner

logger = logging.getLogger(__name__)


def _review_stage_info(
    runner: StageRunner,
) -> tuple[list[str], dict[str, pathlib.Path]]:
    names: list[str] = []
    dirs: dict[str, pathlib.Path] = {}
    for s in runner.pipeline_data.stages:
        if s.type == "parallel":
            for child in s.body:
                if child.type == "review-code":
                    names.append(child.name)
                    dirs[child.name] = runner.session_dir / s.name / child.name
        elif s.type == "review-code":
            names.append(s.name)
            dirs[s.name] = runner.session_dir
    return names, dirs


def _build_plan(entry: StageEntry, spec: ClientSpec, runner: StageRunner) -> Any:
    plan_val = getattr(runner.args, "plan", None)
    if not entry.prompt_paths and not plan_val:
        die(
            f"stage {entry.name!r}: type 'plan' requires a 'prompt' field in the pipeline YAML"
        )
    if not runner.repo:
        logger.info(
            "planning (model: %s) -> %s", spec.model, runner.session_dir / "plan.md"
        )
    return plan.Plan(
        entry,
        spec.model,
        plan_source=plan_val,
        plan_file=runner.session_dir / "plan.md" if not runner.repo else None,
        instructions=runner.instructions,
        repo=runner.repo,
    )


def _build_implement(entry: StageEntry, spec: ClientSpec, runner: StageRunner) -> Any:
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
        entry, spec.model, is_git=runner.is_git, spec_text=spec_text
    )


def _build_materialize_to_branch(
    entry: StageEntry, spec: ClientSpec, _runner: StageRunner
) -> Any:
    return materialize_to_branch_mod.MaterializeToBranch(entry, spec.model)


def _build_verify(entry: StageEntry, spec: ClientSpec, runner: StageRunner) -> Any:
    if not runner.repo:
        cmds = getattr(runner.args, "cmds", None)
        if cmds is not None:
            entry.options["cmds"] = cmds
        entry.options.setdefault(
            "max_attempts", getattr(runner.args, "test_max_attempts", 3)
        )
        resolved_cmds = entry.options.get("cmds", [])
        if resolved_cmds:
            logger.info(
                "running verify (cmds: %r, max-attempts: %s, model: %s)",
                resolved_cmds,
                entry.options.get("max_attempts"),
                spec.model,
            )
    return verify.Verify(entry, spec.model, is_git=runner.is_git)


def _build_commit(entry: StageEntry, spec: ClientSpec, _runner: StageRunner) -> Any:
    return commit.Commit(entry, spec.model)


def _build_open_github_pr(
    entry: StageEntry, spec: ClientSpec, runner: StageRunner
) -> Any:
    return open_github_pr.OpenGitHubPR(
        entry,
        spec.model,
        issue_url=read_state_str(runner.state_file, "issue_url"),
    )


def _build_request_copilot(
    entry: StageEntry, spec: ClientSpec, runner: StageRunner
) -> Any:
    return request_copilot.RequestCopilot(entry, spec.model, repo=runner.repo)


def _build_ghreview(entry: StageEntry, spec: ClientSpec, _runner: StageRunner) -> Any:
    if not entry.prompt_paths:
        die(
            f"stage {entry.name!r}: type 'ghreview' requires a 'prompt' field in the pipeline YAML"
        )
    return review_code.ReviewCode(entry, spec.model, plan_text="", is_git=True)


def _build_wait_copilot(
    entry: StageEntry, spec: ClientSpec, runner: StageRunner
) -> Any:
    return wait_copilot.WaitCopilot(entry, spec.model, repo=runner.repo)


def _build_ghaddress(entry: StageEntry, spec: ClientSpec, _runner: StageRunner) -> Any:
    if not entry.prompt_paths:
        die(
            f"stage {entry.name!r}: type 'ghaddress' requires a 'prompt' field in the pipeline YAML"
        )
    return address_code.AddressCode(entry, spec.model, is_git=True)


def _build_wait_ci(entry: StageEntry, spec: ClientSpec, _runner: StageRunner) -> Any:
    return wait_ci.WaitCI(entry, spec.model)


def _build_review_code(entry: StageEntry, spec: ClientSpec, runner: StageRunner) -> Any:
    plan_file = runner.session_dir / "plan.md"
    plan_text = plan_file.read_text(encoding="utf-8")
    logger.info("reviewing code (model: %s)", spec.model)
    return review_code.ReviewCode(
        entry, spec.model, plan_text=plan_text, is_git=runner.is_git
    )


def _build_address_code(
    entry: StageEntry, spec: ClientSpec, runner: StageRunner
) -> Any:
    names, dirs = _review_stage_info(runner)
    logger.info("addressing code reviews (model: %s)", spec.model)
    return address_code.AddressCode(
        entry,
        spec.model,
        is_git=runner.is_git,
        review_stage_names=names,
        review_stage_dirs=dirs,
    )


def _build_chain(entry: StageEntry, spec: ClientSpec, runner: StageRunner) -> Any:
    logger.info("running chain stage (child: %s)", entry.options.get("child", "local"))
    return chain.Chain(entry, spec, pipeline_builder=runner.build_child_stages)


def _build_parallel(entry: StageEntry, spec: ClientSpec, runner: StageRunner) -> Any:
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
            client=runner._get_client(child_spec),
            session_dir=child_dir,
            gr_id=runner.gr_id,
            child_key=child.name,
        )
        child_runners.append(
            (child.name, child_ctx, runner._make_runner(child, child_ctx, child_spec))
        )
    gr_id = runner.gr_id
    return ParallelStage(
        entry,
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
register_stage_builder("chain", _build_chain, needs_pipe=False)
register_stage_builder("parallel", _build_parallel, needs_pipe=True)


__all__ = [
    "address_code",
    "chain",
    "commit",
    "commit_pr",
    "implement",
    "open_github_pr",
    "plan",
    "request_copilot",
    "review_code",
    "verify",
    "wait_ci",
    "wait_copilot",
]
