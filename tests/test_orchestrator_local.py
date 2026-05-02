
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
        "gremlins.orchestrators.local.resolve_session_dir", lambda: session_dir
    )
    # tmp_path is not a git repo → is_git=False; monkeypatch for clarity.
    monkeypatch.setattr("gremlins.orchestrators.local.in_git_repo", lambda: False)
    monkeypatch.setattr(
        "gremlins.orchestrators.local.load_code_style", lambda: "Be good."
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
