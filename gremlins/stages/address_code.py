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
from gremlins.state import emit_bail

MODEL_RE = re.compile(r"^[A-Za-z0-9._-]+$")

_DEFAULT_PROMPT = (
    pathlib.Path(__file__).resolve().parent.parent
    / "pipelines"
    / "prompts"
    / "address_code.md"
)


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
        code_style: str,
        review_stage_names: list[str] | None = None,
    ) -> None:
        super().__init__(entry, model)
        self.is_git = is_git
        self.code_style = code_style
        self.review_stage_names = (
            review_stage_names if review_stage_names is not None else ["review-code"]
        )

    def run(self, pipe: Any) -> None:
        try:
            review_files: list[tuple[str, pathlib.Path]] = []
            for stage_name in self.review_stage_names:
                for m in sorted(
                    glob.glob(str(self.state.session_dir / f"{stage_name}-*.md"))
                ):
                    review_files.append((stage_name, pathlib.Path(m)))

            if not review_files:
                stages_str = ", ".join(self.review_stage_names)
                raise FileNotFoundError(
                    f"no review files found in {self.state.session_dir} (stages: {stages_str})"
                )

            first_stage_name, first_path = review_files[0]
            model = _model_from(first_path, first_stage_name)
            text = "\n\n---\n\n".join(
                p.read_text(encoding="utf-8") for _, p in review_files
            )

            address_commit_instr = ""
            if self.is_git:
                address_commit_instr = (
                    "After making all fixes, stage the changed files by name and "
                    "create a single git commit titled 'Address review feedback' whose "
                    "body references the review file. Do not push."
                )

            bail_section = ""
            if self.state.gr_id:
                bail_section = """

If a finding asks you to change something that touches secrets/credentials, or you decline to address one or more findings for any other reason that should halt automated recovery, run the bail helper before finishing:
  - `python -m gremlins.bail secrets "<one-line reason>"` if the blocked finding touches secrets.
  - `python -m gremlins.bail other "<one-line reason>"` for any other reason you cannot proceed.
Do not call this helper if you successfully addressed every actionable finding.
"""

            prompt_path = (
                self.prompt_paths[-1] if self.prompt_paths else _DEFAULT_PROMPT
            )
            template = load_prompts([prompt_path])
            address_prompt = template.format(
                code_style=self.code_style,
                model=model,
                text=text,
                address_commit_instr=address_commit_instr,
                bail_section=bail_section,
            )
            self.run_claude(
                address_prompt,
                label="address-code",
                raw_path=self.state.session_dir / "stream-address.jsonl",
            )
        except (SystemExit, Exception) as exc:
            emit_bail(
                self.state.gr_id,
                "other",
                f"address-code stage failed: {exc}"[:200],
                child_key=self.state.child_key,
            )
            raise


register_stage("address-code", AddressCode)
