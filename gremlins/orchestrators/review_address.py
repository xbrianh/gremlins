"""Library functions for review-code and address-code stages."""

from __future__ import annotations

import logging
import pathlib

import gremlins.stages.address_code as address_code
import gremlins.stages.review_code as review_code
from gremlins.clients.client import Client
from gremlins.errors import die
from gremlins.pipeline import Pipeline
from gremlins.pipeline.discovery import resolve_pipeline_path
from gremlins.stages.base import RuntimeState
from gremlins.utils.git import has_diff, has_dirty_worktree, in_git_repo, rev_exists

logger = logging.getLogger(__name__)


def run_review(session_dir: pathlib.Path, plan_text: str, detail: str, client: Client) -> int:
    is_git = in_git_repo()
    if is_git:
        head1_exists = rev_exists("HEAD~1")
        has_commit_diff = head1_exists and has_diff("HEAD~1", "HEAD")
        if not has_commit_diff and not has_dirty_worktree():
            if not head1_exists:
                die("nothing to review: no commit history beyond HEAD and working tree is clean")
            die("nothing to review: HEAD~1..HEAD has no changes and working tree is clean")

    pipeline = Pipeline.from_yaml(resolve_pipeline_path("local", pathlib.Path.cwd()))
    rc_entry = next((s for s in pipeline.stages if s.type == "review-code"), None)
    if rc_entry is None or not rc_entry.prompts[1:]:
        die("local pipeline has no review-code stage with a prompt")
    state = RuntimeState(client=client, session_dir=session_dir, gr_id=None, is_git=is_git)
    if plan_text:
        (session_dir / "plan.md").write_text(plan_text, encoding="utf-8")
    logger.info("reviewing code (model: %s)", detail)
    stage = review_code.ReviewCode(rc_entry.name, detail, rc_entry.prompts, rc_entry.options)
    review_file = stage.run(state)
    logger.info("code review (%s): %s", detail, review_file)
    return 0


def run_address(session_dir: pathlib.Path, address: str, client: Client) -> int:
    is_git = in_git_repo()
    pipeline = Pipeline.from_yaml(resolve_pipeline_path("local", pathlib.Path.cwd()))
    ac_entry = next((s for s in pipeline.stages if s.type == "address-code"), None)
    if ac_entry is None or not ac_entry.prompts:
        die("local pipeline has no address-code stage with a prompt")
    state = RuntimeState(client=client, session_dir=session_dir, gr_id=None, is_git=is_git)
    logger.info("addressing code reviews (model: %s)", address)
    stage = address_code.AddressCode(ac_entry.name, address, ac_entry.prompts, ac_entry.options)
    stage.run(state)
    return 0
