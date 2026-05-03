"""Tests for gremlins.prompts.loader."""

import pytest

from gremlins.prompts.loader import load_prompts


def test_single_path(tmp_path):
    f = tmp_path / "a.md"
    f.write_text("hello", encoding="utf-8")
    assert load_prompts([f]) == "hello"


def test_single_path_rstripped(tmp_path):
    f = tmp_path / "a.md"
    f.write_text("hello\n\n", encoding="utf-8")
    assert load_prompts([f]) == "hello"


def test_multiple_paths_concatenated(tmp_path):
    a = tmp_path / "a.md"
    b = tmp_path / "b.md"
    a.write_text("first", encoding="utf-8")
    b.write_text("second", encoding="utf-8")
    result = load_prompts([a, b])
    assert result == "first\n\nsecond"


def test_multiple_paths_rstripped(tmp_path):
    a = tmp_path / "a.md"
    b = tmp_path / "b.md"
    a.write_text("first\n", encoding="utf-8")
    b.write_text("second\n\n", encoding="utf-8")
    result = load_prompts([a, b])
    assert result == "first\n\n\nsecond"


def test_missing_file_raises(tmp_path):
    with pytest.raises(FileNotFoundError, match="prompt file not found"):
        load_prompts([tmp_path / "nonexistent.md"])


def test_empty_file_raises(tmp_path):
    f = tmp_path / "empty.md"
    f.write_text("", encoding="utf-8")
    with pytest.raises(ValueError, match="prompt file is empty"):
        load_prompts([f])


def test_whitespace_only_file_raises(tmp_path):
    f = tmp_path / "blank.md"
    f.write_text("   \n\n  ", encoding="utf-8")
    with pytest.raises(ValueError, match="prompt file is empty"):
        load_prompts([f])


def test_missing_file_in_list_raises(tmp_path):
    a = tmp_path / "a.md"
    a.write_text("content", encoding="utf-8")
    with pytest.raises(FileNotFoundError):
        load_prompts([a, tmp_path / "missing.md"])
