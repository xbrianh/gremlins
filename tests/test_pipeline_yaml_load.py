"""Load tests for bundled pipeline YAMLs."""

import pathlib
import textwrap

import pytest

from gremlins.clients.client import Client
from gremlins.pipeline import Pipeline
from gremlins.pipeline.loader import fill_names as _fill_names
from gremlins.pipeline.preprocess import expand_pipeline

_BUNDLED_LOCAL = (
    pathlib.Path(__file__).parent.parent / "gremlins" / "pipelines" / "local.yaml"
)

_LOCAL_STAGE_NAMES = [
    "plan",
    "implement",
    "require-impl-progress",
    "review-code",
    "address-code",
    "normalize",
    "verify",
]


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


# --- _fill_names unit tests ---


def _stages(*types: str) -> list[dict]:
    return [{"type": t} for t in types]


def test_fill_names_defaults_to_type() -> None:
    raw = _stages("plan", "implement", "verify")
    _fill_names(raw)
    assert [d["name"] for d in raw] == ["plan", "implement", "verify"]


def test_fill_names_collision_gets_suffix() -> None:
    raw = _stages("verify", "verify", "verify")
    _fill_names(raw)
    assert [d["name"] for d in raw] == ["verify", "verify-2", "verify-3"]


def test_fill_names_explicit_wins() -> None:
    raw = [{"name": "my-plan", "type": "plan"}, {"type": "implement"}]
    _fill_names(raw)
    assert raw[0]["name"] == "my-plan"
    assert raw[1]["name"] == "implement"


def test_fill_names_explicit_name_reserves_slot() -> None:
    # explicit name "verify" (on a different type) still blocks the default for unnamed {type: verify}
    raw = [{"name": "verify", "type": "implement"}, {"type": "verify"}]
    _fill_names(raw)
    assert raw[0]["name"] == "verify"
    assert raw[1]["name"] == "verify-2"


def test_fill_names_parallel_key_uses_parallel_type() -> None:
    raw = [{"parallel": [{"type": "verify"}]}, {"parallel": [{"type": "plan"}]}]
    _fill_names(raw)
    assert raw[0]["name"] == "parallel"
    assert raw[1]["name"] == "parallel-2"


def test_fill_names_mixed_explicit_and_default_no_collision() -> None:
    raw = [
        {"name": "verify", "type": "verify"},
        {"type": "verify"},
        {"type": "verify"},
    ]
    _fill_names(raw)
    assert raw[0]["name"] == "verify"
    assert raw[1]["name"] == "verify-2"
    assert raw[2]["name"] == "verify-3"


# --- integration: name-optional pipeline loading ---


def _write_pipeline(tmp_path: pathlib.Path, content: str) -> pathlib.Path:
    p = tmp_path / "pipeline.yaml"
    p.write_text(textwrap.dedent(content), encoding="utf-8")
    return p


def test_pipeline_name_optional_defaults_to_type(tmp_path: pathlib.Path) -> None:
    p = _write_pipeline(
        tmp_path,
        """\
        default_client: claude:sonnet
        prompts:
          fix: |
            Fix the issue.
        stages:
          - { type: plan }
          - { type: agent, prompt: [] }
          - { type: verify, options: { cmds: ['true'] }, prompt: fix }
        """,
    )
    pipeline = Pipeline.from_yaml(p)
    assert [s.name for s in pipeline.stages] == ["plan", "agent", "verify"]


def test_pipeline_duplicate_unnamed_stages_auto_numbered(
    tmp_path: pathlib.Path,
) -> None:
    p = _write_pipeline(
        tmp_path,
        """\
        default_client: claude:sonnet
        stages:
          - { type: plan }
          - { type: exec, options: { cmds: ['true'] } }
          - { type: exec, options: { cmds: ['true'] } }
          - { type: exec, options: { cmds: ['true'] } }
        """,
    )
    pipeline = Pipeline.from_yaml(p)
    assert [s.name for s in pipeline.stages] == [
        "plan",
        "exec",
        "exec-2",
        "exec-3",
    ]


def test_pipeline_explicit_name_overrides_default(tmp_path: pathlib.Path) -> None:
    p = _write_pipeline(
        tmp_path,
        """\
        default_client: claude:sonnet
        stages:
          - { name: step-a, type: exec, options: { cmds: ['true'] } }
          - { type: exec, options: { cmds: ['true'] } }
        """,
    )
    pipeline = Pipeline.from_yaml(p)
    assert pipeline.stages[0].name == "step-a"
    assert pipeline.stages[1].name == "exec"


def test_pipeline_nested_scopes_disambiguate_independently(
    tmp_path: pathlib.Path,
) -> None:
    p = _write_pipeline(
        tmp_path,
        """\
        default_client: claude:sonnet
        stages:
          - { type: exec, options: { cmds: ['true'] } }
          - name: checks
            parallel:
              - { type: exec, options: { cmds: ['true'] } }
              - { type: exec, options: { cmds: ['true'] } }
        """,
    )
    pipeline = Pipeline.from_yaml(p)
    assert pipeline.stages[0].name == "exec"
    parallel = pipeline.stages[1]
    assert parallel.name == "checks"
    assert [c.name for c in parallel.body] == ["exec", "exec-2"]


# --- stage-definitions tests ---


def test_stage_definition_expands_to_primitive(tmp_path: pathlib.Path) -> None:
    p = _write_pipeline(
        tmp_path,
        """\
        default_client: claude:sonnet
        stage-definitions:
          normalize:
            type: exec
            options:
              cmds: ["ruff format ."]
        stages:
          - { type: plan }
          - { type: normalize }
        """,
    )
    expanded = expand_pipeline(p)
    stages = expanded["stages"]
    assert len(stages) == 2
    assert stages[1]["type"] == "exec"
    assert stages[1]["options"]["cmds"] == ["ruff format ."]


def test_stage_definition_call_site_out_applied(tmp_path: pathlib.Path) -> None:
    p = _write_pipeline(
        tmp_path,
        """\
        default_client: claude:sonnet
        stage-definitions:
          normalize:
            type: exec
            options:
              cmds: ["ruff format ."]
        stages:
          - name: normalize
            type: normalize
            out:
              commits: git://range
        """,
    )
    expanded = expand_pipeline(p)
    stage = expanded["stages"][0]
    assert stage["type"] == "exec"
    assert stage["name"] == "normalize"
    assert stage["out"] == {"commits": "git://range"}


def test_stage_definition_reused_twice_with_different_out(
    tmp_path: pathlib.Path,
) -> None:
    p = _write_pipeline(
        tmp_path,
        """\
        default_client: claude:sonnet
        stage-definitions:
          normalize:
            type: exec
            options:
              cmds: ["ruff format ."]
        stages:
          - { type: normalize, out: { a: git://range } }
          - { type: normalize, out: { b: git://range } }
        """,
    )
    expanded = expand_pipeline(p)
    stages = expanded["stages"]
    assert stages[0]["out"] == {"a": "git://range"}
    assert stages[1]["out"] == {"b": "git://range"}
    assert stages[0]["type"] == "exec"
    assert stages[1]["type"] == "exec"


def test_stage_definition_with_out_rejected(tmp_path: pathlib.Path) -> None:
    p = _write_pipeline(
        tmp_path,
        """\
        default_client: claude:sonnet
        stage-definitions:
          bad:
            type: exec
            out:
              key: git://range
            options:
              cmds: ["echo hi"]
        stages:
          - { type: bad }
        """,
    )
    with pytest.raises(ValueError, match="must not declare 'out:'"):
        expand_pipeline(p)


def test_stage_definitions_not_in_expanded_output(tmp_path: pathlib.Path) -> None:
    p = _write_pipeline(
        tmp_path,
        """\
        default_client: claude:sonnet
        stage-definitions:
          normalize:
            type: exec
            options:
              cmds: ["echo"]
        stages:
          - { type: normalize }
        """,
    )
    expanded = expand_pipeline(p)
    assert "stage-definitions" not in expanded


def test_stage_definition_self_cycle_raises(tmp_path: pathlib.Path) -> None:
    p = _write_pipeline(
        tmp_path,
        """\
        default_client: claude:sonnet
        stage-definitions:
          loop:
            type: loop
        stages:
          - { type: loop }
        """,
    )
    with pytest.raises(ValueError, match="stage-definition cycle"):
        expand_pipeline(p)


def test_stage_definition_mutual_cycle_raises(tmp_path: pathlib.Path) -> None:
    p = _write_pipeline(
        tmp_path,
        """\
        default_client: claude:sonnet
        stage-definitions:
          a:
            type: b
          b:
            type: a
        stages:
          - { type: a }
        """,
    )
    with pytest.raises(ValueError, match="stage-definition cycle"):
        expand_pipeline(p)


def test_stage_definitions_non_mapping_rejected(tmp_path: pathlib.Path) -> None:
    p = _write_pipeline(
        tmp_path,
        """\
        default_client: claude:sonnet
        stage-definitions:
          - normalize
        stages:
          - { type: plan }
        """,
    )
    with pytest.raises(ValueError, match="stage-definitions must be a mapping"):
        expand_pipeline(p)


def test_stage_definition_gremlins_recipe_expands(tmp_path: pathlib.Path) -> None:
    p = _write_pipeline(
        tmp_path,
        """\
        default_client: claude:sonnet
        prompts:
          impl-prompt: |
            Do the implementation.
        stage-definitions:
          implement: gremlins:implement
        stages:
          - { type: implement, name: impl-step, prompt: impl-prompt }
        """,
    )
    expanded = expand_pipeline(p)
    assert len(expanded["stages"]) > 0
    assert expanded["stages"][0]["name"] == "impl-step"


def test_required_prompt_missing_raises(tmp_path: pathlib.Path) -> None:
    p = _write_pipeline(
        tmp_path,
        """\
        default_client: claude:sonnet
        stages:
          - { type: verify, options: { cmds: ['true'] } }
        """,
    )
    with pytest.raises(ValueError, match="required prompt is missing or empty"):
        expand_pipeline(p)


def test_required_prompt_empty_raises(tmp_path: pathlib.Path) -> None:
    p = _write_pipeline(
        tmp_path,
        """\
        default_client: claude:sonnet
        stages:
          - { type: verify, options: { cmds: ['true'] }, prompt: [] }
        """,
    )
    with pytest.raises(ValueError, match="required prompt is missing or empty"):
        expand_pipeline(p)


def test_stage_definition_gremlins_recipe_missing_name_raises(
    tmp_path: pathlib.Path,
) -> None:
    p = _write_pipeline(
        tmp_path,
        """\
        default_client: claude:sonnet
        stage-definitions:
          bad: "gremlins:"
        stages:
          - { type: bad }
        """,
    )
    with pytest.raises(ValueError, match="missing name after"):
        expand_pipeline(p)


def test_stage_definition_gremlins_recipe_not_found_raises(
    tmp_path: pathlib.Path,
) -> None:
    p = _write_pipeline(
        tmp_path,
        """\
        default_client: claude:sonnet
        stage-definitions:
          bad: "gremlins:no-such-recipe"
        stages:
          - { type: bad }
        """,
    )
    with pytest.raises(FileNotFoundError, match="bundled recipe not found"):
        expand_pipeline(p)


def test_stage_definition_gremlins_recipe_path_traversal_raises(
    tmp_path: pathlib.Path,
) -> None:
    p = _write_pipeline(
        tmp_path,
        """\
        default_client: claude:sonnet
        stage-definitions:
          bad: "gremlins:../../etc/passwd"
        stages:
          - { type: bad }
        """,
    )
    with pytest.raises(ValueError, match="invalid bundled recipe name"):
        expand_pipeline(p)


def test_gremlins_prefix_type_resolves_directly(tmp_path: pathlib.Path) -> None:
    p = _write_pipeline(
        tmp_path,
        """\
        default_client: claude:sonnet
        prompts:
          impl-prompt: |
            Do the implementation.
        stages:
          - { type: gremlins:implement, name: impl-step, prompt: impl-prompt }
        """,
    )
    expanded = expand_pipeline(p)
    assert len(expanded["stages"]) == 2
    assert expanded["stages"][0]["name"] == "impl-step"
    assert expanded["stages"][0]["type"] == "agent"


def test_gremlins_prefix_type_accepts_dashes(tmp_path: pathlib.Path) -> None:
    p = _write_pipeline(
        tmp_path,
        """\
        default_client: claude:sonnet
        stages:
          - { type: gremlins:github-request-copilot-review }
        """,
    )
    expanded = expand_pipeline(p)
    assert len(expanded["stages"]) == 1
    assert expanded["stages"][0]["type"] == "exec"


def test_gremlins_prefix_type_unknown_raises(tmp_path: pathlib.Path) -> None:
    p = _write_pipeline(
        tmp_path,
        """\
        default_client: claude:sonnet
        stages:
          - { type: gremlins:no-such-recipe }
        """,
    )
    with pytest.raises(FileNotFoundError, match="bundled recipe not found"):
        expand_pipeline(p)


def test_gremlins_prefix_type_path_traversal_raises(tmp_path: pathlib.Path) -> None:
    p = _write_pipeline(
        tmp_path,
        """\
        default_client: claude:sonnet
        stages:
          - { type: "gremlins:../../etc/passwd" }
        """,
    )
    with pytest.raises(ValueError, match="invalid bundled recipe name"):
        expand_pipeline(p)


# --- type: <pipeline-name> tests ---


def test_type_resolves_to_pipeline_file(tmp_path: pathlib.Path) -> None:
    gremlins_dir = tmp_path / ".gremlins"
    gremlins_dir.mkdir()
    sub = gremlins_dir / "sub.yaml"
    sub.write_text(
        textwrap.dedent("""\
        default_client: claude:sonnet
        prompts:
          impl-prompt: |
            Do the implementation.
        stages:
          - { type: plan }
          - { type: implement, prompt: impl-prompt }
        """),
        encoding="utf-8",
    )
    p = _write_pipeline(
        tmp_path,
        """\
        default_client: claude:sonnet
        stages:
          - { type: sub }
        """,
    )
    expanded = expand_pipeline(p, project_root=tmp_path)
    assert len(expanded["stages"]) == 3
    assert expanded["stages"][0]["type"] == "plan"
    assert expanded["stages"][1]["type"] == "agent"
    assert expanded["stages"][1]["name"] == "implement"
    assert expanded["stages"][2]["name"] == "require-impl-progress"


def test_type_self_referencing_pipeline_does_not_recurse(
    tmp_path: pathlib.Path,
) -> None:
    gremlins_dir = tmp_path / ".gremlins"
    gremlins_dir.mkdir()
    p = gremlins_dir / "self-ref.yaml"
    p.write_text(
        textwrap.dedent("""\
        default_client: claude:sonnet
        stages:
          - { type: self-ref }
        """),
        encoding="utf-8",
    )
    # self-reference is skipped (falls through to loader); expand_pipeline must not recurse
    expanded = expand_pipeline(p)
    assert expanded["stages"][0]["type"] == "self-ref"
