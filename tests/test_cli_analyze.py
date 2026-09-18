"""Tests for gremlins analyze command."""

from __future__ import annotations

import json
import pathlib
from unittest.mock import AsyncMock, patch

from gremlins.cli.analyze import analyze_main


def _setup_analyze_gremlin(
    sandbox,
    gremlin_id: str = "analyze-test-abc123",
    *,
    log_text: str | None = None,
    artifacts: dict[str, str] | None = None,
    state_overrides: dict | None = None,
) -> pathlib.Path:
    """Create a gremlin state directory under the sandbox for analysis."""
    state_dir = sandbox.state / gremlin_id
    state_dir.mkdir(parents=True, exist_ok=True)

    state = {
        "id": gremlin_id,
        "kind": "localgremlin",
        "stage": "implement",
        "status": "done",
        "exit_code": 0,
        "client": "openai:gpt-4o",
        "started_at": "2025-01-01T00:00:00Z",
        "ended_at": "2025-01-01T00:05:00Z",
        "pipeline_path": "local.yaml",
    }
    if state_overrides:
        state.update(state_overrides)

    (state_dir / "state.json").write_text(json.dumps(state))

    if log_text is not None:
        (state_dir / "log").write_text(log_text)

    if artifacts:
        art_dir = state_dir / "artifacts"
        art_dir.mkdir()
        for name, content in artifacts.items():
            (art_dir / name).write_text(content)

    return state_dir


class TestAnalyzeHappyPath:
    def test_analyze_with_mocked_client(self, sandbox, monkeypatch, capsys):
        """Full happy path with mocked _run_analysis."""
        monkeypatch.setenv("GREMLINS_CWD_OF_CLI_CMD", str(sandbox.project))

        log_text = (
            "2025-01-01T00:00:00Z INFO gremlins.spawn.pipeline entering plan\n"
            "2025-01-01T00:01:00Z INFO gremlins.spawn.pipeline exiting plan\n"
            "2025-01-01T00:01:00Z INFO gremlins.spawn.pipeline entering implement\n"
            "2025-01-01T00:04:00Z INFO gremlins.spawn.pipeline exiting implement\n"
            "2025-01-01T00:04:00Z INFO gremlins.spawn.pipeline entering review-code\n"
            "2025-01-01T00:05:00Z INFO gremlins.spawn.pipeline exiting review-code\n"
        )

        artifacts = {
            "plan.md": "# Plan\n\nThis is the plan.",
            "description.txt": "Fix the bug",
        }

        gremlin_id = "analyze-happy-abc123"
        _setup_analyze_gremlin(
            sandbox,
            gremlin_id=gremlin_id,
            log_text=log_text,
            artifacts=artifacts,
        )

        expected_report = "## Analysis Report\nAll good!"

        mock_run = AsyncMock(return_value=expected_report)

        with patch("gremlins.cli.analyze._run_analysis", mock_run):
            rc = analyze_main([gremlin_id])

        assert rc == 0

        # Verify the analysis function was called with the right arguments.
        mock_run.assert_called_once()
        args, _kwargs = mock_run.call_args
        client, prompt = args

        assert "2025-01-01T00:00:00Z" in prompt
        assert "plan.md" in prompt
        assert "# Plan" in prompt
        assert "description.txt" in prompt
        assert "Fix the bug" in prompt

        captured = capsys.readouterr()
        assert expected_report in captured.out


class TestAnalyzeErrors:
    def test_no_state_root(self, monkeypatch, tmp_path):
        """Returns error when state root doesn't exist."""
        monkeypatch.setenv("GREMLINS_SANDBOX_ROOT", str(tmp_path / "nonexistent"))
        rc = analyze_main(["nonexistent-id"])
        assert rc == 1

    def test_no_match(self, sandbox):
        """Returns error when gremlin ID doesn't match anything."""
        rc = analyze_main(["nonexistent-id"])
        assert rc == 1

    def test_no_client(self, sandbox, monkeypatch):
        """Returns error when no client is available."""
        monkeypatch.setenv("GREMLINS_CWD_OF_CLI_CMD", str(sandbox.project))

        _setup_analyze_gremlin(
            sandbox,
            gremlin_id="no-client-abc123",
            state_overrides={"client": ""},
        )

        rc = analyze_main(["no-client-abc123"])
        assert rc == 1

    def test_gremlin_with_no_log(self, sandbox, monkeypatch):
        """Works when gremlin has no log file."""
        monkeypatch.setenv("GREMLINS_CWD_OF_CLI_CMD", str(sandbox.project))

        _setup_analyze_gremlin(
            sandbox,
            gremlin_id="no-log-abc123",
            log_text=None,
        )

        mock_run = AsyncMock(return_value="ok")

        with patch("gremlins.cli.analyze._run_analysis", mock_run):
            rc = analyze_main(["no-log-abc123"])

        assert rc == 0
        args, _kwargs = mock_run.call_args
        _client, prompt = args
        assert "(no log file)" in prompt

    def test_gremlin_with_empty_log(self, sandbox, monkeypatch):
        """Shows (empty log) placeholder for empty log file."""
        monkeypatch.setenv("GREMLINS_CWD_OF_CLI_CMD", str(sandbox.project))

        _setup_analyze_gremlin(
            sandbox,
            gremlin_id="empty-log-abc123",
            log_text="",
        )

        mock_run = AsyncMock(return_value="ok")

        with patch("gremlins.cli.analyze._run_analysis", mock_run):
            rc = analyze_main(["empty-log-abc123"])

        assert rc == 0
        args, _kwargs = mock_run.call_args
        _client, prompt = args
        assert "(empty log)" in prompt

    def test_gremlin_with_no_artifacts(self, sandbox, monkeypatch):
        """Works when gremlin has no artifacts directory."""
        monkeypatch.setenv("GREMLINS_CWD_OF_CLI_CMD", str(sandbox.project))

        _setup_analyze_gremlin(
            sandbox,
            gremlin_id="no-artifacts-abc123",
            log_text="some log content",
            artifacts=None,
        )

        mock_run = AsyncMock(return_value="ok")

        with patch("gremlins.cli.analyze._run_analysis", mock_run):
            rc = analyze_main(["no-artifacts-abc123"])

        assert rc == 0
        args, _kwargs = mock_run.call_args
        _client, prompt = args
        assert "(no artifacts directory)" in prompt


class TestClientResolution:
    def test_uses_flag_client_over_state_client(self, sandbox, monkeypatch):
        """--client flag is parsed and passed to _resolve_client."""
        monkeypatch.setenv("GREMLINS_CWD_OF_CLI_CMD", str(sandbox.project))

        _setup_analyze_gremlin(
            sandbox,
            gremlin_id="override-client-abc123",
            state_overrides={"client": "openai:gpt-4o"},
            log_text="log content",
        )

        mock_run = AsyncMock(return_value="ok")

        with patch("gremlins.cli.analyze._run_analysis", mock_run):
            rc = analyze_main(
                ["override-client-abc123", "--client", "openai:gpt-4o-mini"]
            )

        assert rc == 0
        args, _kwargs = mock_run.call_args
        client, _prompt = args
        assert str(client) == "openai:gpt-4o-mini"

    def test_uses_state_client_when_no_flag(self, sandbox, monkeypatch):
        """Uses state.json client when --client flag is not provided."""
        monkeypatch.setenv("GREMLINS_CWD_OF_CLI_CMD", str(sandbox.project))

        _setup_analyze_gremlin(
            sandbox,
            gremlin_id="state-client-abc123",
            state_overrides={"client": "openai:gpt-4o"},
            log_text="log",
        )

        mock_run = AsyncMock(return_value="ok")

        with patch("gremlins.cli.analyze._run_analysis", mock_run):
            rc = analyze_main(["state-client-abc123"])

        assert rc == 0
        args, _kwargs = mock_run.call_args
        client, _prompt = args
        assert str(client) == "openai:gpt-4o"


class TestLogTail:
    def test_small_log_read_entirely(self, sandbox, monkeypatch):
        """A small log is read entirely."""
        monkeypatch.setenv("GREMLINS_CWD_OF_CLI_CMD", str(sandbox.project))

        log_text = "line 1\nline 2\nline 3\n"
        _setup_analyze_gremlin(
            sandbox,
            gremlin_id="small-log-abc123",
            log_text=log_text,
        )

        mock_run = AsyncMock(return_value="ok")

        with patch("gremlins.cli.analyze._run_analysis", mock_run):
            rc = analyze_main(["small-log-abc123"])

        assert rc == 0
        args, _kwargs = mock_run.call_args
        _client, prompt = args
        assert "line 1" in prompt
        assert "line 3" in prompt

    def test_large_log_truncated(self, sandbox, monkeypatch):
        """A large log is truncated to tail."""
        monkeypatch.setenv("GREMLINS_CWD_OF_CLI_CMD", str(sandbox.project))

        # Generate a log larger than _LOG_MAX_BYTES (50_000).
        # Each line is ~60 bytes, so 900 lines = ~54KB > 50KB.
        prefix = "2025-01-01T00:00:00Z INFO test.prefix "
        body = "x" * 30 + "\n"
        lines = [prefix + body for _ in range(900)]
        log_text = "".join(lines)

        _setup_analyze_gremlin(
            sandbox,
            gremlin_id="large-log-abc123",
            log_text=log_text,
        )

        mock_run = AsyncMock(return_value="ok")

        with patch("gremlins.cli.analyze._run_analysis", mock_run):
            rc = analyze_main(["large-log-abc123"])

        assert rc == 0
        args, _kwargs = mock_run.call_args
        _client, prompt = args
        # Should contain the last line's content pattern
        # but NOT the first line's content
        assert "test.prefix" in prompt
        # The last line body should be present
        assert body.strip() in prompt
        # The first few lines should not be present (they were truncated)
        # Verify we have fewer than 900 lines of test.prefix
        prompt_lines = prompt.splitlines()
        prefix_count = sum(1 for ln in prompt_lines if "test.prefix" in ln)
        assert 0 < prefix_count < 900

    def test_artifact_content_in_prompt(self, sandbox, monkeypatch):
        """Artifact content is included in the prompt."""
        monkeypatch.setenv("GREMLINS_CWD_OF_CLI_CMD", str(sandbox.project))

        _setup_analyze_gremlin(
            sandbox,
            gremlin_id="artifact-inline-abc123",
            log_text="log",
            artifacts={"plan.md": "# My Plan\n\nDo the thing."},
        )

        mock_run = AsyncMock(return_value="ok")

        with patch("gremlins.cli.analyze._run_analysis", mock_run):
            rc = analyze_main(["artifact-inline-abc123"])

        assert rc == 0
        args, _kwargs = mock_run.call_args
        _client, prompt = args
        assert "# My Plan" in prompt
        assert "plan.md" in prompt


class TestAnalyzeSubcommandRegistration:
    def test_analyze_in_dispatch(self):
        """Verify analyze is registered in the CLI dispatch table."""
        from gremlins.cli import _DISPATCH

        assert "analyze" in _DISPATCH
        help_text, handler = _DISPATCH["analyze"]
        assert "analyze" in help_text.lower()
        assert callable(handler)
