"""gremlins init — scaffold .gremlins/ with editable copies of bundled pipelines."""

from __future__ import annotations

import argparse
import os
import pathlib
import sys
from typing import Any, cast

from gremlins import PACKAGE_ROOT
from gremlins.utils.yaml import YamlLoadError, dump_yaml_text, load_yaml_file

_PIPELINES_DIR = PACKAGE_ROOT / "pipelines"
_PROMPTS_DIR = PACKAGE_ROOT / "prompts"


def _bundled_pipeline_names() -> list[str]:
    return sorted(p.stem for p in _PIPELINES_DIR.glob("*.yaml"))


_BUNDLED_PREFIX = "gremlins:"


def _strip_bundled_prefix(name: str) -> str:
    return name[len(_BUNDLED_PREFIX) :] if name.startswith(_BUNDLED_PREFIX) else name


def _rewrite_prompts_to_bare(stages: list[Any], named_keys: set[str]) -> None:
    """Strip the `gremlins:` prefix on every `prompt:` entry in-place.

    After init, scaffolded YAMLs reference editable local copies under
    `.gremlins/prompts/`, so bundled-prefixed names become bare names that
    resolve against `prompt_dir`. Named-entry references (keys in `named_keys`)
    are left unchanged.
    """
    for stage in stages:
        if not isinstance(stage, dict):
            continue
        s = cast(dict[str, Any], stage)
        if "parallel" in s:
            _rewrite_prompts_to_bare(cast(list[Any], s["parallel"]), named_keys)
            continue
        prompts = s.get("prompt")
        if not prompts:
            continue
        if isinstance(prompts, str):
            s["prompt"] = (
                prompts if prompts in named_keys else _strip_bundled_prefix(prompts)
            )
        else:
            s["prompt"] = [
                p if p in named_keys else _strip_bundled_prefix(p)
                for p in cast(list[str], prompts)
            ]


def _rewrite_named_prompts_to_bare(named_prompts: dict[str, Any]) -> None:
    """Strip `gremlins:` prefix from values in the top-level prompts: mapping."""
    for key in named_prompts:
        val = named_prompts[key]
        if isinstance(val, str):
            named_prompts[key] = _strip_bundled_prefix(val)
        else:
            named_prompts[key] = [
                _strip_bundled_prefix(p) for p in cast(list[str], val)
            ]


def _collect_prompt_subpaths(
    stages: list[Any], named_prompts: dict[str, Any] | None = None
) -> list[str]:
    """Return unique subpaths for all prompt files.

    Collects from the named prompts mapping (if any) and from stage `prompt:`
    fields, skipping stage items that are named-entry references.
    """
    seen: set[str] = set()
    result: list[str] = []

    def _add(p: str) -> None:
        bare = _strip_bundled_prefix(p)
        if bare not in seen:
            seen.add(bare)
            result.append(bare)

    named_keys: set[str] = set()
    for key, value in (named_prompts or {}).items():
        named_keys.add(key)
        if isinstance(value, str):
            _add(value)
        else:
            for p in cast(list[str], value):
                _add(p)

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
            if p not in named_keys:
                _add(p)

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


def _tmp_path(dst: pathlib.Path) -> pathlib.Path:
    return dst.with_suffix(dst.suffix + f".tmp.{os.getpid()}")


def _build_plan(
    selected: list[str], base: pathlib.Path
) -> list[tuple[pathlib.Path, bytes]]:
    dot_gremlins = base / ".gremlins"
    plan: list[tuple[pathlib.Path, bytes]] = []

    pipeline_data: dict[str, dict[str, Any]] = {}
    for name in selected:
        pipeline_data[name] = load_yaml_file(_PIPELINES_DIR / f"{name}.yaml")

    seen_subpaths: set[str] = set()
    for name in selected:
        raw = pipeline_data[name]
        named = cast(dict[str, Any], raw.get("prompts") or {})
        for subpath in _collect_prompt_subpaths(raw.get("stages", []), named):
            if subpath not in seen_subpaths:
                seen_subpaths.add(subpath)
                src = _PROMPTS_DIR / subpath
                dst = dot_gremlins / "prompts" / subpath
                plan.append((dst, src.read_bytes()))

    for name in selected:
        data = dict(pipeline_data[name])
        data.setdefault("prompt_dir", "prompts")
        stages = cast(list[Any], data.get("stages", []))
        named = cast(dict[str, Any], data.get("prompts") or {})
        named_keys = set(named)
        _rewrite_prompts_to_bare(stages, named_keys)
        if named:
            _rewrite_named_prompts_to_bare(named)
        dst = dot_gremlins / f"{name}.yaml"
        content = dump_yaml_text(data)
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
    except (OSError, YamlLoadError, ValueError) as exc:
        sys.stderr.write(f"error: {str(exc).splitlines()[0]}\n")
        _cleanup_tmp([_tmp_path(dst) for dst, _ in plan])
        return 1
    return 0
