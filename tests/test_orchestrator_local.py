import dataclasses
import json

import pytest
from conftest import MINIMAL_EVENTS
from conftest import REVIEW_LABELS as _REVIEW_LABELS
from conftest import ReviewCreatingClient as _ReviewCreatingClient
from conftest import common_local_patches as _common_patches

from gremlins.clients import ClientSpec
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
    assert labels[1] == "review-code:sonnet"
    assert labels[2] == "address-code"


def test_local_main_resume_from_review_code_requires_git_changes(
    tmp_path, monkeypatch, capsys
):
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
    monkeypatch.setattr("gremlins.orchestrators.local.in_git_repo", lambda: True)
    monkeypatch.setattr(
        "gremlins.orchestrators.local.has_dirty_worktree", lambda: False
    )
    monkeypatch.setattr("gremlins.orchestrators.local.has_commits", lambda: False)
    monkeypatch.setattr(
        "gremlins.orchestrators.local.load_prompts", lambda paths: "Be good."
    )

    with pytest.raises(SystemExit):
        local_main(
            ["--plan", str(plan_file), "--resume-from", "review-code"],
            client=FakeClaudeClient(fixtures={}),
        )

    assert (
        "--resume-from review-code requires implementation changes in the worktree"
        in capsys.readouterr().err
    )


def test_local_main_resume_from_review_code_allows_existing_git_changes(
    tmp_path, monkeypatch
):
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
    monkeypatch.setattr("gremlins.orchestrators.local.in_git_repo", lambda: True)
    monkeypatch.setattr(
        "gremlins.orchestrators.local.has_dirty_worktree", lambda: False
    )
    monkeypatch.setattr("gremlins.orchestrators.local.has_commits", lambda: True)
    monkeypatch.setattr(
        "gremlins.orchestrators.local.load_prompts", lambda paths: "Be good."
    )

    client = _ReviewCreatingClient(
        fixtures={
            "review-code:sonnet": MINIMAL_EVENTS,
            "address-code": MINIMAL_EVENTS,
        }
    )

    result = local_main(
        ["--plan", str(plan_file), "--resume-from", "review-code"],
        client=client,
    )

    assert result == 0
    assert [call.label for call in client.calls] == [
        "review-code:sonnet",
        "address-code",
    ]


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


def test_review_main_requires_commit_diff_or_dirty_worktree(
    tmp_path, monkeypatch, capsys
):
    _common_patches(monkeypatch)
    monkeypatch.setattr("gremlins.orchestrators.local.in_git_repo", lambda: True)
    monkeypatch.setattr("gremlins.orchestrators.local.rev_exists", lambda rev: True)
    monkeypatch.setattr(
        "gremlins.orchestrators.local.has_diff", lambda base, head: False
    )
    monkeypatch.setattr(
        "gremlins.orchestrators.local.has_dirty_worktree", lambda: False
    )

    with pytest.raises(SystemExit):
        review_main(["--dir", str(tmp_path)], client=FakeClaudeClient(fixtures={}))

    assert (
        "nothing to review: HEAD~1..HEAD has no changes and working tree is clean"
        in capsys.readouterr().err
    )


def test_review_main_allows_dirty_worktree_without_commit_diff(tmp_path, monkeypatch):
    _common_patches(monkeypatch)
    monkeypatch.setattr("gremlins.orchestrators.local.in_git_repo", lambda: True)
    monkeypatch.setattr("gremlins.orchestrators.local.rev_exists", lambda rev: True)
    monkeypatch.setattr(
        "gremlins.orchestrators.local.has_diff", lambda base, head: False
    )
    monkeypatch.setattr("gremlins.orchestrators.local.has_dirty_worktree", lambda: True)

    client = _ReviewCreatingClient(
        fixtures={lbl: MINIMAL_EVENTS for lbl in _REVIEW_LABELS}
    )

    result = review_main(["--dir", str(tmp_path)], client=client)

    assert result == 0
    assert {call.label for call in client.calls} == _REVIEW_LABELS


# ---------------------------------------------------------------------------
# address_main smoke test
# ---------------------------------------------------------------------------


def test_address_main_calls_client(tmp_path, monkeypatch):
    (tmp_path / "review-code-sonnet.md").write_text(
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
    review_label = "review-code:gpt-4o"
    client = _ReviewCreatingClient(
        fixtures={
            "implement": MINIMAL_EVENTS,
            review_label: MINIMAL_EVENTS,
            "address-code": MINIMAL_EVENTS,
        }
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

    # Override load_pipeline to inject default_client without a live client instance.
    _real_load_pipeline = load_pipeline

    def _load_pipeline_copilot_default(path):
        pipeline = _real_load_pipeline(path)
        stripped_stages = [dataclasses.replace(s, client=None) for s in pipeline.stages]
        return dataclasses.replace(
            pipeline,
            default_client=ClientSpec("copilot", "gpt-5.4"),
            stages=stripped_stages,
        )

    monkeypatch.setattr(
        "gremlins.orchestrators.local.load_pipeline", _load_pipeline_copilot_default
    )

    review_label = "review-code:gpt-5.4"
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


def test_local_main_resume_prefers_persisted_stage_clients_over_edited_pipeline(
    tmp_path, monkeypatch, make_state_dir
):
    gr_id = "resume-test-gr-id"
    make_state_dir(gr_id)

    session_dir = tmp_path / "session"
    session_dir.mkdir()
    plan_file = tmp_path / "plan.md"
    plan_file.write_text("# Plan\nDo stuff.\n")

    stage_defs = [
        ("plan", "plan"),
        ("implement", "implement"),
        ("review-code", "review-code"),
        ("address-code", "address-code"),
        ("verify", "verify"),
    ]
    original_stage_clients = {
        "plan": "claude:claude-sonnet-4-6",
        "implement": "claude:claude-haiku-4-5-20251001",
        "review-code": "copilot:gpt-4o",
        "address-code": "claude:claude-sonnet-4-6",
        "verify": "claude:claude-opus-4-1",
    }
    mutated_stage_clients = {
        stage_name: "claude:claude-opus-4-7" for stage_name, _ in stage_defs
    }

    pipeline_dir = tmp_path / ".gremlins" / "pipelines"
    pipeline_dir.mkdir(parents=True)
    pipeline_path = pipeline_dir / "local.yaml"
    style_path = pipeline_dir / "style.md"
    style_path.write_text("Style content.\n", encoding="utf-8")
    review_prompt_path = pipeline_dir / "review.md"
    review_prompt_path.write_text("Review prompt content.\n", encoding="utf-8")

    def write_pipeline(stage_clients: dict[str, str]) -> None:
        lines = ["name: local", "", "stages:"]
        for stage_name, stage_type in stage_defs:
            extras = ""
            if stage_type == "review-code":
                extras = ", prompt: [style.md, review.md]"
            lines.append(
                "  - { name: "
                f"{stage_name}, type: {stage_type}, client: "
                f"{json.dumps(stage_clients[stage_name])}{extras} }}"
            )
        pipeline_path.write_text("\n".join(lines) + "\n", encoding="utf-8")

    write_pipeline(original_stage_clients)

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)
    monkeypatch.setattr("gremlins.orchestrators.local.load_pipeline", load_pipeline)
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
    verify_models: list[str] = []
    monkeypatch.setattr(
        "gremlins.stages.verify.Verify.run",
        lambda self, pipe: verify_models.append(self.model),
    )

    original_review_label = "review-code:gpt-4o"
    mutated_review_label = "review-code:claude-opus-4-7"

    launch_client = _ReviewCreatingClient(
        fixtures={
            "implement": MINIMAL_EVENTS,
            original_review_label: MINIMAL_EVENTS,
            mutated_review_label: MINIMAL_EVENTS,
            "address-code": MINIMAL_EVENTS,
        }
    )

    result = local_main(["--plan", str(plan_file)], client=launch_client, gr_id=gr_id)
    assert result == 0

    state_dir = tmp_path / "xdg" / "claude-gremlins" / gr_id
    launch_state = json.loads((state_dir / "state.json").read_text(encoding="utf-8"))
    assert launch_state.get("stage_clients") == original_stage_clients
    assert verify_models == ["claude-opus-4-1"]

    write_pipeline(mutated_stage_clients)
    verify_models.clear()
    resume_client = _ReviewCreatingClient(
        fixtures={
            "implement": MINIMAL_EVENTS,
            original_review_label: MINIMAL_EVENTS,
            mutated_review_label: MINIMAL_EVENTS,
            "address-code": MINIMAL_EVENTS,
        }
    )

    result = local_main(
        ["--plan", str(plan_file), "--resume-from", "implement"],
        client=resume_client,
        gr_id=gr_id,
    )
    assert result == 0

    called_models = {call.label: call.model for call in resume_client.calls}
    assert called_models == {
        "implement": "claude-haiku-4-5-20251001",
        "review-code:gpt-4o": "gpt-4o",
        "address-code": "claude-sonnet-4-6",
    }
    assert verify_models == ["claude-opus-4-1"]


def test_local_main_resume_requires_persisted_stage_clients(
    tmp_path, monkeypatch, make_state_dir, capsys
):
    gr_id = "resume-test-gr-id"
    make_state_dir(gr_id)

    plan_file = tmp_path / "plan.md"
    plan_file.write_text("# Plan\nDo stuff.\n")

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)

    with pytest.raises(SystemExit):
        local_main(
            ["--plan", str(plan_file), "--resume-from", "implement"],
            client=FakeClaudeClient(fixtures={}),
            gr_id=gr_id,
        )

    assert "stage_clients not found" in capsys.readouterr().err


def test_local_main_resume_requires_each_persisted_stage_client(
    tmp_path, monkeypatch, make_state_dir, capsys
):
    gr_id = "resume-test-gr-id"
    state_dir = make_state_dir(gr_id)

    plan_file = tmp_path / "plan.md"
    plan_file.write_text("# Plan\nDo stuff.\n")

    state_file = state_dir / "state.json"
    state = json.loads(state_file.read_text(encoding="utf-8"))
    state["stage_clients"] = {
        "plan": "claude:sonnet",
        "implement": "claude:sonnet",
        "address-code": "claude:sonnet",
        "verify": "claude:sonnet",
    }
    state_file.write_text(json.dumps(state), encoding="utf-8")

    monkeypatch.chdir(tmp_path)
    _common_patches(monkeypatch)

    with pytest.raises(SystemExit):
        local_main(
            ["--plan", str(plan_file), "--resume-from", "implement"],
            client=FakeClaudeClient(fixtures={}),
            gr_id=gr_id,
        )

    assert "stage_clients missing stage: 'review-code'" in capsys.readouterr().err
