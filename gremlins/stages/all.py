"""Import all stage modules so each self-registers into the stage registry."""

from __future__ import annotations

import dataclasses
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
from gremlins.stages.base import RuntimeState
from gremlins.stages.registry import register_stage_builder


def _build_plan(entry: StageEntry, spec: Client, _state: RuntimeState) -> Any:
    return plan.Plan(entry.name, spec.model, entry.prompts, entry.options)


def _build_implement(entry: StageEntry, spec: Client, _state: RuntimeState) -> Any:
    return implement.Implement(entry.name, spec.model, entry.prompts, entry.options)


def _build_materialize_to_branch(
    entry: StageEntry, spec: Client, _state: RuntimeState
) -> Any:
    return materialize_to_branch_mod.MaterializeToBranch(
        entry.name, spec.model, entry.prompts, entry.options
    )


def _build_verify(entry: StageEntry, spec: Client, _state: RuntimeState) -> Any:
    return verify.Verify(entry.name, spec.model, entry.prompts, entry.options)


def _build_commit(entry: StageEntry, spec: Client, _state: RuntimeState) -> Any:
    return commit.Commit(entry.name, spec.model, entry.prompts, entry.options)


def _build_open_github_pr(entry: StageEntry, spec: Client, _state: RuntimeState) -> Any:
    return open_github_pr.OpenGitHubPR(
        entry.name,
        spec.model,
        entry.prompts,
        entry.options,
        base_ref=(entry.options.get("base_ref") or "").strip() or None,
    )


def _build_request_copilot(
    entry: StageEntry, spec: Client, _state: RuntimeState
) -> Any:
    return request_copilot.RequestCopilot(
        entry.name, spec.model, entry.prompts, entry.options
    )


def _build_ghreview(entry: StageEntry, spec: Client, _state: RuntimeState) -> Any:
    if not entry.prompts:
        die(
            f"stage {entry.name!r}: type 'ghreview' requires a 'prompt' field in the pipeline YAML"
        )
    return review_code.ReviewCode(
        entry.name,
        spec.model,
        entry.prompts,
        entry.options,
    )


def _build_wait_copilot(entry: StageEntry, spec: Client, _state: RuntimeState) -> Any:
    return wait_copilot.WaitCopilot(
        entry.name, spec.model, entry.prompts, entry.options
    )


def _build_ghaddress(entry: StageEntry, spec: Client, _state: RuntimeState) -> Any:
    if not entry.prompts:
        die(
            f"stage {entry.name!r}: type 'ghaddress' requires a 'prompt' field in the pipeline YAML"
        )
    return address_code.AddressCode(
        entry.name, spec.model, entry.prompts, entry.options
    )


def _build_wait_ci(entry: StageEntry, spec: Client, _state: RuntimeState) -> Any:
    return wait_ci.WaitCI(entry.name, spec.model, entry.prompts, entry.options)


def _build_review_code(entry: StageEntry, spec: Client, _state: RuntimeState) -> Any:
    return review_code.ReviewCode(entry.name, spec.model, entry.prompts, entry.options)


def _build_address_code(entry: StageEntry, spec: Client, _state: RuntimeState) -> Any:
    return address_code.AddressCode(
        entry.name, spec.model, entry.prompts, entry.options
    )


def _build_loop(entry: StageEntry, spec: Client, state: RuntimeState) -> Any:
    from gremlins.stages.loop import LoopStage

    max_iterations = entry.options.get("max_iterations", 3)
    body_runners: list[Any] = []
    for child in entry.body:
        child_spec = state.stage_specs.get(child.name, spec)
        child_state = dataclasses.replace(state, client=state.get_client(child_spec))
        body_runners.append(
            child_state.make_runner(child, child_spec, scope=entry.body)
        )
    return LoopStage(
        entry.name, body_runners=body_runners, max_iterations=max_iterations
    )


def _build_sequence(entry: StageEntry, spec: Client, state: RuntimeState) -> Any:
    from gremlins.stages.sequence import SequenceStage

    body: list[Any] = []
    for child in entry.body:
        child_spec = state.stage_specs.get(child.name, spec)
        child_state = dataclasses.replace(state, client=state.get_client(child_spec))
        child_runner = child_state.make_runner(child, child_spec, scope=entry.body)
        body.append((child_state, child_runner))
    return SequenceStage(entry.name, body=body)


def _build_run_cmd(entry: StageEntry, spec: Client, _state: RuntimeState) -> Any:
    return run_cmd.RunCmd(entry.name, spec.model, entry.prompts, entry.options)


def _build_claude_prompt(entry: StageEntry, spec: Client, _state: RuntimeState) -> Any:
    if not entry.prompts:
        die(f"stage {entry.name!r}: type 'claude-prompt' requires a 'prompt' field")
    return claude_prompt.ClaudePrompt(
        entry.name, spec.model, entry.prompts, entry.options
    )


def _build_handoff(entry: StageEntry, spec: Client, _state: RuntimeState) -> Any:
    from gremlins.stages.handoff import Handoff

    return Handoff(entry.name, spec)


def _build_parallel(entry: StageEntry, _spec: Client, state: RuntimeState) -> Any:
    from gremlins.stages.parallel import ParallelStage
    from gremlins.state import set_stage

    group_dir = state.session_dir / entry.name
    group_dir.mkdir(parents=True, exist_ok=True)
    child_runners: list[tuple[str, Any, Any]] = []
    for child in entry.body:
        child_spec = require_stage_spec(state.stage_specs, child.name)
        child_dir = group_dir / child.name
        child_dir.mkdir(parents=True, exist_ok=True)
        child_state = dataclasses.replace(
            state,
            client=state.get_client(child_spec),
            session_dir=child_dir,
            child_key=child.name,
        )
        child_runners.append(
            (
                child.name,
                child_state,
                child_state.make_runner(child, child_spec, scope=entry.body),
            )
        )
    gr_id = state.gr_id
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


register_stage_builder("plan", _build_plan)
register_stage_builder("implement", _build_implement)
register_stage_builder("materialize-to-branch", _build_materialize_to_branch)
register_stage_builder("verify", _build_verify)
register_stage_builder("commit", _build_commit)
register_stage_builder("open-github-pr", _build_open_github_pr)
register_stage_builder("request-copilot", _build_request_copilot)
register_stage_builder("ghreview", _build_ghreview)
register_stage_builder("wait-copilot", _build_wait_copilot)
register_stage_builder("ghaddress", _build_ghaddress)
register_stage_builder("wait-ci", _build_wait_ci)
register_stage_builder("review-code", _build_review_code)
register_stage_builder("address-code", _build_address_code)
register_stage_builder("loop", _build_loop)
register_stage_builder("sequence", _build_sequence)
register_stage_builder("run-cmd", _build_run_cmd)
register_stage_builder("claude-prompt", _build_claude_prompt)
register_stage_builder("handoff", _build_handoff)
register_stage_builder("parallel", _build_parallel)


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
