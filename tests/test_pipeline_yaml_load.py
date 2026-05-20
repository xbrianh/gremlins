"""Load tests for bundled pipeline YAMLs."""

import pathlib
import textwrap

import pytest

from gremlins.clients.client import Client
from gremlins.pipeline import Pipeline

_BUNDLED_LOCAL = pathlib.Path(__file__).parent.parent / "gremlins" / "pipelines" / "local.yaml"

_LOCAL_STAGE_NAMES = ["plan", "implement", "review-code", "address-code", "verify"]


def test_bundled_local_loads() -> None:
    pipeline = Pipeline.from_yaml(_BUNDLED_LOCAL)
    assert pipeline.default_client == Client("claude", "sonnet")
    assert [s.name for s in pipeline.stages] == _LOCAL_STAGE_NAMES
    for stage in pipeline.stages:
        assert stage.client == Client("claude", "sonnet")


def test_bad_default_client_rejected(tmp_path: pathlib.Path) -> None:
    bad = tmp_path / "pipeline.yaml"
    bad.write_text(
        textwrap.dedent("""\
            default_client: bogus:foo
            stages:
              - { name: plan, type: plan }
        """),
        encoding="utf-8",
    )
    with pytest.raises(ValueError, match="unknown provider"):
        Pipeline.from_yaml(bad)
