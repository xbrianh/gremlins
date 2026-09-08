"""Tests for gremlins validate subcommand."""

from __future__ import annotations

import pathlib
from unittest.mock import MagicMock, patch

import gremlins.cli.validate as validate_mod

# ---------------------------------------------------------------------------
# validate_main — positive cases
# ---------------------------------------------------------------------------


def test_validate_valid_yaml_returns_zero(tmp_path):
    yaml_file = tmp_path / "valid.yaml"
    yaml_file.write_text("default_client: 'openai:gpt-4o'\nstages: []\n")

    mock_pipeline = MagicMock()
    mock_pipeline.from_yaml = MagicMock()

    with patch("_gremlins_core.schemas.Pipeline", mock_pipeline):
        rc = validate_mod.validate_main([str(yaml_file)])

    assert rc == 0
    mock_pipeline.from_yaml.assert_called_once()


def test_validate_valid_yml_extension_returns_zero(tmp_path):
    yaml_file = tmp_path / "valid.yml"
    yaml_file.write_text("default_client: 'openai:gpt-4o'\nstages: []\n")

    mock_pipeline = MagicMock()
    mock_pipeline.from_yaml = MagicMock()

    with patch("_gremlins_core.schemas.Pipeline", mock_pipeline):
        rc = validate_mod.validate_main([str(yaml_file)])

    assert rc == 0
    mock_pipeline.from_yaml.assert_called_once()


# ---------------------------------------------------------------------------
# validate_main — error cases
# ---------------------------------------------------------------------------


def test_validate_file_not_found_returns_one(tmp_path):
    nonexistent = tmp_path / "nonexistent.yaml"
    rc = validate_mod.validate_main([str(nonexistent)])
    assert rc == 1


def test_validate_not_a_yaml_file_returns_one(tmp_path):
    not_yaml = tmp_path / "file.txt"
    not_yaml.write_text("hello")
    rc = validate_mod.validate_main([str(not_yaml)])
    assert rc == 1


def test_validate_pipeline_parse_error_returns_one(tmp_path):
    yaml_file = tmp_path / "bad.yaml"
    yaml_file.write_text("invalid: [\n")

    mock_pipeline = MagicMock()
    mock_pipeline.from_yaml = MagicMock(
        side_effect=ValueError("pipeline is missing 'default_client'")
    )

    with patch("_gremlins_core.schemas.Pipeline", mock_pipeline):
        rc = validate_mod.validate_main([str(yaml_file)])

    assert rc == 1


def test_validate_resolves_relative_path(tmp_path):
    """validate_main resolves the path via pathlib.Path.resolve()."""
    yaml_file = tmp_path / "subdir" / "pipeline.yaml"
    yaml_file.parent.mkdir()
    yaml_file.write_text("default_client: 'openai:gpt-4o'\nstages: []\n")

    mock_pipeline = MagicMock()
    mock_pipeline.from_yaml = MagicMock()

    orig_cwd = pathlib.Path.cwd()
    try:
        import os

        os.chdir(tmp_path)
        with patch("_gremlins_core.schemas.Pipeline", mock_pipeline):
            rc = validate_mod.validate_main(["subdir/pipeline.yaml"])
        assert rc == 0
    finally:
        os.chdir(orig_cwd)
