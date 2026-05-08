"""gremlins init — scaffold .gremlins/ with editable copies of bundled pipelines."""

from __future__ import annotations

import argparse
import os
import pathlib
import sys
from typing import Any, cast

import yaml

from gremlins import PACKAGE_ROOT

_PIPELINES_DIR = PACKAGE_ROOT / "pipelines"
_PROMPTS_DIR = PACKAGE_ROOT / "prompts"


def _bundled_pipeline_names() -> list[str]:
    return sorted(p.stem for p in _PIPELINES_DIR.glob("*.yaml"))


_BUNDLED_PREFIX = "gremlins:"


def _strip_bundled_prefix(name: str) -> str:
    return name[len(_BUNDLED_PREFIX) :] if name.startswith(_BUNDLED_PREFIX) else name


def _rewrite_prompts_to_bare(stages: list[Any]) -> None:
    """Strip the `gremlins:` prefix on every `prompt:` entry in-place.

    After init, scaffolded YAMLs reference editable local copies under
    `.gremlins/prompts/`, so bundled-prefixed names become bare names that
    resolve against `prompt_dir`.
    """
    for stage in stages:
        if not isinstance(stage, dict):
            continue
        s = cast(dict[str, Any], stage)
        if "parallel" in s:
            _rewrite_prompts_to_bare(cast(list[Any], s["parallel"]))
            continue
        prompts = s.get("prompt")
        if not prompts:
            continue
        if isinstance(prompts, str):
            s["prompt"] = _strip_bundled_prefix(prompts)
        else:
            s["prompt"] = [_strip_bundled_prefix(p) for p in cast(list[str], prompts)]


def _collect_prompt_subpaths(stages: list[Any]) -> list[str]:
    """Walk stage list (including parallel groups) and return unique prompt
    subpaths, with any `gremlins:` prefix stripped — bundled pipelines
    reference their own prompts via that prefix, but on disk they live as
    bare names under the bundled prompts dir."""
    seen: set[str] = set()
    result: list[str] = []

    def _walk(stage: Any) -> None:
        if not isinstance(stage, dict):
            return
        s = cast(dict[str, Any], stage)
        if "parallel" in s:
            for child in cast(list[Any], s["parallel"]):
                _walk(child)
            return
        prompts = s.get("prompt")
        if not prompts:
            return
        if isinstance(prompts, str):
            prompts = [prompts]
        for p in cast(list[str], prompts):
            bare = _strip_bundled_prefix(p)
            if bare not in seen:
                seen.add(bare)
                result.append(bare)

    for stage in stages:
        _walk(stage)
    return result


def _parse_args(argv: list[str]) -> argparse.Namespace:
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
    return p.parse_args(argv)


def _validate_selection(selected: list[str], bundled: list[str]) -> int | None:
    unknown = [n for n in selected if n not in bundled]
    if not unknown:
        return None
    sys.stderr.write(
        f"error: unknown pipeline(s): {', '.join(unknown)}\n"
        f"bundled pipelines: {', '.join(bundled)}\n"
    )
    return 1


def _format_yaml_error(name: str, exc: yaml.YAMLError) -> str:
    mark = getattr(exc, "problem_mark", None)
    problem = getattr(exc, "problem", None) or " ".join(str(exc).split())
    if mark is None:
        return f"parse failed in {name}: {problem}"
    return f"parse failed in {name}: {problem} (line {mark.line + 1}, column {mark.column + 1})"


class _YamlParseError(Exception):
    def __init__(self, name: str, original: yaml.YAMLError) -> None:
        super().__init__(_format_yaml_error(name, original))
        self.name = name


def _tmp_path(dst: pathlib.Path) -> pathlib.Path:
    return dst.with_suffix(dst.suffix + f".tmp.{os.getpid()}")


def _build_plan(
    selected: list[str], base: pathlib.Path
) -> list[tuple[pathlib.Path, bytes]]:
    dot_gremlins = base / ".gremlins"
    plan: list[tuple[pathlib.Path, bytes]] = []

    pipeline_data: dict[str, dict[str, Any]] = {}
    for name in selected:
        text = (_PIPELINES_DIR / f"{name}.yaml").read_text(encoding="utf-8")
        try:
            raw = yaml.safe_load(text)
        except yaml.YAMLError as exc:
            raise _YamlParseError(f"{name}.yaml", exc) from exc
        if not isinstance(raw, dict):
            raise ValueError(f"malformed pipeline YAML: {name}.yaml")
        pipeline_data[name] = cast(dict[str, Any], raw)

    seen_subpaths: set[str] = set()
    for name in selected:
        for subpath in _collect_prompt_subpaths(pipeline_data[name].get("stages", [])):
            if subpath not in seen_subpaths:
                seen_subpaths.add(subpath)
                src = _PROMPTS_DIR / subpath
                dst = dot_gremlins / "prompts" / subpath
                plan.append((dst, src.read_bytes()))

    for name in selected:
        data = dict(pipeline_data[name])
        data.setdefault("prompt_dir", "../prompts")
        stages = cast(list[Any], data.get("stages", []))
        _rewrite_prompts_to_bare(stages)
        dst = dot_gremlins / "pipelines" / f"{name}.yaml"
        content = yaml.safe_dump(data, default_flow_style=False, sort_keys=False)
        plan.append((dst, content.encode("utf-8")))

    plan.append((base / "AGENTS.md", (_PIPELINES_DIR / "AGENTS.md").read_bytes()))

    plan.append((dot_gremlins / ".gitignore", b"env\n"))

    return plan


def _check_conflicts(plan: list[tuple[pathlib.Path, bytes]], force: bool) -> int | None:
    if force:
        return None
    conflicts = [dst for dst, _ in plan if dst.exists()]
    if not conflicts:
        return None
    for c in conflicts:
        sys.stderr.write(f"error: already exists: {c}\n")
    return 1


def _stage_writes(plan: list[tuple[pathlib.Path, bytes]]) -> list[pathlib.Path]:
    staged: list[pathlib.Path] = []
    for dst, data in plan:
        dst.parent.mkdir(parents=True, exist_ok=True)
        tmp = _tmp_path(dst)
        tmp.write_bytes(data)
        staged.append(tmp)
    return staged


def _commit_writes(
    staged: list[pathlib.Path], plan: list[tuple[pathlib.Path, bytes]]
) -> None:
    for tmp, (dst, _) in zip(staged, plan):
        tmp.replace(dst)
        sys.stdout.write(f"{dst}\n")


def _cleanup_tmp(paths: list[pathlib.Path]) -> None:
    for p in paths:
        try:
            p.unlink()
        except OSError:
            pass


def init_main(argv: list[str]) -> int:
    args = _parse_args(argv)
    bundled = _bundled_pipeline_names()
    selected = list(dict.fromkeys(args.pipelines or bundled))
    if rc := _validate_selection(selected, bundled):
        return rc
    plan: list[tuple[pathlib.Path, bytes]] = []
    try:
        base = pathlib.Path(args.path) if args.path else pathlib.Path.cwd()
        plan = _build_plan(selected, base)
        if rc := _check_conflicts(plan, args.force):
            return rc
        staged = _stage_writes(plan)
        try:
            _commit_writes(staged, plan)
        except OSError:
            _cleanup_tmp(staged)
            raise
    except _YamlParseError as exc:
        sys.stderr.write(f"error: {exc}\n")
        _cleanup_tmp([_tmp_path(dst) for dst, _ in plan])
        return 1
    except (OSError, yaml.YAMLError, ValueError) as exc:
        sys.stderr.write(f"error: {str(exc).splitlines()[0]}\n")
        _cleanup_tmp([_tmp_path(dst) for dst, _ in plan])
        return 1
    return 0
