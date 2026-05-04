"""Address-code stage."""

from __future__ import annotations

import dataclasses
import glob
import pathlib
import re

from ..prompts import load_prompts
from ..state import emit_bail
from .context import StageContext
from .registry import register_stage

MODEL_RE = re.compile(r"^[A-Za-z0-9._-]+$")

_DEFAULT_PROMPT = (
    pathlib.Path(__file__).resolve().parent.parent
    / "pipelines"
    / "prompts"
    / "address_code.md"
)


@dataclasses.dataclass
class AddressCodeOptions:
    address_model: str
    is_git: bool
    code_style: str
    prompt_path: pathlib.Path = dataclasses.field(
        default_factory=lambda: _DEFAULT_PROMPT
    )


def _model_from(path: pathlib.Path, lens: str) -> str:
    stem = path.stem
    prefix = f"review-code-{lens}-"
    model = stem[len(prefix) :] if stem.startswith(prefix) else ""
    if not model or not MODEL_RE.match(model):
        raise ValueError(
            f"cannot extract a valid model name from review file: {path.name}"
        )
    return model


def run(ctx: StageContext, options: AddressCodeOptions) -> None:
    try:
        matches = sorted(glob.glob(str(ctx.session_dir / "review-code-detail-*.md")))
        if not matches:
            raise FileNotFoundError(
                f"no review-code-detail-*.md file found in {ctx.session_dir}"
            )
        if len(matches) > 1:
            raise RuntimeError(
                f"multiple review-code-detail-*.md files in {ctx.session_dir}: "
                f"{', '.join(matches)}"
            )
        review_file = pathlib.Path(matches[0])

        model = _model_from(review_file, "detail")
        text = review_file.read_text(encoding="utf-8")

        address_commit_instr = ""
        if options.is_git:
            address_commit_instr = (
                "After making all fixes, stage the changed files by name and "
                "create a single git commit titled 'Address review feedback' whose "
                "body references the review file. Do not push."
            )

        bail_section = ""
        if ctx.gr_id:
            bail_section = """

If a finding asks you to change something that touches secrets/credentials, or you decline to address one or more findings for any other reason that should halt automated recovery, run the bail helper before finishing:
  - `python -m gremlins.bail secrets "<one-line reason>"` if the blocked finding touches secrets.
  - `python -m gremlins.bail other "<one-line reason>"` for any other reason you cannot proceed.
Do not call this helper if you successfully addressed every actionable finding.
"""

        template = load_prompts([options.prompt_path])
        address_prompt = template.format(
            code_style=options.code_style,
            model=model,
            text=text,
            address_commit_instr=address_commit_instr,
            bail_section=bail_section,
        )
        ctx.client.run(
            address_prompt,
            label="address-code",
            model=options.address_model,
            raw_path=ctx.session_dir / "stream-address.jsonl",
        )
    except (SystemExit, Exception) as exc:
        emit_bail(ctx.gr_id, "other", f"address-code stage failed: {exc}"[:200])
        raise


register_stage("address-code", run)
