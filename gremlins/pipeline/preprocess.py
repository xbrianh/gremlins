from __future__ import annotations

import pathlib
from typing import Any, cast

from gremlins.pipeline import GREMLINS_PREFIX
from gremlins.pipeline.discovery import resolve_pipeline_name
from gremlins.prompts import BUNDLED_PROMPT_DIR
from gremlins.recipes import BUNDLED_STAGE_DEF_DIR
from gremlins.utils.yaml_io import load_yaml_file


def expand_pipeline(
    yaml_path: pathlib.Path, project_root: pathlib.Path | None = None
) -> dict[str, Any]:
    """Read YAML, resolve all include: and prompt: references, return self-contained dict."""
    if project_root is None:
        d = yaml_path.parent
        project_root = d.parent if d.name == ".gremlins" else d
    return _expand(yaml_path, project_root, chain=[])


def _expand(
    yaml_path: pathlib.Path,
    project_root: pathlib.Path,
    chain: list[pathlib.Path],
) -> dict[str, Any]:
    resolved = yaml_path.resolve()
    if resolved in chain:
        cycle = " -> ".join(str(p) for p in chain + [resolved])
        raise ValueError(f"include cycle detected: {cycle}")

    raw = load_yaml_file(yaml_path)
    if raw.get("__gremlins_expanded__"):
        result = dict(raw)
        result.pop("__gremlins_expanded__", None)
        return result
    yaml_dir = yaml_path.parent
    prompt_dir = _resolve_prompt_dir(raw.get("prompt_dir"), yaml_dir)
    new_chain = chain + [resolved]

    named_prompts: dict[str, list[str]] = {}
    for name, value in cast(dict[str, Any], raw.get("prompts") or {}).items():
        # Named prompts can't reference each other — pass empty dict to prevent cycles.
        named_prompts[name] = _read_prompts(value, prompt_dir, {})

    raw_stage_defs = raw.get("stage-definitions")
    if raw_stage_defs is not None and not isinstance(raw_stage_defs, dict):
        raise ValueError(
            f"stage-definitions must be a mapping, got {type(raw_stage_defs).__name__!r}"
        )
    stage_defs: dict[str, dict[str, Any]] = {}
    for name, defn in cast(dict[str, Any], raw_stage_defs or {}).items():
        if isinstance(defn, str) and defn.startswith(GREMLINS_PREFIX):
            recipe_name = defn[len(GREMLINS_PREFIX) :]
            if not recipe_name:
                raise ValueError(
                    f"stage-definition {name!r}: missing name after {GREMLINS_PREFIX!r}"
                )
            recipe_path = (BUNDLED_STAGE_DEF_DIR / f"{recipe_name}.yaml").resolve()
            if not recipe_path.is_relative_to(BUNDLED_STAGE_DEF_DIR.resolve()):
                raise ValueError(
                    f"stage-definition {name!r}: invalid recipe name {recipe_name!r}"
                )
            if not recipe_path.exists():
                raise FileNotFoundError(f"bundled stage-definition not found: {defn!r}")
            stage_defs[name] = load_yaml_file(recipe_path)
        elif not isinstance(defn, dict):
            raise ValueError(
                f"stage-definition {name!r} must be a dict or gremlins: reference"
            )
        else:
            stage_defs[name] = cast(dict[str, Any], defn)

    expanded_stages: list[dict[str, Any]] = []
    for entry in cast(list[dict[str, Any]], raw.get("stages") or []):
        expanded_stages.extend(
            _expand_entry(
                entry, prompt_dir, project_root, new_chain, named_prompts, stage_defs
            )
        )

    result: dict[str, Any] = {
        k: v
        for k, v in raw.items()
        if k not in ("stages", "prompt_dir", "prompts", "stage-definitions")
    }
    result["stages"] = expanded_stages
    return result


def _expand_entry(
    entry: dict[str, Any],
    prompt_dir: pathlib.Path,
    project_root: pathlib.Path,
    chain: list[pathlib.Path],
    named_prompts: dict[str, list[str]],
    stage_defs: dict[str, dict[str, Any]],
    seen_defs: frozenset[str] = frozenset(),
) -> list[dict[str, Any]]:
    if "include" in entry and len(entry) == 1:
        name = entry["include"]
        if not isinstance(name, str) or not name:
            raise ValueError("include: value must be a non-empty string")
        included_path = resolve_pipeline_name(name, project_root)
        included = _expand(included_path, project_root, chain)
        return cast(list[dict[str, Any]], included.get("stages") or [])

    stage_type = entry.get("type")
    if isinstance(stage_type, str) and stage_type:
        if stage_type in stage_defs:
            return _expand_stage_def(
                entry,
                stage_type,
                stage_defs,
                prompt_dir,
                project_root,
                chain,
                named_prompts,
                seen_defs,
            )
        # Auto-resolve bundled stage-definitions by type name (hyphens → underscores).
        recipe_path = BUNDLED_STAGE_DEF_DIR / f"{stage_type.replace('-', '_')}.yaml"
        if recipe_path.exists():
            auto_defs = {**stage_defs, stage_type: load_yaml_file(recipe_path)}
            return _expand_stage_def(
                entry,
                stage_type,
                auto_defs,
                prompt_dir,
                project_root,
                chain,
                named_prompts,
                seen_defs,
            )
        try:
            included_path = resolve_pipeline_name(stage_type, project_root)
        except FileNotFoundError:
            pass
        else:
            # Skip if the path is already in chain — the pipeline is referencing
            # its own name as a type, which falls through to loader.py validation.
            if included_path not in chain:
                included = _expand(included_path, project_root, chain)
                return cast(list[dict[str, Any]], included.get("stages") or [])

    entry = dict(entry)

    if "prompt" in entry:
        entry["prompt"] = _read_prompts(entry["prompt"], prompt_dir, named_prompts)

    if "parallel" in entry and isinstance(entry["parallel"], list):
        expanded_parallel: list[dict[str, Any]] = []
        for child in cast(list[Any], entry["parallel"]):
            child_dict = cast(dict[str, Any], child)
            include_name = child_dict.get("include") if len(child_dict) == 1 else None
            expanded = _expand_entry(
                child_dict,
                prompt_dir,
                project_root,
                chain,
                named_prompts,
                stage_defs,
                seen_defs,
            )
            if len(expanded) == 0:
                raise ValueError(
                    "parallel child expanded to 0 stages via include; "
                    "includes inside parallel groups must resolve to at least one stage"
                )
            if len(expanded) == 1:
                expanded_parallel.append(expanded[0])
            else:
                name = include_name or f"sequence-{len(expanded_parallel)}"
                expanded_parallel.append(
                    {"name": name, "type": "sequence", "body": expanded}
                )
        entry["parallel"] = expanded_parallel

    if "body" in entry and isinstance(entry["body"], list):
        expanded_body: list[dict[str, Any]] = []
        for body_entry in cast(list[dict[str, Any]], entry["body"]):
            expanded_body.extend(
                _expand_entry(
                    body_entry,
                    prompt_dir,
                    project_root,
                    chain,
                    named_prompts,
                    stage_defs,
                    seen_defs,
                )
            )
        entry["body"] = expanded_body

    return [entry]


def _expand_stage_def(
    call_site: dict[str, Any],
    def_name: str,
    stage_defs: dict[str, dict[str, Any]],
    prompt_dir: pathlib.Path,
    project_root: pathlib.Path,
    chain: list[pathlib.Path],
    named_prompts: dict[str, list[str]],
    seen_defs: frozenset[str] = frozenset(),
) -> list[dict[str, Any]]:
    if def_name in seen_defs:
        raise ValueError(f"stage-definition cycle: {def_name!r}")
    definition = stage_defs[def_name]
    new_seen = seen_defs | {def_name}

    inner_list = definition.get("stages")
    if inner_list is not None:
        if not isinstance(inner_list, list) or not inner_list:
            raise ValueError(
                f"stage-definition {def_name!r}: 'stages' must be a non-empty list"
            )
        if "out" in definition:
            raise ValueError(
                f"stage-definition {def_name!r} must not declare 'out:' keys; "
                "declare them at each call site instead"
            )
        inner_list = cast(list[dict[str, Any]], inner_list)
        last_idx = len(inner_list) - 1
        result: list[dict[str, Any]] = []
        for i, raw_inner in enumerate(inner_list):
            inner = dict(raw_inner)
            if i == 0:
                for key in ("name", "client"):
                    if key in call_site:
                        inner[key] = call_site[key]
                if "prompt" in call_site:
                    recipe_prompts: Any = inner.get("prompt") or []
                    cs_prompts = call_site["prompt"]
                    if not isinstance(recipe_prompts, list):
                        recipe_prompts = [recipe_prompts] if recipe_prompts else []
                    if not isinstance(cs_prompts, list):
                        cs_prompts = [cs_prompts]
                    # Call-site prompts first so recipe's closing instructions remain last.
                    inner["prompt"] = cs_prompts + recipe_prompts
                if "in" in call_site:
                    merged_in = dict(cast(dict[str, Any], inner.get("in") or {}))
                    merged_in.update(cast(dict[str, Any], call_site["in"]))
                    inner["in"] = merged_in
            if i == last_idx and "out" in call_site:
                if "out" in inner:
                    raise ValueError(
                        f"stage-definition {def_name!r}: inner stage {i} declares 'out:'; "
                        "call-site must not also declare 'out:'"
                    )
                inner["out"] = call_site["out"]
            result.extend(
                _expand_entry(
                    inner,
                    prompt_dir,
                    project_root,
                    chain,
                    named_prompts,
                    stage_defs,
                    new_seen,
                )
            )
        return result

    # Single-primitive definition (existing behavior)
    if "out" in definition:
        raise ValueError(
            f"stage-definition {def_name!r} must not declare 'out:' keys; "
            "declare them at each call site instead"
        )
    merged = dict(definition)
    for key in ("name", "in", "out"):
        if key in call_site:
            merged[key] = call_site[key]
    return _expand_entry(
        merged, prompt_dir, project_root, chain, named_prompts, stage_defs, new_seen
    )


def _resolve_prompt_dir(value: object, yaml_dir: pathlib.Path) -> pathlib.Path:
    if value is None:
        return yaml_dir
    if not isinstance(value, str):
        raise ValueError(f"prompt_dir must be a string, got {type(value)!r}")
    return (yaml_dir / value).resolve()


def _read_prompt_file(path: pathlib.Path) -> str:
    if not path.exists():
        raise FileNotFoundError(f"prompt file not found: {path}")
    text = path.read_text(encoding="utf-8")
    if not text.strip():
        raise ValueError(f"prompt file is empty: {path}")
    return text


def _read_prompts(
    prompt_field: object,
    prompt_dir: pathlib.Path,
    named_prompts: dict[str, list[str]],
) -> list[str]:
    if isinstance(prompt_field, str):
        raw: list[str] = [prompt_field]
    elif isinstance(prompt_field, list):
        raw = [str(item) for item in cast(list[Any], prompt_field)]
    else:
        raise ValueError(f"prompt must be a string or list, got {type(prompt_field)!r}")

    texts: list[str] = []
    for p in raw:
        if p in named_prompts:
            texts.extend(named_prompts[p])
        elif p.startswith(GREMLINS_PREFIX):
            name = p[len(GREMLINS_PREFIX) :]
            if not name:
                raise ValueError(
                    f"prompt {p!r} is missing a name after {GREMLINS_PREFIX!r}"
                )
            texts.append(_read_prompt_file((BUNDLED_PROMPT_DIR / name).resolve()))
        elif "\n" in p:
            # Inline text. YAML block scalars always include a trailing \n,
            # so this heuristic is reliable for recipe-sourced content.
            texts.append(p)
        else:
            path = (prompt_dir / p).resolve()
            if not path.exists() and named_prompts:
                raise FileNotFoundError(
                    f"prompt {p!r} not found as a named entry or file under {prompt_dir}"
                )
            texts.append(_read_prompt_file(path))

    return texts
