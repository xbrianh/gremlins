"""gremlins init — scaffold .gremlins/ with editable copies of bundled pipelines."""

from __future__ import annotations

import argparse
import pathlib
import sys
from typing import Any

import yaml

_PIPELINES_DIR = pathlib.Path(__file__).resolve().parent / "pipelines"
_PROMPTS_DIR = _PIPELINES_DIR / "prompts"


def _bundled_pipeline_names() -> list[str]:
    return sorted(p.stem for p in _PIPELINES_DIR.glob("*.yaml"))


def _collect_prompt_subpaths(stages: list[Any]) -> list[str]:
    """Walk stage list (including parallel groups) and return unique prompt subpaths.

    Subpaths are the part after 'prompts/' — e.g. 'plan.md', 'lenses/detail.md'.
    """
    seen: set[str] = set()
    result: list[str] = []

    def _walk(stage: Any) -> None:
        if not isinstance(stage, dict):
            return
        if "parallel" in stage:
            for child in stage["parallel"]:
                _walk(child)
            return
        prompts = stage.get("prompt")
        if not prompts:
            return
        if isinstance(prompts, str):
            prompts = [prompts]
        for p in prompts:
            if p.startswith("prompts/") and p not in seen:
                seen.add(p)
                result.append(p[len("prompts/"):])

    for stage in stages:
        _walk(stage)
    return result


def _rewrite_stage(stage: Any) -> Any:
    """Return stage with prompt paths rewritten from prompts/ to ../prompts/."""
    if not isinstance(stage, dict):
        return stage
    stage = dict(stage)
    if "parallel" in stage:
        stage["parallel"] = [_rewrite_stage(c) for c in stage["parallel"]]
        return stage
    prompts = stage.get("prompt")
    if prompts is None:
        return stage
    if isinstance(prompts, str):
        if prompts.startswith("prompts/"):
            stage["prompt"] = "../" + prompts
    else:
        stage["prompt"] = [
            ("../" + p if p.startswith("prompts/") else p) for p in prompts
        ]
    return stage


def init_main(argv: list[str]) -> int:
    p = argparse.ArgumentParser(
        prog="gremlins init",
        description="Scaffold .gremlins/ with editable copies of bundled pipelines.",
    )
    p.add_argument(
        "--pipeline",
        action="append",
        dest="pipelines",
        metavar="NAME",
        help="Pipeline to scaffold (repeatable; default: all).",
    )
    p.add_argument(
        "--force",
        action="store_true",
        help="Overwrite existing files.",
    )
    p.add_argument(
        "--path",
        default=None,
        metavar="DIR",
        help="Scaffold under DIR/.gremlins/ (default: cwd).",
    )
    args = p.parse_args(argv)

    bundled = _bundled_pipeline_names()
    selected = args.pipelines or bundled

    unknown = [n for n in selected if n not in bundled]
    if unknown:
        sys.stderr.write(
            f"error: unknown pipeline(s): {', '.join(unknown)}\n"
            f"bundled pipelines: {', '.join(bundled)}\n"
        )
        return 1

    base = pathlib.Path(args.path) if args.path else pathlib.Path.cwd()
    dot_gremlins = base / ".gremlins"

    # Collect prompt subpaths across all selected pipelines
    prompt_subpaths: list[str] = []
    seen_subpaths: set[str] = set()
    for name in selected:
        data = yaml.safe_load(
            (_PIPELINES_DIR / f"{name}.yaml").read_text(encoding="utf-8")
        )
        for subpath in _collect_prompt_subpaths(data.get("stages", [])):
            if subpath not in seen_subpaths:
                seen_subpaths.add(subpath)
                prompt_subpaths.append(subpath)

    prompt_targets: list[tuple[pathlib.Path, pathlib.Path]] = [
        (_PROMPTS_DIR / sub, dot_gremlins / "prompts" / sub)
        for sub in prompt_subpaths
    ]
    pipeline_targets: list[tuple[str, pathlib.Path]] = [
        (name, dot_gremlins / "pipelines" / f"{name}.yaml") for name in selected
    ]

    if not args.force:
        conflicts = [dst for _, dst in prompt_targets if dst.exists()] + [
            dst for _, dst in pipeline_targets if dst.exists()
        ]
        if conflicts:
            for c in conflicts:
                sys.stderr.write(f"error: already exists: {c}\n")
            return 1

    for src, dst in prompt_targets:
        dst.parent.mkdir(parents=True, exist_ok=True)
        dst.write_bytes(src.read_bytes())
        sys.stdout.write(f"{dst}\n")

    for name, dst in pipeline_targets:
        data = yaml.safe_load(
            (_PIPELINES_DIR / f"{name}.yaml").read_text(encoding="utf-8")
        )
        data["stages"] = [_rewrite_stage(s) for s in data.get("stages", [])]
        dst.parent.mkdir(parents=True, exist_ok=True)
        dst.write_text(yaml.dump(data, default_flow_style=False), encoding="utf-8")
        sys.stdout.write(f"{dst}\n")

    return 0
