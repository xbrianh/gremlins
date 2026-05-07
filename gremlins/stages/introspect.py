"""Stage parameter introspection and argparse builder."""

from __future__ import annotations

import argparse
import inspect
import pathlib
import types
import typing
from collections.abc import Callable
from dataclasses import dataclass
from typing import Any

SKIP_PARAMS = frozenset({"self", "entry", "model"})
SUPPORTED_TYPES: tuple[type, ...] = (str, int, float, bool, pathlib.Path)

_INFRA_FLAGS = frozenset(
    {"description", "parent", "print-id", "base-ref", "client", "spec", "plan"}
)


@dataclass
class ArgSpec:
    name: str
    type: type
    required: bool
    default: Any
    help: str


def _is_callable(annotation: Any) -> bool:
    if annotation is Callable:
        return True
    origin = typing.get_origin(annotation)
    if origin is Callable:
        return True
    for arg in typing.get_args(annotation):
        if _is_callable(arg):
            return True
    return False


def _resolve_type(annotation: Any, param_name: str) -> type:
    if annotation in SUPPORTED_TYPES:
        return annotation
    origin = typing.get_origin(annotation)
    args = typing.get_args(annotation)
    is_union = origin is typing.Union or isinstance(annotation, types.UnionType)
    if is_union and args:
        non_none = [a for a in args if a is not type(None)]
        if len(non_none) == 1:
            return _resolve_type(non_none[0], param_name)
    raise TypeError(
        f"param {param_name!r} has unsupported annotation: {annotation!r}"
    )


def _annotation_str(annotation: Any) -> str:
    if isinstance(annotation, type):
        if annotation is pathlib.Path:
            return "pathlib.Path"
        if annotation is type(None):
            return "None"
        return annotation.__name__
    origin = typing.get_origin(annotation)
    args = typing.get_args(annotation)
    is_union = origin is typing.Union or isinstance(annotation, types.UnionType)
    if is_union and args:
        return " | ".join(_annotation_str(a) for a in args)
    return str(annotation)


def _parse_bool(v: str) -> bool:
    if v.lower() in ("1", "true", "yes"):
        return True
    if v.lower() in ("0", "false", "no"):
        return False
    raise argparse.ArgumentTypeError(f"invalid bool value: {v!r}")


def stage_argspecs(stage_cls: type) -> list[ArgSpec]:
    try:
        hints = typing.get_type_hints(stage_cls.__init__)
    except Exception:
        hints = {}
    sig = inspect.signature(stage_cls.__init__)
    specs = []
    for name, param in sig.parameters.items():
        if name in SKIP_PARAMS:
            continue
        annotation = hints.get(name, param.annotation)
        if annotation is inspect.Parameter.empty:
            raise TypeError(f"param {name!r} has no annotation")
        if _is_callable(annotation):
            continue
        resolved = _resolve_type(annotation, name)
        required = param.default is inspect.Parameter.empty
        default = None if required else param.default
        specs.append(
            ArgSpec(
                name=name,
                type=resolved,
                required=required,
                default=default,
                help=_annotation_str(annotation),
            )
        )
    return specs


def build_launch_parser(
    pipeline_name: str, stage_cls: type
) -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(prog=f"gremlins launch {pipeline_name}")
    p.add_argument("--description", default=None)
    p.add_argument("--parent", dest="parent_id", default=None)
    p.add_argument("--print-id", action="store_true")
    p.add_argument("--base-ref", default="HEAD")
    p.add_argument("--client", default=None)
    p.add_argument("--spec", dest="spec_path", default=None)
    p.add_argument("--plan", default=None)

    for spec in stage_argspecs(stage_cls):
        flag = "--" + spec.name.replace("_", "-")
        flag_key = flag.lstrip("-")
        if flag_key in _INFRA_FLAGS:
            raise ValueError(
                f"stage param {spec.name!r} conflicts with infra flag {flag!r}"
            )
        kwargs: dict[str, Any] = {"help": spec.help}
        if spec.type is bool:
            kwargs["type"] = _parse_bool
        else:
            kwargs["type"] = spec.type
        if spec.required:
            kwargs["required"] = True
        else:
            kwargs["default"] = spec.default
        p.add_argument(flag, **kwargs)

    return p
