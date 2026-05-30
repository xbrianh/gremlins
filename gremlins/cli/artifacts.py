from __future__ import annotations

import argparse
import sys
from typing import Any

from gremlins.artifacts.registry import ArtifactRegistry
from gremlins.paths import project_root, state_root
from gremlins.pipeline import Pipeline
from gremlins.pipeline.discovery import resolve_pipeline_name
from gremlins.utils.yaml_io import YamlLoadError


def artifacts_main(argv: list[str]) -> int:
    p = argparse.ArgumentParser(prog="gremlins artifacts")
    p.add_argument("target")
    args = p.parse_args(argv)
    target = args.target
    gdir = state_root() / target
    if gdir.exists():
        sdir = gdir / "artifacts"
        reg = ArtifactRegistry(session_dir=sdir)
        _print_live(reg)
        return 0
    try:
        ppath = resolve_pipeline_name(target, project_root())
        pipe = Pipeline.from_yaml(ppath)
        _print_static(pipe)
        return 0
    except (FileNotFoundError, ValueError, YamlLoadError) as exc:
        sys.stderr.write(f"error: {exc}\n")
        return 1


def _print_static(pipe: Pipeline) -> None:
    info: dict[str, dict[str, list[str]]] = {}
    _walk(pipe.stages, info)
    if pipe.land is not None:
        _walk([pipe.land], info)
    print(f"static:{pipe.name}")
    for k in sorted(info):
        d = info[k]
        uri = d["uri"]
        sch = d["scheme"]
        ps = ",".join(d["producers"]) or "-"
        cs = ",".join(d["consumers"]) or "-"
        print(f"  {k} {uri}({sch}) p={ps} c={cs}")


def _walk(stages: list, info: dict[str, dict[str, list[str]]]) -> None:
    for st in stages:
        _collect(st, info)
        body = getattr(st, "body", None) or []
        if body:
            _walk(body, info)


def _collect(st: Any, info: dict[str, dict[str, list[str]]]) -> None:
    nm = getattr(st, "name", "?")
    for k, u in getattr(st, "out_map", {}).items():
        if k.endswith("?"):
            continue
        if k not in info:
            info[k] = {"uri": u, "scheme": _sch(u), "producers": [], "consumers": []}
        d = info[k]
        if d["uri"] == "?":
            d["uri"] = u
            d["scheme"] = _sch(u)
        if nm not in d["producers"]:
            d["producers"].append(nm)
    for ref in getattr(st, "in_map", {}).values():
        k = ref.split("?", 1)[0].split(".", 1)[0]
        if not k:
            continue
        if k not in info:
            info[k] = {"uri": "?", "scheme": "?", "producers": [], "consumers": []}
        d = info[k]
        if nm not in d["consumers"]:
            d["consumers"].append(nm)


def _sch(u: str) -> str:
    return u.split("://", 1)[0] if "://" in u else "?"


def _print_live(reg: ArtifactRegistry) -> None:
    print(f"live:{reg.registry_path}")
    for k in sorted(reg._data):
        v = reg._data[k]
        sch = v.split("://", 1)[0] if isinstance(v, str) and "://" in v else "?"
        print(f"  {k} {v}({sch})")
