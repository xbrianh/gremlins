"""Tests for gremlins/init.py."""

from __future__ import annotations

import os
import pathlib
from collections.abc import Iterator
from typing import Any

import pytest
import yaml

from gremlins.init import _bundled_pipeline_names, init_main


def _iter_stage_prompts(
    stages: list[Any], named_keys: set[str] | None = None
) -> Iterator[str]:
    named = named_keys or set()
    for stage in stages:
        if not isinstance(stage, dict):
            continue
        if "parallel" in stage:
            yield from _iter_stage_prompts(stage["parallel"], named)
        else:
            prompts = stage.get("prompt")
            if not prompts:
                continue
            if isinstance(prompts, str):
                prompts = [prompts]
            for p in prompts:
                if p not in named:
                    yield p


def _iter_named_prompt_values(named_prompts: dict[str, Any]) -> Iterator[str]:
    for value in named_prompts.values():
        if isinstance(value, str):
            yield value
        else:
            yield from value


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
        dst = dot / f"{name}.yaml"
        assert dst.exists(), f"missing {dst}"
        assert str(dst) in out

        data = yaml.safe_load(dst.read_text())
        assert data.get("prompt_dir") == "prompts", f"prompt_dir not injected in {dst}"
        named = data.get("prompts") or {}
        named_keys = set(named)
        for p in _iter_stage_prompts(data.get("stages", []), named_keys):
            assert "/" not in p or p.startswith("review/"), (
                f"prompt should be a bare name (or review/<lens>): {p}"
            )
        for p in _iter_named_prompt_values(named):
            assert "/" not in p or p.startswith("review/"), (
                f"named prompt value should be a bare name (or review/<lens>): {p}"
            )

    agents_md = tmp_path / "AGENTS.md"
    assert agents_md.exists()
    assert str(agents_md) in out

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
    assert (dot / "local.yaml").exists()
    assert not (dot / "gh.yaml").exists()

    data = yaml.safe_load((dot / "local.yaml").read_text())
    assert data["prompt_dir"] == "prompts"
    named = data.get("prompts") or {}
    named_keys = set(named)

    for ref in _iter_stage_prompts(data["stages"], named_keys):
        assert (dot / "prompts" / ref).exists(), f"missing prompt file: {ref}"
    for ref in _iter_named_prompt_values(named):
        assert (dot / "prompts" / ref).exists(), f"missing named prompt file: {ref}"


# ---------------------------------------------------------------------------
# Conflict detection: exit 1, nothing written
# ---------------------------------------------------------------------------


def test_init_conflict_exits_without_writing(tmp_path, capsys):
    conflict = tmp_path / ".gremlins" / "local.yaml"
    conflict.parent.mkdir(parents=True)
    conflict.write_text("existing", encoding="utf-8")

    rc = init_main(["--path", str(tmp_path), "--pipeline", "local"])
    assert rc == 1

    err = capsys.readouterr().err
    assert "already exists" in err

    # Nothing else written
    written = list((tmp_path / ".gremlins").rglob("*"))
    assert set(written) == {conflict}
    assert conflict.read_text(encoding="utf-8") == "existing"


# ---------------------------------------------------------------------------
# --force overwrites
# ---------------------------------------------------------------------------


def test_init_force_overwrites(tmp_path, capsys):
    conflict = tmp_path / ".gremlins" / "local.yaml"
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

    assert (target / ".gremlins" / "local.yaml").exists()


# ---------------------------------------------------------------------------
# Error handling: write failure mid-stage
# ---------------------------------------------------------------------------


def test_write_failure_mid_stage(tmp_path, capsys, monkeypatch):
    call_count = 0
    original = pathlib.Path.write_bytes

    def patched(self, data):
        nonlocal call_count
        call_count += 1
        if call_count == 2:
            raise OSError("disk full")
        return original(self, data)

    monkeypatch.setattr(pathlib.Path, "write_bytes", patched)
    rc = init_main(["--path", str(tmp_path), "--pipeline", "local"])
    assert rc == 1

    err = capsys.readouterr().err
    assert err.startswith("error:")
    assert err.count("\n") == 1

    assert list(tmp_path.rglob("*.tmp.*")) == []
    dot = tmp_path / ".gremlins"
    if dot.exists():
        assert [f for f in dot.rglob("*") if f.is_file()] == []


# ---------------------------------------------------------------------------
# Error handling: corrupt bundled YAML
# ---------------------------------------------------------------------------


def test_corrupt_bundled_yaml(tmp_path, capsys, monkeypatch):
    import gremlins.init as init_mod

    fake_dir = tmp_path / "pipelines"
    fake_dir.mkdir()
    (fake_dir / "local.yaml").write_text("key: [unclosed", encoding="utf-8")
    monkeypatch.setattr(init_mod, "_PIPELINES_DIR", fake_dir)

    out_path = tmp_path / "out"
    out_path.mkdir()
    rc = init_main(["--path", str(out_path), "--pipeline", "local"])
    assert rc == 1

    err = capsys.readouterr().err
    assert err.startswith("error:")
    assert err.count("\n") == 1
    assert "local.yaml" in err
    assert "YAML parse error" in err
    assert "line " in err
    assert "column " in err
    assert not (out_path / ".gremlins").exists()


# ---------------------------------------------------------------------------
# Error handling: unwritable --path
# ---------------------------------------------------------------------------


def test_unwritable_path(tmp_path, capsys):
    if os.getuid() == 0:
        pytest.skip("running as root; chmod has no effect")

    ro = tmp_path / "readonly"
    ro.mkdir()
    os.chmod(ro, 0o500)
    try:
        rc = init_main(["--path", str(ro), "--pipeline", "local"])
        assert rc == 1
        err = capsys.readouterr().err
        assert err.startswith("error:")
        assert err.count("\n") == 1
        assert list(tmp_path.rglob("*.tmp.*")) == []
    finally:
        os.chmod(ro, 0o700)


def test_init_writes_gremlins_gitignore(tmp_path, capsys):
    rc = init_main(["--path", str(tmp_path), "--pipeline", "local"])
    assert rc == 0
    gitignore = tmp_path / ".gremlins" / ".gitignore"
    assert gitignore.exists()
    assert "env" in gitignore.read_text(encoding="utf-8")


# ---------------------------------------------------------------------------
# Error handling: commit-phase rename failure
# ---------------------------------------------------------------------------


def test_commit_rename_failure(tmp_path, capsys, monkeypatch):
    call_count = 0
    original = pathlib.Path.replace

    def patched(self, target):
        nonlocal call_count
        call_count += 1
        if call_count == 2:
            raise OSError("rename failed")
        return original(self, target)

    monkeypatch.setattr(pathlib.Path, "replace", patched)
    rc = init_main(["--path", str(tmp_path), "--pipeline", "local"])
    assert rc == 1

    err = capsys.readouterr().err
    assert err.startswith("error:")
    assert err.count("\n") == 1
    assert list(tmp_path.rglob("*.tmp.*")) == []


# ---------------------------------------------------------------------------
# AGENTS.md: conflict blocks without --force; --force overwrites
# ---------------------------------------------------------------------------


def test_agents_md_conflict_blocked(tmp_path, capsys):
    agents_md = tmp_path / "AGENTS.md"
    agents_md.write_text("old", encoding="utf-8")

    rc = init_main(["--path", str(tmp_path), "--pipeline", "local"])
    assert rc == 1

    err = capsys.readouterr().err
    assert "already exists" in err
    assert agents_md.read_text(encoding="utf-8") == "old"


def test_agents_md_force_overwrites(tmp_path, capsys):
    agents_md = tmp_path / "AGENTS.md"
    agents_md.write_text("old", encoding="utf-8")

    rc = init_main(["--path", str(tmp_path), "--pipeline", "local", "--force"])
    assert rc == 0
    content = agents_md.read_text(encoding="utf-8")
    assert content != "old"
    assert "gremlins" in content
