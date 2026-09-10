"""Tests for gremlins.executor.bootstrap."""

from __future__ import annotations

import asyncio
import pathlib
import sys

import pytest
from _gremlins_core.artifacts import ArtifactRegistry
from _gremlins_core.clients import RustClient as Client
from _gremlins_core.schemas import Bootstrap, InputSource, InputSources
from conftest import MockGremlin

from gremlins.executor.bootstrap import run_bootstrap, run_pipeline_bootstrap
from gremlins.executor.state import StateData, build_state


def test_run_bootstrap_empty_cmds_does_nothing(tmp_path: pathlib.Path) -> None:
    async def _test() -> None:
        await run_bootstrap([], tmp_path)

    asyncio.run(_test())


def test_run_bootstrap_runs_successfully(tmp_path: pathlib.Path) -> None:
    marker = tmp_path / "marker"

    async def _test() -> None:
        await run_bootstrap(
            [
                f"{sys.executable} -c \"import pathlib; pathlib.Path('{marker}').write_text('')\""
            ],
            tmp_path,
        )

    asyncio.run(_test())
    assert marker.exists()


def test_run_bootstrap_non_zero_exit_raises(tmp_path: pathlib.Path) -> None:
    async def _test() -> None:
        with pytest.raises(RuntimeError, match="bootstrap failed"):
            await run_bootstrap(["exit 1"], tmp_path)

    asyncio.run(_test())


def _gremlin(tmp_path: pathlib.Path) -> MockGremlin:
    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir(exist_ok=True)
    registry = ArtifactRegistry(artifact_dir)
    state = build_state(
        data=StateData(),
        client=Client("openai", "gpt-4o"),
        artifact_dir=artifact_dir,
        cwd=str(tmp_path),
        artifacts=registry,
    )
    return MockGremlin(state=state)


def test_launch_cmds_see_template_substitution(tmp_path: pathlib.Path) -> None:
    marker = tmp_path / "seen.txt"
    bootstrap = Bootstrap(
        source=InputSources(
            {"plan": InputSource(name="plan", types=["string"], optional=True)}
        ),
        launch_cmds=[f'printf "%s" {{plan}} > "{marker}"'],
    )
    gremlin = _gremlin(tmp_path)
    assert gremlin.state is not None

    async def _test() -> None:
        await run_pipeline_bootstrap(
            bootstrap,
            cwd=tmp_path,
            stage_inputs={"plan": "hello-plan"},
            gremlin=gremlin,
            include_launch=True,
        )

    asyncio.run(_test())
    assert marker.read_text() == "hello-plan"


def test_cli_out_bound_after_launch_cmds(tmp_path: pathlib.Path) -> None:
    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir(exist_ok=True)
    bootstrap = Bootstrap(
        source=InputSources(
            {
                "instructions": InputSource(
                    name="instructions", types=["string"], optional=True
                )
            }
        ),
        launch_cmds=[
            "gremlins:bind_artifact(instructions, instructions, artifact://session/instructions.txt)",
        ],
    )
    gremlin = _gremlin(tmp_path)

    async def _test() -> None:
        await run_pipeline_bootstrap(
            bootstrap,
            cwd=tmp_path,
            stage_inputs={"instructions": "do the thing"},
            gremlin=gremlin,
            include_launch=True,
        )

    asyncio.run(_test())
    assert gremlin.state.artifacts.exists("instructions")
    assert gremlin.state.artifacts.content("instructions") == "do the thing"


def test_children_only_run_cmds(tmp_path: pathlib.Path) -> None:
    cmds_marker = tmp_path / "cmds"
    launch_marker = tmp_path / "launch"
    bootstrap = Bootstrap(
        cmds=[
            f"{sys.executable} -c \"import pathlib; pathlib.Path('{cmds_marker}').write_text('ok')\""
        ],
        launch_cmds=[
            f"{sys.executable} -c \"import pathlib; pathlib.Path('{launch_marker}').write_text('nope')\""
        ],
        cli_out={"instructions?": "artifact://session/instructions.txt"},
    )
    gremlin = _gremlin(tmp_path)

    async def _test() -> None:
        await run_pipeline_bootstrap(
            bootstrap,
            cwd=tmp_path,
            stage_inputs={"instructions": "do the thing"},
            gremlin=gremlin,
            include_launch=False,
        )

    asyncio.run(_test())
    assert cmds_marker.exists()
    assert not launch_marker.exists()
    assert not gremlin.state.artifacts.exists("instructions")


def test_cli_out_skipped_when_launch_excluded(tmp_path: pathlib.Path) -> None:
    """Resume / children skip launch_cmds and cli_out."""
    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir(exist_ok=True)
    (artifact_dir / "instructions.txt").write_text("stale", encoding="utf-8")
    bootstrap = Bootstrap(
        launch_cmds=[
            f'printf "%s" "fresh" > "{artifact_dir}/instructions.txt"',
        ],
        cli_out={"instructions?": "artifact://session/instructions.txt"},
    )
    gremlin = _gremlin(tmp_path)

    async def _test() -> None:
        await run_pipeline_bootstrap(
            bootstrap,
            cwd=tmp_path,
            stage_inputs={"instructions": "fresh"},
            gremlin=gremlin,
            include_launch=False,
        )

    asyncio.run(_test())
    assert not gremlin.state.artifacts.exists("instructions")
    assert (artifact_dir / "instructions.txt").read_text() == "stale"


# ---------------------------------------------------------------------------
# gremlins:bind_artifact DSL command tests
# ---------------------------------------------------------------------------


def test_parse_gremlins_command_detects_dsl() -> None:
    from gremlins.executor.bootstrap import _parse_gremlins_command

    result = _parse_gremlins_command(
        'gremlins:bind_artifact(plan, "plan", artifact://session/plan.md)'
    )
    assert result is not None
    cmd_name, args = result
    assert cmd_name == "bind_artifact"
    assert args == ["plan", "plan", "artifact://session/plan.md"]


def test_parse_gremlins_command_ignores_shell() -> None:
    from gremlins.executor.bootstrap import _parse_gremlins_command

    assert _parse_gremlins_command("ls -la") is None
    assert _parse_gremlins_command("echo hello") is None
    assert _parse_gremlins_command("gremlins:unknown") is None  # no parens


def test_parse_gremlins_command_trailing_content() -> None:
    """Content after the closing paren makes it a shell command, not DSL."""
    from gremlins.executor.bootstrap import _parse_gremlins_command

    result = _parse_gremlins_command(
        'gremlins:bind_artifact(plan, "plan", artifact://session/plan.md) && ls -la'
    )
    assert result is None  # treated as shell


def test_bind_artifact_inline_text(tmp_path: pathlib.Path) -> None:
    """Inline text source is written to artifact file and bound in registry."""
    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir(exist_ok=True)
    bootstrap = Bootstrap(
        source=InputSources(
            {"plan": InputSource(name="plan", types=["string"], optional=True)}
        ),
        launch_cmds=[
            "gremlins:bind_artifact(plan, plan, artifact://session/plan.md)",
        ],
    )
    gremlin = _gremlin(tmp_path)

    async def _test() -> None:
        await run_pipeline_bootstrap(
            bootstrap,
            cwd=tmp_path,
            stage_inputs={"plan": "implement the feature"},
            gremlin=gremlin,
            include_launch=True,
        )

    asyncio.run(_test())
    assert gremlin.state.artifacts.exists("plan")
    assert gremlin.state.artifacts.content("plan") == "implement the feature"
    assert gremlin.state.artifacts.exists("plan")


def test_bind_artifact_filepath_source(tmp_path: pathlib.Path) -> None:
    """Filepath source copies the file to artifact dir."""
    src = tmp_path / "my-plan.txt"
    src.write_text("plan from file", encoding="utf-8")

    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir(exist_ok=True)
    bootstrap = Bootstrap(
        source=InputSources(
            {
                "plan": InputSource(
                    name="plan", types=["filepath", "string"], optional=True
                )
            }
        ),
        launch_cmds=[
            "gremlins:bind_artifact(plan, plan, artifact://session/plan.md)",
        ],
    )
    gremlin = _gremlin(tmp_path)

    async def _test() -> None:
        await run_pipeline_bootstrap(
            bootstrap,
            cwd=tmp_path,
            stage_inputs={"plan": str(src)},
            gremlin=gremlin,
            include_launch=True,
        )

    asyncio.run(_test())
    assert gremlin.state.artifacts.exists("plan")
    assert gremlin.state.artifacts.content("plan") == "plan from file"
    assert gremlin.state.artifacts.exists("plan")


def test_bind_artifact_optional_missing(tmp_path: pathlib.Path) -> None:
    """Optional source with no value silently skips binding."""
    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir(exist_ok=True)
    bootstrap = Bootstrap(
        source=InputSources(
            {"plan": InputSource(name="plan", types=["string"], optional=True)}
        ),
        launch_cmds=[
            "gremlins:bind_artifact(plan, plan, artifact://session/plan.md)",
        ],
    )
    gremlin = _gremlin(tmp_path)

    async def _test() -> None:
        await run_pipeline_bootstrap(
            bootstrap,
            cwd=tmp_path,
            stage_inputs={},
            gremlin=gremlin,
            include_launch=True,
        )

    asyncio.run(_test())
    assert not gremlin.state.artifacts.exists("plan")
    assert not (artifact_dir / "plan.md").exists()


def test_bind_artifact_optional_empty(tmp_path: pathlib.Path) -> None:
    """Optional source with empty string value silently skips binding."""
    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir(exist_ok=True)
    bootstrap = Bootstrap(
        source=InputSources(
            {"plan": InputSource(name="plan", types=["string"], optional=True)}
        ),
        launch_cmds=[
            "gremlins:bind_artifact(plan, plan, artifact://session/plan.md)",
        ],
    )
    gremlin = _gremlin(tmp_path)

    async def _test() -> None:
        await run_pipeline_bootstrap(
            bootstrap,
            cwd=tmp_path,
            stage_inputs={"plan": ""},
            gremlin=gremlin,
            include_launch=True,
        )

    asyncio.run(_test())
    assert not gremlin.state.artifacts.exists("plan")


def test_bind_artifact_different_source_and_artifact_keys(
    tmp_path: pathlib.Path,
) -> None:
    """Source key and artifact key can differ."""
    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir(exist_ok=True)
    bootstrap = Bootstrap(
        source=InputSources(
            {"my_plan": InputSource(name="my_plan", types=["string"], optional=True)}
        ),
        launch_cmds=[
            "gremlins:bind_artifact(my_plan, plan, artifact://session/plan.md)",
        ],
    )
    gremlin = _gremlin(tmp_path)

    async def _test() -> None:
        await run_pipeline_bootstrap(
            bootstrap,
            cwd=tmp_path,
            stage_inputs={"my_plan": "hello from my_plan"},
            gremlin=gremlin,
            include_launch=True,
        )

    asyncio.run(_test())
    assert gremlin.state.artifacts.exists("plan")
    assert gremlin.state.artifacts.content("plan") == "hello from my_plan"
    assert not gremlin.state.artifacts.exists("my_plan")  # source key not bound


def test_bind_artifact_mixed_with_shell_commands(tmp_path: pathlib.Path) -> None:
    """DSL commands and shell commands can coexist in launch_cmds."""
    shell_script = tmp_path / "touch-marker.sh"
    shell_script.write_text(f"touch {tmp_path / 'shell-marker'}\n", encoding="utf-8")
    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir(exist_ok=True)
    bootstrap = Bootstrap(
        source=InputSources(
            {
                "plan": InputSource(name="plan", types=["string"], optional=True),
                "instructions": InputSource(
                    name="instructions", types=["string"], optional=True
                ),
            }
        ),
        launch_cmds=[
            # DSL command
            "gremlins:bind_artifact(plan, plan, artifact://session/plan.md)",
            # Regular shell command
            f"sh {shell_script}",
            # Another DSL command
            "gremlins:bind_artifact(instructions, instructions, artifact://session/instructions.txt)",
        ],
        cli_out={"plan?": "artifact://session/plan.md"},
    )
    gremlin = _gremlin(tmp_path)

    async def _test() -> None:
        await run_pipeline_bootstrap(
            bootstrap,
            cwd=tmp_path,
            stage_inputs={
                "plan": "the plan",
                "instructions": "the instructions",
            },
            gremlin=gremlin,
            include_launch=True,
        )

    asyncio.run(_test())
    assert gremlin.state.artifacts.exists("plan")
    assert gremlin.state.artifacts.content("plan") == "the plan"
    assert gremlin.state.artifacts.exists("instructions")
    assert gremlin.state.artifacts.content("instructions") == "the instructions"
    assert (tmp_path / "shell-marker").exists()


def test_bind_artifact_unknown_command(tmp_path: pathlib.Path) -> None:
    """Unknown gremlins: DSL commands raise ValueError."""
    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir(exist_ok=True)
    bootstrap = Bootstrap(
        source=InputSources(
            {"plan": InputSource(name="plan", types=["string"], optional=True)}
        ),
        launch_cmds=["gremlins:unknown_cmd(plan, plan, artifact://session/plan.md)"],
    )
    gremlin = _gremlin(tmp_path)

    async def _test() -> None:
        with pytest.raises(ValueError, match="unknown gremlins: command"):
            await run_pipeline_bootstrap(
                bootstrap,
                cwd=tmp_path,
                stage_inputs={"plan": "value"},
                gremlin=gremlin,
                include_launch=True,
            )

    asyncio.run(_test())


def test_bind_artifact_too_few_args(tmp_path: pathlib.Path) -> None:
    """bind_artifact with a non-URI first argument raises ValueError."""
    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir(exist_ok=True)
    bootstrap = Bootstrap(
        source=InputSources(
            {"plan": InputSource(name="plan", types=["string"], optional=True)}
        ),
        launch_cmds=["gremlins:bind_artifact(plan, plan)"],
    )
    gremlin = _gremlin(tmp_path)

    async def _test() -> None:
        with pytest.raises(ValueError, match="does not look like a URI"):
            await run_pipeline_bootstrap(
                bootstrap,
                cwd=tmp_path,
                stage_inputs={"plan": "value"},
                gremlin=gremlin,
                include_launch=True,
            )

    asyncio.run(_test())


def test_parse_gremlins_command_quoted_args() -> None:
    """Quoted arguments have their surrounding quotes stripped."""
    from gremlins.executor.bootstrap import _parse_gremlins_command

    result = _parse_gremlins_command(
        'gremlins:bind_artifact(plan, "my plan", artifact://session/my-plan.md)'
    )
    assert result is not None
    _, args = result
    assert args == ["plan", "my plan", "artifact://session/my-plan.md"]

    result = _parse_gremlins_command(
        "gremlins:bind_artifact(plan, 'my plan', artifact://session/my-plan.md)"
    )
    assert result is not None
    _, args = result
    assert args == ["plan", "my plan", "artifact://session/my-plan.md"]


def test_bind_artifact_skipped_when_launch_excluded(tmp_path: pathlib.Path) -> None:
    """DSL commands are skipped when include_launch=False (resume/children)."""
    artifact_dir = tmp_path / "artifacts"
    artifact_dir.mkdir(exist_ok=True)
    (artifact_dir / "plan.md").write_text("stale", encoding="utf-8")
    bootstrap = Bootstrap(
        source=InputSources(
            {"plan": InputSource(name="plan", types=["string"], optional=True)}
        ),
        launch_cmds=[
            "gremlins:bind_artifact(plan, plan, artifact://session/plan.md)",
        ],
    )
    gremlin = _gremlin(tmp_path)

    async def _test() -> None:
        await run_pipeline_bootstrap(
            bootstrap,
            cwd=tmp_path,
            stage_inputs={"plan": "fresh"},
            gremlin=gremlin,
            include_launch=False,
        )

    asyncio.run(_test())
    assert not gremlin.state.artifacts.exists("plan")
    assert (artifact_dir / "plan.md").read_text() == "stale"
