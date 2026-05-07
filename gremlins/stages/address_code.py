"""Address-code stage."""

from __future__ import annotations

import glob
import pathlib
import re
from typing import Any

from gremlins.pipeline import StageEntry
from gremlins.prompts import load_prompts
from gremlins.stages.base import Stage
from gremlins.stages.registry import register_stage
from gremlins.state import check_bail, emit_bail

MODEL_RE = re.compile(r"^[A-Za-z0-9._-]+$")


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
    def __init__(
        self,
        entry: StageEntry,
        model: str | None,
        *,
        is_git: bool,
        review_stage_names: list[str] | None = None,
        review_stage_dirs: dict[str, pathlib.Path] | None = None,
        pr_url: str = "",
    ) -> None:
        super().__init__(entry, model)
        self.is_git = is_git
        self.review_stage_names = (
            review_stage_names if review_stage_names is not None else ["review-code"]
        )
        self.review_stage_dirs = review_stage_dirs or {}
        self.pr_url = pr_url

    def run(self, pipe: Any) -> None:
        target = getattr(pipe, "target", "local")
        if target == "github":
            self.results_to_github(pipe)
        else:
            try:
                inputs = self._inputs_from_local(pipe)
                self.results_to_local(inputs, pipe)
            except (SystemExit, Exception) as exc:
                emit_bail(
                    self.state.gr_id,
                    "other",
                    f"address-code stage failed: {exc}"[:200],
                    child_key=self.state.child_key,
                )
                raise

    def _inputs_from_local(self, pipe: Any) -> dict[str, str]:
        review_files: list[tuple[str, pathlib.Path]] = []
        for stage_name in self.review_stage_names:
            search_dir = self.review_stage_dirs.get(stage_name, self.state.session_dir)
            for m in sorted(glob.glob(str(search_dir / f"{stage_name}-*.md"))):
                review_files.append((stage_name, pathlib.Path(m)))
        if not review_files:
            stages_str = ", ".join(self.review_stage_names)
            searched = ", ".join(
                str(self.review_stage_dirs.get(s, self.state.session_dir))
                for s in self.review_stage_names
            )
            raise FileNotFoundError(
                f"no review files found in [{searched}] (stages: {stages_str})"
            )
        first_stage_name, first_path = review_files[0]
        review_model = _model_from(first_path, first_stage_name)
        text = "\n\n---\n\n".join(
            p.read_text(encoding="utf-8") for _, p in review_files
        )
        return {"text": text, "review_model": review_model}

    def results_to_local(self, inputs: dict[str, str], pipe: Any) -> None:
        address_commit_instr = ""
        if self.is_git:
            address_commit_instr = (
                "After making all fixes, stage the changed files by name and "
                "create a single git commit titled 'Address review feedback' whose "
                "body references the review file. Do not push."
            )
        template = load_prompts(self.prompt_paths)
        address_prompt = template.format(
            bail_command=self.bail_command(),
            model=inputs["review_model"],
            text=inputs["text"],
            address_commit_instr=address_commit_instr,
        )
        self.run_claude(
            address_prompt,
            label="address-code",
            raw_path=self.state.session_dir / "stream-address.jsonl",
        )

    def results_to_github(self, pipe: Any) -> None:
        prompt = load_prompts(self.prompt_paths).format(
            bail_command=self.bail_command(),
            pr_url=self.pr_url,
        )
        self.run_claude(
            prompt,
            label="ghaddress",
            raw_path=self.state.session_dir / "stream-ghaddress.jsonl",
        )
        check_bail(self.state.gr_id, "/ghaddress", child_key=self.state.child_key)


register_stage("address-code", AddressCode)
register_stage("ghaddress", AddressCode)
