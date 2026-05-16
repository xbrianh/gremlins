"""Load tests for repo-local and bundled pipeline YAMLs."""

import pathlib
import textwrap

import pytest

from gremlins.clients.client import Client
from gremlins.pipeline import Pipeline

_REPO_ROOT = pathlib.Path(__file__).parent.parent
_LOCAL_TERSE = _REPO_ROOT / ".gremlins" / "local-terse.yaml"
_GH_TERSE_NO_COPILOT = _REPO_ROOT / ".gremlins" / "gh-terse-no-copilot.yaml"
_BUNDLED_LOCAL = _REPO_ROOT / "gremlins" / "pipelines" / "local.yaml"

_LOCAL_STAGE_NAMES = ["plan", "implement", "review-code", "address-code", "verify"]


@pytest.mark.parametrize(
    "path,expected_client",
    [
        (_LOCAL_TERSE, Client("xai", "grok-4")),
        (_BUNDLED_LOCAL, Client("claude", "sonnet")),
    ],
)
def test_local_pipeline_loads(path: pathlib.Path, expected_client: Client) -> None:
    pipeline = Pipeline.from_yaml(path)
    assert pipeline.default_client == expected_client
    assert [s.name for s in pipeline.stages] == _LOCAL_STAGE_NAMES
    for stage in pipeline.stages:
        assert stage.client == expected_client


def test_gh_terse_no_copilot_loads() -> None:
    pipeline = Pipeline.from_yaml(_GH_TERSE_NO_COPILOT)
    assert pipeline.name == "gh-terse-no-copilot"
    assert pipeline.default_client == Client("claude", "sonnet")
    assert pipeline.stages[0].name == "plan"


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
