"""Address-code stage."""

from __future__ import annotations

import glob
import pathlib
import re
from typing import Any

from gremlins.executor.state import State, pipeline_uses_gh
from gremlins.stages.base import Stage
from gremlins.stages.registry import register_stage

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

    @classmethod
    def with_dict(cls, d: dict[str, Any], depth: int = 0) -> AddressCode:
        from gremlins.pipeline.loader import get_client_from_dict

        prompts: list[str] = d.get("prompt") or []
        if not prompts and d.get("type") == "ghaddress":
            raise ValueError(f"stage {d['name']!r}: 'prompt' is required for ghaddress")
        stage = cls(d["name"], None, prompts, d.get("options") or {})
        stage.client = get_client_from_dict(d)
        return stage

    def __init__(
        self,
        name: str,
        model: str | None,
        prompts: list[str],
        options: dict[str, Any],
        *,
        pr_url: str = "",
    ) -> None:
        super().__init__(name, model, prompts, options)
        self.pr_url = pr_url

    def run(self, state: State) -> None:
        is_gh = bool(state.pipeline_data and pipeline_uses_gh(state.pipeline_data))
        if is_gh:
            self.results_to_github(state)
        else:
            try:
                inputs = self._inputs_from_local(state)
                self.results_to_local(inputs, state)
            except (SystemExit, Exception) as exc:
                state.emit_bail(
                    "other",
                    f"address-code stage failed: {exc}"[:200],
                )
                raise

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

    def results_to_local(self, inputs: dict[str, str], state: State) -> None:
        address_commit_instr = ""
        if state.is_git:
            address_commit_instr = (
                "After making all fixes, stage the changed files by name and "
                "create a single git commit titled 'Address review feedback' whose "
                "body references the review file. Do not push."
            )
        template = "\n\n".join(self.prompts).rstrip()
        address_prompt = template.format(
            bail_command=self.bail_command(state),
            model=inputs["review_model"],
            text=inputs["text"],
            address_commit_instr=address_commit_instr,
        )
        self.run_claude(
            address_prompt,
            state=state,
            label="address-code",
            raw_path=state.session_dir / "stream-address.jsonl",
        )

    def results_to_github(self, state: State) -> None:
        pr_url = self.pr_url or state.read_pr_url()
        if not pr_url:
            raise RuntimeError("no pr_url in state.json (rewind to open-pr?)")
        prompt = (
            "\n\n".join(self.prompts)
            .rstrip()
            .format(
                bail_command=self.bail_command(state),
                pr_url=pr_url,
            )
        )
        self.run_claude(
            prompt,
            state=state,
            label="ghaddress",
            raw_path=state.session_dir / "stream-ghaddress.jsonl",
        )
        state.check_bail("/ghaddress")


register_stage("address-code", AddressCode)
register_stage("ghaddress", AddressCode)
