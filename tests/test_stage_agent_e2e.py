"""End-to-end test: YAML pipeline with type: agent reads one artifact and writes one.

The plan stage that uses Pipeline.from_yaml requires prompt entries to be file paths
(the preprocessor resolves them). Here we test via parse_stages() with pre-expanded
prompts, which is what the runtime sees after preprocessing — same execution path.
"""

from __future__ import annotations

import asyncio

from conftest import MINIMAL_EVENTS

from gremlins.artifacts.registry import ArtifactRegistry
from gremlins.artifacts.uri import Uri
from gremlins.clients.fake import FakeClaudeClient
from gremlins.executor.state import State, StateData, build_state
from gremlins.pipeline.loader import parse_stages
from gremlins.stages.outcome import Done


def test_agent_stage_e2e_reads_artifact_and_writes_output(tmp_path):
    """Full stack: parse from dict → run with registry → verify produced."""
    # parse_stages() receives pre-expanded prompt lists (post-preprocessing)
    raw = [
        {
            "name": "summarise",
            "type": "agent",
            "prompt": ["Summarise the following:\n\n{src}"],
            "in": {"src": "source-doc"},
            "out": {"summary": "file://session/summary.md"},
        }
    ]
    stages = parse_stages(raw)
    assert len(stages) == 1
    stage = stages[0]
    assert stage.type == "agent"
    assert stage.name == "summarise"

    # Bind the source document in the registry
    src_file = tmp_path / "source.md"
    src_file.write_bytes(b"# Hello\nWorld")
    registry = ArtifactRegistry(tmp_path, cwd=tmp_path)
    registry.bind("source-doc", Uri.parse("file://session/source.md"))

    # Client writes the expected output file when called
    summary_file = tmp_path / "summary.md"

    class WritingClient(FakeClaudeClient):
        async def run(self, prompt, *, label, **kwargs):
            summary_file.write_text("# Summary\nHello World")
            return await super().run(prompt, label=label, **kwargs)

    client = WritingClient(fixtures={"summarise": MINIMAL_EVENTS})
    state = build_state(
        data=StateData(),
        client=client,
        session_dir=tmp_path,
        worktree=tmp_path,
        artifacts=registry,
    )

    result = asyncio.run(stage.run(state))

    assert isinstance(result, Done)
    # Source content was substituted into the prompt
    assert "# Hello" in client.calls[0].prompt
    # Output artifact is bound in the registry
    assert registry.produced("summary")
    # Output file exists
    assert summary_file.exists()


def test_agent_parse_stages_registers_type():
    """Confirm the 'agent' type is recognised by the pipeline loader."""
    raw = [
        {
            "name": "my-stage",
            "type": "agent",
            "prompt": ["Do the thing"],
        }
    ]
    stages = parse_stages(raw)
    assert stages[0].type == "agent"
