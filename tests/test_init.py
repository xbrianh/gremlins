"""Tests for gremlins/init.py."""

from __future__ import annotations

import pathlib

import pytest
import yaml

from gremlins.init import _bundled_pipeline_names, init_main


# ---------------------------------------------------------------------------
# Happy path: all pipelines
# ---------------------------------------------------------------------------


def test_init_all_pipelines(tmp_path, capsys):
    rc = init_main(["--path", str(tmp_path)])
    assert rc == 0

    dot = tmp_path / ".gremlins"
    out = capsys.readouterr().out

    bundled = _bundled_pipeline_names()
    for name in bundled:
        dst = dot / "pipelines" / f"{name}.yaml"
        assert dst.exists(), f"missing {dst}"
        assert str(dst) in out

        data = yaml.safe_load(dst.read_text())
        for stage in data.get("stages", []):
            if not isinstance(stage, dict):
                continue
            prompts = stage.get("prompt")
            if prompts is None:
                continue
            if isinstance(prompts, str):
                prompts = [prompts]
            for p in prompts:
                assert p.startswith("../prompts/"), f"prompt not rewritten: {p}"

    assert (dot / "prompts").is_dir()
    for p in out.splitlines():
        assert pathlib.Path(p).exists(), f"listed but missing: {p}"


# ---------------------------------------------------------------------------
# --pipeline local: only local.yaml and its prompts
# ---------------------------------------------------------------------------


def test_init_single_pipeline(tmp_path, capsys):
    rc = init_main(["--path", str(tmp_path), "--pipeline", "local"])
    assert rc == 0

    dot = tmp_path / ".gremlins"
    assert (dot / "pipelines" / "local.yaml").exists()
    assert not (dot / "pipelines" / "gh.yaml").exists()

    data = yaml.safe_load((dot / "pipelines" / "local.yaml").read_text())
    prompt_refs = set()
    for stage in data["stages"]:
        if not isinstance(stage, dict):
            continue
        prompts = stage.get("prompt")
        if not prompts:
            continue
        if isinstance(prompts, str):
            prompts = [prompts]
        prompt_refs.update(prompts)

    for ref in prompt_refs:
        assert ref.startswith("../prompts/")
        subpath = ref[len("../prompts/"):]
        assert (dot / "prompts" / subpath).exists()


# ---------------------------------------------------------------------------
# Conflict detection: exit 1, nothing written
# ---------------------------------------------------------------------------


def test_init_conflict_exits_without_writing(tmp_path, capsys):
    conflict = tmp_path / ".gremlins" / "pipelines" / "local.yaml"
    conflict.parent.mkdir(parents=True)
    conflict.write_text("existing", encoding="utf-8")

    rc = init_main(["--path", str(tmp_path), "--pipeline", "local"])
    assert rc == 1

    err = capsys.readouterr().err
    assert "already exists" in err

    # Nothing else written
    written = list((tmp_path / ".gremlins").rglob("*"))
    assert written == [
        conflict.parent,
        conflict,
    ] or all(p == conflict or p == conflict.parent for p in written)
    assert conflict.read_text(encoding="utf-8") == "existing"


# ---------------------------------------------------------------------------
# --force overwrites
# ---------------------------------------------------------------------------


def test_init_force_overwrites(tmp_path, capsys):
    conflict = tmp_path / ".gremlins" / "pipelines" / "local.yaml"
    conflict.parent.mkdir(parents=True)
    conflict.write_text("old content", encoding="utf-8")

    rc = init_main(["--path", str(tmp_path), "--pipeline", "local", "--force"])
    assert rc == 0

    data = yaml.safe_load(conflict.read_text())
    assert data.get("name") == "local"


# ---------------------------------------------------------------------------
# Unknown pipeline name exits 1 and lists bundled names
# ---------------------------------------------------------------------------


def test_init_unknown_pipeline(tmp_path, capsys):
    rc = init_main(["--path", str(tmp_path), "--pipeline", "nonexistent"])
    assert rc == 1

    err = capsys.readouterr().err
    assert "nonexistent" in err
    for name in _bundled_pipeline_names():
        assert name in err


# ---------------------------------------------------------------------------
# --path scaffolds under the given directory
# ---------------------------------------------------------------------------


def test_init_custom_path(tmp_path, capsys):
    target = tmp_path / "some" / "dir"
    rc = init_main(["--path", str(target), "--pipeline", "local"])
    assert rc == 0

    assert (target / ".gremlins" / "pipelines" / "local.yaml").exists()
