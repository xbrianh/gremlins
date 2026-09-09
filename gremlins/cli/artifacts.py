from __future__ import annotations

import argparse
import pathlib
import sys
from typing import Any

from _gremlins_core.artifacts import ArtifactRegistry, Uri
from _gremlins_core.config import scratch_root, state_root
from _gremlins_core.discovery import resolve_pipeline_name
from _gremlins_core.schemas import Pipeline

from gremlins.utils.yaml_io import YamlLoadError


def artifacts_main(argv: list[str]) -> int:
    p = argparse.ArgumentParser(prog="gremlins artifacts")
    p.add_argument("target")
    args = p.parse_args(argv)
    target = args.target
    gdir = pathlib.Path(state_root()) / target
    if gdir.exists():
        sdir = pathlib.Path(scratch_root(target)) / "artifacts"
        if sdir.exists():
            reg = ArtifactRegistry(artifact_dir=sdir)
            _print_live(reg)
            return 0
    try:
        ppath = resolve_pipeline_name(target)
        pipe = Pipeline.from_yaml(ppath)
        _print_static(pipe)
        return 0
    except (FileNotFoundError, ValueError, YamlLoadError) as exc:
        sys.stderr.write(f"error: {exc}\n")
        return 1


def _print_static(pipe: Pipeline) -> None:
    info: dict[str, dict[str, Any]] = {}
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


def _walk(stages: list[Any], info: dict[str, dict[str, Any]]) -> None:
    for st in stages:
        _collect(st, info)
        body: list[Any] = getattr(st, "body", None) or []
        if body:
            _walk(body, info)


def _collect(st: Any, info: dict[str, dict[str, Any]]) -> None:
    nm = getattr(st, "name", "?")
    for k, u in getattr(st, "bind_map", {}).items():
        k = k[:-1] if k.endswith("?") else k
        uri = Uri.parse_or_none(u)
        sch = uri.scheme if uri else "?"
        if k not in info:
            info[k] = {
                "uri": u,
                "scheme": sch,
                "producers": [],
                "consumers": [],
            }
        d = info[k]
        if d.get("uri") == "?":
            d["uri"] = u
            d["scheme"] = sch
        if nm not in d["producers"]:
            d["producers"].append(nm)
    for ref in getattr(st, "interpolation_map", {}).values():
        k = ref.split("?", 1)[0].split(".", 1)[0]
        if not k:
            continue
        if k not in info:
            info[k] = {"uri": "?", "scheme": "?", "producers": [], "consumers": []}
        d = info[k]
        if nm not in d["consumers"]:
            d["consumers"].append(nm)


def _print_live(reg: ArtifactRegistry) -> None:
    rpath = reg.registry_path
    print(f"live:{rpath}")
    for k in sorted(reg.keys()):
        v = reg.data_uri(k)
        uri = Uri.parse_or_none(v)
        sch = uri.scheme if uri else "?"
        print(f"  {k} {v}({sch})")
