"""Record-PR-Artifact stage: writes {type: pr, url, branch} into state.artifacts."""

from __future__ import annotations

import json
import logging
import re
from typing import Any

from gremlins.executor.state import State
from gremlins.stages.base import Stage
from gremlins.stages.outcome import Done, Outcome
from gremlins.utils import proc

logger = logging.getLogger(__name__)


def pr_num_from_ref(ref: str) -> str:
    m = re.search(r"pull/(\d+)/head", ref)
    return m.group(1) if m else ""


class RecordPrArtifact(Stage):
    type = "record-pr-artifact"
    needs_gh = True

    def __init__(self, name: str) -> None:
        super().__init__(name)

    @classmethod
    def with_dict(cls, d: dict[str, Any], depth: int = 0) -> RecordPrArtifact:  # noqa: ARG003
        from gremlins.pipeline.loader import get_client_from_dict

        stage = cls(d["name"])
        stage.client = get_client_from_dict(d)
        return stage

    async def run(self, state: State) -> Outcome:
        pr_ref = state.data.worktree_base or state.data.base_ref_sha
        pr_num = pr_num_from_ref(pr_ref)
        if not pr_num:
            logger.warning(
                "record-pr-artifact: no pull/N/head ref in state (worktree_base=%r, base_ref_sha=%r)",
                state.data.worktree_base,
                state.data.base_ref_sha,
            )
            return Done()
        r = await proc.run_async(
            ["gh", "pr", "view", pr_num, "--json", "url,headRefName"],
            timeout=15,
        )
        if r.returncode != 0:
            logger.warning(
                "record-pr-artifact: gh pr view %s failed: %s", pr_num, r.stderr.strip()
            )
            return Done()
        data = json.loads(r.stdout)
        state.record_artifact(
            {"type": "pr", "url": data["url"], "branch": data["headRefName"]}
        )
        logger.info("PR: %s", data["url"])
        return Done()
