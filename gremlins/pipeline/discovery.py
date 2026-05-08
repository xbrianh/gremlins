from __future__ import annotations

import os
import pathlib

from gremlins import PACKAGE_ROOT

BUNDLED_PIPELINE_DIR = PACKAGE_ROOT / "pipelines"


def overlay_dir(project_root: pathlib.Path) -> pathlib.Path:
    overlay = os.environ.get("GREMLINS_OVERLAY_DIR", "")
    if overlay:
        return pathlib.Path(overlay)
    return project_root / ".gremlins"


def _project_pipeline_dirs(project_root: pathlib.Path) -> list[pathlib.Path]:
    """Return project pipeline dirs to search, deduped, overlay first."""
    dirs: list[pathlib.Path] = []
    seen: set[pathlib.Path] = set()
    for d in [
        overlay_dir(project_root),
        project_root / ".gremlins",
    ]:
        resolved = d.resolve()
        if resolved not in seen:
            dirs.append(d)
            seen.add(resolved)
    return dirs


def list_pipelines(project_root: pathlib.Path) -> list[tuple[str, pathlib.Path]]:
    """Return (name, path) pairs for all resolvable pipelines, project-local first."""
    results: list[tuple[str, pathlib.Path]] = []
    seen: set[str] = set()

    for local_dir in _project_pipeline_dirs(project_root):
        if local_dir.exists():
            for p in sorted(local_dir.glob("*.yaml")):
                if p.stem not in seen:
                    results.append((p.stem, p.resolve()))
                    seen.add(p.stem)

    for p in sorted(BUNDLED_PIPELINE_DIR.glob("*.yaml")):
        if p.stem not in seen:
            results.append((p.stem, p.resolve()))

    return results


def resolve_pipeline_name(name: str, project_root: pathlib.Path) -> pathlib.Path:
    dirs = _project_pipeline_dirs(project_root)
    for d in dirs:
        candidate = d / f"{name}.yaml"
        if candidate.exists():
            return candidate.resolve()
    bundled = BUNDLED_PIPELINE_DIR / f"{name}.yaml"
    if bundled.exists():
        return bundled.resolve()
    names: list[str] = []
    for d in dirs:
        if d.exists():
            names += sorted(p.stem for p in d.glob("*.yaml"))
    names += sorted(p.stem for p in BUNDLED_PIPELINE_DIR.glob("*.yaml"))
    available = list(dict.fromkeys(names))
    raise FileNotFoundError(
        f"pipeline {name!r} not found; available: {', '.join(available) or '(none)'}"
    )


def resolve_pipeline_path(name_or_path: str, base_dir: pathlib.Path) -> pathlib.Path:
    candidate = pathlib.Path(name_or_path)
    if candidate.suffix == ".yaml" or len(candidate.parts) > 1:
        resolved = candidate.resolve()
        if not resolved.exists():
            raise FileNotFoundError(f"pipeline file not found: {resolved}")
        return resolved
    for d in _project_pipeline_dirs(base_dir):
        project_scoped = d / f"{name_or_path}.yaml"
        if project_scoped.exists():
            return project_scoped.resolve()
    bundled = BUNDLED_PIPELINE_DIR / f"{name_or_path}.yaml"
    if bundled.exists():
        return bundled.resolve()
    dirs = _project_pipeline_dirs(base_dir)
    dirs_str = " or ".join(str(d) for d in dirs)
    raise FileNotFoundError(
        f"pipeline {name_or_path!r} not found in {dirs_str} or bundled pipelines"
    )
