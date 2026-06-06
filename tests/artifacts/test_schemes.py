"""Tests for gremlins.artifacts.schemes."""

from __future__ import annotations

from conftest import _TestGremlin
import pathlib
import subprocess

import pytest

from gremlins.artifacts.registry import ArtifactRegistry
from gremlins.artifacts.schemes import (
    FileArtifactResolver,
    GitResolver,
    snapshot_head_before,
)
from gremlins.artifacts.uri import Uri


def make_git_repo(tmp_path: pathlib.Path) -> tuple[str, str]:
    subprocess.run(_TestGremlin(["git", "init"], cwd=tmp_path, check=True, capture_output=True))
    subprocess.run(
        ["git", "config", "user.email", "test@test.com"],
        cwd=tmp_path,
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "config", "user.name", "Test"],
        cwd=tmp_path,
        check=True,
        capture_output=True,
    )
    (tmp_path / "a.txt").write_text("a")
    subprocess.run(_TestGremlin(["git", "add", "."], cwd=tmp_path, check=True, capture_output=True))
    subprocess.run(
        ["git", "commit", "-m", "first"], cwd=tmp_path, check=True, capture_output=True
    )
    (tmp_path / "b.txt").write_text("b")
    subprocess.run(_TestGremlin(["git", "add", "."], cwd=tmp_path, check=True, capture_output=True))
    subprocess.run(
        ["git", "commit", "-m", "second"], cwd=tmp_path, check=True, capture_output=True
    )
    first = subprocess.run(
        ["git", "rev-parse", "HEAD~1"],
        cwd=tmp_path,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    head = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=tmp_path,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    return first, head


# FileArtifactResolver tests


def test_file_resolver_read(tmp_path: pathlib.Path) -> None:
    (tmp_path / "out.txt").write_text("content", encoding="utf-8")
    resolver = FileArtifactResolver(tmp_path)
    uri = Uri(scheme="file", path="session/out.txt")
    assert resolver.read(uri) == "content"


def test_file_resolver_verify_produced_raises_when_absent(
    tmp_path: pathlib.Path,
) -> None:
    resolver = FileArtifactResolver(tmp_path)
    uri = Uri(scheme="file", path="session/missing.txt")
    with pytest.raises(FileNotFoundError):
        resolver.verify_produced(uri)


def test_file_resolver_verify_produced_raises_when_empty(
    tmp_path: pathlib.Path,
) -> None:
    (tmp_path / "empty.txt").write_bytes(b"")
    resolver = FileArtifactResolver(tmp_path)
    uri = Uri(scheme="file", path="session/empty.txt")
    with pytest.raises(FileNotFoundError):
        resolver.verify_produced(uri)


# GitResolver tests


def test_git_resolver_read_range(tmp_path: pathlib.Path) -> None:
    first, head = make_git_repo(tmp_path)
    resolver = GitResolver(cwd=tmp_path)
    uri = Uri(scheme="git", path=f"range/{first}..{head}")
    commits = resolver.read(uri)
    assert isinstance(commits, list)
    assert len(commits) == 1
    assert commits[0]["subject"] == "second"
    assert "sha" in commits[0]


# snapshot_head_before and bind_git_commit_range tests


def test_snapshot_and_bind_range(tmp_path: pathlib.Path) -> None:
    make_git_repo(tmp_path)

    base = snapshot_head_before(cwd=tmp_path)

    (tmp_path / "c.txt").write_text("c")
    subprocess.run(_TestGremlin(["git", "add", "."], cwd=tmp_path, check=True, capture_output=True))
    subprocess.run(
        ["git", "commit", "-m", "third"], cwd=tmp_path, check=True, capture_output=True
    )
    new_head = subprocess.run(
        ["git", "rev-parse", "HEAD"],
        cwd=tmp_path,
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()

    registry = ArtifactRegistry(artifact_dir=tmp_path, cwd=tmp_path)
    registry.bind_git_commit_range("test-range", base)

    assert registry.produced("test-range")
    uri = registry.resolve("test-range")
    assert uri.scheme == "git"
    assert base in uri.path
    assert new_head in uri.path
