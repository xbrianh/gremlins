import dataclasses
import json

from conftest import (
    MINIMAL_EVENTS,
)
from conftest import (
    REVIEW_LABELS as _REVIEW_LABELS,
)
from conftest import (
    ReviewCreatingClient as _ReviewCreatingClient,
)
from conftest import (
    common_local_patches as _common_patches,
)

from gremlins.clients.fake import FakeClaudeClient
from gremlins.orchestrators.local import address_main, local_main, review_main
from gremlins.pipeline import load_pipeline, resolve_pipeline_path

# ---------------------------------------------------------------------------
# local_main smoke test (--plan mode: skips plan, runs implement→review→address)
# ---------------------------------------------------------------------------


def test_local_main_plan_mode(tmp_path, monkeypatch):
    session_dir = tmp_path / "session"
    session_dir.mkdir()
    plan_file = tmp_path / "plan.md"
    plan_file.write_text("# Plan\nDo stuff.\n")

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)
    monkeypatch.setattr(
        "gremlins.orchestrators.local.resolve_session_dir",
        lambda gr_id=None: session_dir,
    )
    # tmp_path is not a git repo → is_git=False; monkeypatch for clarity.
    monkeypatch.setattr("gremlins.orchestrators.local.in_git_repo", lambda: False)
    monkeypatch.setattr(
        "gremlins.orchestrators.local.load_prompts", lambda paths: "Be good."
    )
    # Fake that implement produced changes (FakeClaudeClient won't create files).
    monkeypatch.setattr(
        "gremlins.stages.implement.changes_outside_git", lambda s, d: True
    )

    client = _ReviewCreatingClient(
        fixtures={
            "implement": MINIMAL_EVENTS,
            **{lbl: MINIMAL_EVENTS for lbl in _REVIEW_LABELS},
            "address-code": MINIMAL_EVENTS,
        }
    )

    result = local_main(["--plan", str(plan_file)], client=client)
    assert result == 0

    labels = [c.label for c in client.calls]
    assert labels[0] == "implement"
    assert labels[1] == "review-code:detail:sonnet"
    assert labels[2] == "address-code"


# ---------------------------------------------------------------------------
# review_main smoke test
# ---------------------------------------------------------------------------


def test_review_main_calls_client(tmp_path, monkeypatch):
    _common_patches(monkeypatch)
    monkeypatch.setattr("gremlins.orchestrators.local.in_git_repo", lambda: False)

    client = _ReviewCreatingClient(
        fixtures={lbl: MINIMAL_EVENTS for lbl in _REVIEW_LABELS}
    )

    result = review_main(["--dir", str(tmp_path)], client=client)
    assert result == 0
    assert {c.label for c in client.calls} == _REVIEW_LABELS


# ---------------------------------------------------------------------------
# address_main smoke test
# ---------------------------------------------------------------------------


def test_address_main_calls_client(tmp_path, monkeypatch):
    (tmp_path / "review-code-detail-sonnet.md").write_text(
        "# Detail Review\n\n## Findings\nNone.\n"
    )

    _common_patches(monkeypatch)
    monkeypatch.setattr("gremlins.orchestrators.local.in_git_repo", lambda: False)

    client = FakeClaudeClient(fixtures={"address-code": MINIMAL_EVENTS})

    result = address_main(["--dir", str(tmp_path)], client=client)
    assert result == 0
    assert len(client.calls) == 1
    assert client.calls[0].label == "address-code"
    assert client.calls[0].model == "sonnet"


def test_local_main_client_specifier_model(tmp_path, monkeypatch):
    """Model from --client provider:model flows into stage run() calls."""
    session_dir = tmp_path / "session"
    session_dir.mkdir()
    plan_file = tmp_path / "plan.md"
    plan_file.write_text("# Plan\nDo stuff.\n")

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)
    monkeypatch.setattr(
        "gremlins.orchestrators.local.resolve_session_dir",
        lambda gr_id=None: session_dir,
    )
    monkeypatch.setattr("gremlins.orchestrators.local.in_git_repo", lambda: False)
    monkeypatch.setattr(
        "gremlins.orchestrators.local.load_prompts", lambda paths: "Be good."
    )
    monkeypatch.setattr(
        "gremlins.stages.implement.changes_outside_git", lambda s, d: True
    )
    review_label = "review-code:detail:gpt-4o"
    client = _ReviewCreatingClient(
        fixtures={
            "implement": MINIMAL_EVENTS,
            review_label: MINIMAL_EVENTS,
            "address-code": MINIMAL_EVENTS,
        }
    )

    # Stub parse_client_specifier so it returns the injected test client rather
    # than creating a real SubprocessCopilotClient that would replace it.
    monkeypatch.setattr(
        "gremlins.orchestrators.local.parse_client_specifier",
        lambda spec: client,
    )

    result = local_main(
        ["--plan", str(plan_file), "--client", "copilot:gpt-4o"],
        client=client,
    )
    assert result == 0
    assert client.calls[0].model == "gpt-4o"  # implement stage
    assert client.calls[1].label == review_label


def test_local_pipeline_stage_names(tmp_path):
    pipeline = load_pipeline(resolve_pipeline_path("local", tmp_path))
    names = [s.name for s in pipeline.stages]
    assert names == ["plan", "implement", "review-code", "address-code", "verify"]


def test_local_main_writes_stage_to_state(tmp_path, monkeypatch, make_state_dir):
    gr_id = "test-gr-id"
    state_dir = make_state_dir(gr_id)

    session_dir = tmp_path / "session"
    session_dir.mkdir()
    plan_file = tmp_path / "plan.md"
    plan_file.write_text("# Plan\nDo stuff.\n")

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)
    monkeypatch.setattr(
        "gremlins.orchestrators.local.resolve_session_dir",
        lambda gr_id=None: session_dir,
    )
    monkeypatch.setattr("gremlins.orchestrators.local.in_git_repo", lambda: False)
    monkeypatch.setattr(
        "gremlins.orchestrators.local.load_prompts", lambda paths: "Be good."
    )
    monkeypatch.setattr(
        "gremlins.stages.implement.changes_outside_git", lambda s, d: True
    )

    client = _ReviewCreatingClient(
        fixtures={
            "implement": MINIMAL_EVENTS,
            **{lbl: MINIMAL_EVENTS for lbl in _REVIEW_LABELS},
            "address-code": MINIMAL_EVENTS,
        }
    )

    result = local_main(["--plan", str(plan_file)], client=client, gr_id=gr_id)
    assert result == 0

    data = json.loads((state_dir / "state.json").read_text())
    assert data.get("stage") == "address-code"


def test_local_main_state_client_tracks_effective_model(
    tmp_path, monkeypatch, make_state_dir
):
    gr_id = "test-gr-id"
    state_dir = make_state_dir(gr_id)

    session_dir = tmp_path / "session"
    session_dir.mkdir()
    plan_file = tmp_path / "plan.md"
    plan_file.write_text("# Plan\nDo stuff.\n")

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)
    monkeypatch.setattr(
        "gremlins.orchestrators.local.resolve_session_dir",
        lambda gr_id=None: session_dir,
    )
    monkeypatch.setattr("gremlins.orchestrators.local.in_git_repo", lambda: False)
    monkeypatch.setattr(
        "gremlins.orchestrators.local.load_prompts", lambda paths: "Be good."
    )
    monkeypatch.setattr(
        "gremlins.stages.implement.changes_outside_git", lambda s, d: True
    )

    client = _ReviewCreatingClient(
        fixtures={
            "implement": MINIMAL_EVENTS,
            "review-code:detail:opus": MINIMAL_EVENTS,
            "address-code": MINIMAL_EVENTS,
        }
    )
    monkeypatch.setattr(
        "gremlins.orchestrators.local.parse_client_specifier",
        lambda spec: client,
    )

    result = local_main(
        [
            "--plan",
            str(plan_file),
            "--client",
            "copilot:gpt-5.4",
            "-i",
            "opus",
            "-x",
            "opus",
            "-b",
            "opus",
            "-t",
            "opus",
        ],
        client=client,
        gr_id=gr_id,
    )
    assert result == 0

    data = json.loads((state_dir / "state.json").read_text())
    assert data.get("stage") == "address-code"
    assert data.get("client") == "copilot:opus"


def test_local_main_env_file_vars_reach_verify(tmp_path, monkeypatch):
    """Vars from .gremlins/env are visible to verify subprocesses."""
    session_dir = tmp_path / "session"
    session_dir.mkdir()
    plan_file = tmp_path / "plan.md"
    plan_file.write_text("# Plan\nDo stuff.\n")

    dot_gremlins = tmp_path / ".gremlins"
    dot_gremlins.mkdir()
    env_file = dot_gremlins / "env"
    env_file.write_text("export GREMLIN_ENV_TEST_SENTINEL=from_env_file\n")

    verify_cmd = 'test "$GREMLIN_ENV_TEST_SENTINEL" = from_env_file'

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)
    monkeypatch.setattr(
        "gremlins.orchestrators.local.resolve_session_dir",
        lambda gr_id=None: session_dir,
    )
    monkeypatch.setattr("gremlins.orchestrators.local.in_git_repo", lambda: False)
    monkeypatch.setattr(
        "gremlins.orchestrators.local.load_prompts", lambda paths: "Be good."
    )
    monkeypatch.setattr(
        "gremlins.stages.implement.changes_outside_git", lambda s, d: True
    )

    client = _ReviewCreatingClient(
        fixtures={
            "implement": MINIMAL_EVENTS,
            **{lbl: MINIMAL_EVENTS for lbl in _REVIEW_LABELS},
            "address-code": MINIMAL_EVENTS,
        }
    )

    monkeypatch.delenv("GREMLIN_ENV_TEST_SENTINEL", raising=False)
    result = local_main(
        ["--plan", str(plan_file), "--cmd", verify_cmd],
        client=client,
    )
    assert result == 0


def test_local_main_pipeline_default_client_model(tmp_path, monkeypatch):
    """pipeline.default_client_spec model used when --model and --client are absent.

    Regression: the model was computed before pipeline loading, so the pipeline's
    default_client_spec model was never consulted. A pipeline with
    default_client: copilot:gpt-5.4 produced model=sonnet.
    """
    session_dir = tmp_path / "session"
    session_dir.mkdir()
    plan_file = tmp_path / "plan.md"
    plan_file.write_text("# Plan\nDo stuff.\n")

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)
    monkeypatch.setattr(
        "gremlins.orchestrators.local.resolve_session_dir",
        lambda gr_id=None: session_dir,
    )
    monkeypatch.setattr("gremlins.orchestrators.local.in_git_repo", lambda: False)
    monkeypatch.setattr(
        "gremlins.orchestrators.local.load_prompts", lambda paths: "Be good."
    )
    monkeypatch.setattr(
        "gremlins.stages.implement.changes_outside_git", lambda s, d: True
    )

    # Override load_pipeline (wins over _common_patches's version) to inject
    # default_client_spec without a live client instance.
    _real_load_pipeline = load_pipeline

    def _load_pipeline_copilot_default(path):
        pipeline = _real_load_pipeline(path)
        stripped_stages = [dataclasses.replace(s, client=None) for s in pipeline.stages]
        return dataclasses.replace(
            pipeline,
            clients=[],
            default_client=None,
            default_client_spec="copilot:gpt-5.4",
            stages=stripped_stages,
        )

    monkeypatch.setattr(
        "gremlins.orchestrators.local.load_pipeline", _load_pipeline_copilot_default
    )

    review_label = "review-code:detail:gpt-5.4"
    client = _ReviewCreatingClient(
        fixtures={
            "implement": MINIMAL_EVENTS,
            review_label: MINIMAL_EVENTS,
            "address-code": MINIMAL_EVENTS,
        }
    )

    result = local_main(["--plan", str(plan_file)], client=client)
    assert result == 0
    assert client.calls[0].model == "gpt-5.4"  # implement
    assert client.calls[1].label == review_label
    assert client.calls[1].model == "gpt-5.4"  # review
