"""Address-code stage."""

from __future__ import annotations

import glob
import pathlib
import re
from typing import Any

from gremlins.executor.state import State
from gremlins.stages.agent import bail_command, run_agent
from gremlins.stages.base import Stage
from gremlins.stages.outcome import Done, Outcome

MODEL_RE = re.compile(r"^[A-Za-z0-9._-]+$")


def _review_stage_info(
    state: State,
) -> tuple[list[str], dict[str, pathlib.Path]]:
    names: list[str] = []
    dirs: dict[str, pathlib.Path] = {}
    scope = state.current_scope or (
        list(state.pipeline_data.stages) if state.pipeline_data else []
    )
    for s in scope:
        if s.type == "parallel":
            for child in s.body:
                if child.type == "review-code":
                    names.append(child.name)
                    dirs[child.name] = state.session_dir / s.name / child.name
        elif s.type == "review-code":
            names.append(s.name)
            dirs[s.name] = state.session_dir
    return names, dirs


def _model_from(path: pathlib.Path, stage_name: str) -> str:
    stem = path.stem
    prefix = f"{stage_name}-"
    model = stem[len(prefix) :] if stem.startswith(prefix) else ""
    if not model or not MODEL_RE.match(model):
        raise ValueError(
            f"cannot extract a valid model name from review file: {path.name}"
        )
    return model


class AddressCode(Stage):
    type = "address-code"

    def __init__(self, name: str, prompts: list[str], options: dict[str, Any]) -> None:
        super().__init__(name)
        self.prompts = prompts
        self.options = options

    async def run(self, state: State) -> Outcome:
        inputs = self._inputs_from_local(state)
        await self._run_local(inputs, state)
        return Done()

    def _inputs_from_local(self, state: State) -> dict[str, str]:
        names, dirs = _review_stage_info(state)
        if not names:
            names = ["review-code"]
        review_files: list[tuple[str, pathlib.Path]] = []
        for stage_name in names:
            search_dir = dirs.get(stage_name, state.session_dir)
            for m in sorted(glob.glob(str(search_dir / f"{stage_name}-*.md"))):
                review_files.append((stage_name, pathlib.Path(m)))
        if not review_files:
            stages_str = ", ".join(names)
            searched = ", ".join(str(dirs.get(s, state.session_dir)) for s in names)
            raise FileNotFoundError(
                f"no review files found in [{searched}] (stages: {stages_str})"
            )
        first_stage_name, first_path = review_files[0]
        review_model = _model_from(first_path, first_stage_name)
        text = "\n\n---\n\n".join(
            p.read_text(encoding="utf-8") for _, p in review_files
        )
        return {"text": text, "review_model": review_model}

    async def _run_local(self, inputs: dict[str, str], state: State) -> None:
        template = "\n\n".join(self.prompts).rstrip()
        address_prompt = template.format(
            bail_command=bail_command(state),
            model=inputs["review_model"],
            text=inputs["text"],
        )
        await run_agent(
            state,
            address_prompt,
            label="address-code",
            raw_path=state.session_dir / "stream-address.jsonl",
        )


class GitHubAddressPullRequestReviews(Stage):
    type = "github-address-pull-request-reviews"

    @classmethod
    def with_dict(
        cls, d: dict[str, Any], depth: int = 0
    ) -> GitHubAddressPullRequestReviews:
        from gremlins.pipeline.loader import get_client_from_dict

        prompts: list[str] = d.get("prompt") or []
        if not prompts:
            raise ValueError(
                f"stage {d['name']!r}: 'prompt' is required for github-address-pull-request-reviews"
            )
        stage = cls(d["name"], prompts, d.get("options") or {})
        stage.client = get_client_from_dict(d)
        return stage

    def __init__(
        self,
        name: str,
        prompts: list[str],
        options: dict[str, Any],
        *,
        pr_url: str = "",
    ) -> None:
        super().__init__(name)
        self.prompts = prompts
        self.options = options
        self.pr_url = pr_url

    async def run(self, state: State) -> Outcome:
        pr_url = self.pr_url or state.data.read_pr_url()
        if not pr_url:
            raise RuntimeError("no pr_url in state.json (rewind to open-pr?)")
        prompt = (
            "\n\n".join(self.prompts)
            .rstrip()
            .format(
                bail_command=bail_command(state),
                pr_url=pr_url,
            )
        )
        await run_agent(
            state,
            prompt,
            label="github-address-pull-request-reviews",
            raw_path=state.session_dir
            / "stream-github-address-pull-request-reviews.jsonl",
        )
        return Done()
