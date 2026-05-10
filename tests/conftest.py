import dataclasses
import json
import logging
import os
import pathlib
import re
import shutil
import sys

os.environ.setdefault("GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME", "main")

import pytest

import gremlins.orchestrators.run as _run_mod
from gremlins.clients.fake import FakeClaudeClient
from gremlins.schema import PipelineDef as _PipelineDef
from gremlins.schema import StageEntry as _StageEntry

TESTS_DIR = pathlib.Path(__file__).resolve().parent


def gh_pipeline() -> _PipelineDef:
    return _PipelineDef(
        name="test",
        path=pathlib.Path("."),
        stages=[
            _StageEntry(
                name="open-github-pr",
                type="open-github-pr",
                client=None,
                prompts=[],
                options={},
            )
        ],
    )
if str(TESTS_DIR) not in sys.path:
    sys.path.insert(0, str(TESTS_DIR))

# Shared minimal event stream used across test modules.
MINIMAL_EVENTS = [
    {"type": "system", "subtype": "init"},
    {"type": "result", "subtype": "success"},
]

# Label the detail reviewer emits (default sonnet model). Shared so the
# orchestrator smoke tests and the GR_ID-isolation regression tests stay
# in sync if the label scheme changes.
REVIEW_LABELS = {
    "review-code:sonnet",
}


class ReviewCreatingClient(FakeClaudeClient):
    """FakeClaudeClient that writes the review output file when a review-code
    label is called. Extracts the output path from the prompt so it lands at
    exactly the path run_review_code_stage expects to exist after the reviewer
    finishes. Shared between test_orchestrator_local and test_state_isolation."""

    def run(self, prompt, *, label, **kwargs):
        if label.startswith("review-code:"):
            m = re.search(r"`([^`]+\.md)`\s+is the canonical", prompt)
            assert m, f"regex did not match review-code prompt for label {label!r}"
            out = pathlib.Path(m.group(1))
            out.parent.mkdir(parents=True, exist_ok=True)
            out.write_text("# Review\n\n## Findings\nNone.\n")
        return super().run(prompt, label=label, **kwargs)


def common_local_patches(monkeypatch):
    """Apply monkeypatches shared across local-orchestrator smoke tests."""
    monkeypatch.setattr(
        shutil, "which", lambda n: "/fake/claude" if n == "claude" else None
    )
    monkeypatch.setattr(
        "gremlins.orchestrators.run.install_signal_handlers", lambda *c: None
    )

    # Strip pipeline client keys so the injected client is used for every stage.
    _real_load_pipeline = _run_mod.load_pipeline

    def _load_pipeline_no_clients(path):
        pipeline = _real_load_pipeline(path)
        stripped_stages = [dataclasses.replace(s, client=None) for s in pipeline.stages]
        return dataclasses.replace(
            pipeline, default_client=None, stages=stripped_stages
        )

    monkeypatch.setattr(
        "gremlins.orchestrators.run.load_pipeline", _load_pipeline_no_clients
    )


@pytest.fixture(autouse=True)
def _restore_root_logger():
    root = logging.getLogger()
    orig_level = root.level
    orig_handlers = root.handlers[:]
    yield
    root.setLevel(orig_level)
    root.handlers[:] = orig_handlers


@pytest.fixture(autouse=True)
def _isolate_gr_id(monkeypatch):
    # If the test process inherits GR_ID from a parent gremlin (e.g. an
    # implement stage running `python -m pytest`), gremlins.state.set_stage
    # would shell out to set-stage.sh against the parent's state.json and
    # corrupt its `stage` / `sub_stage` fields. Default-deny here; tests that
    # genuinely need GR_ID set it explicitly via monkeypatch.setenv, which
    # overrides this delenv.
    monkeypatch.delenv("GR_ID", raising=False)


@pytest.fixture(autouse=True)
def _clear_gremlins_overlay_env(monkeypatch):
    monkeypatch.delenv("GREMLINS_OVERLAY_DIR", raising=False)


@pytest.fixture
def test_state_root(tmp_path, monkeypatch):
    """Create and patch an isolated gremlins state root."""
    root = tmp_path / "state"
    monkeypatch.setattr("gremlins.paths.state_root", lambda: root)
    return root


@pytest.fixture
def make_state_dir(test_state_root):
    """Fixture factory: create a minimal state.json for gr_id under the state root.

    Returns a callable: make_state_dir(gr_id) -> state_dir_path
    """

    def _factory(gr_id: str) -> pathlib.Path:
        state_dir = test_state_root / gr_id
        state_dir.mkdir(parents=True, exist_ok=True)
        (state_dir / "state.json").write_text(
            json.dumps({"id": gr_id, "stage": "", "bail_class": ""})
        )
        return state_dir

    return _factory
