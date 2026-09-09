from pathlib import Path

import pytest
from _gremlins_core.schemas import Pipeline

CLIENT = "openai:gpt-4o"


def _write_pipeline(tmp_path: Path, content: str) -> Path:
    path = tmp_path / "pipeline.yaml"
    path.write_text(content)
    return path


def test_unused_interpolation_key_raises(tmp_path: Path):
    yaml_content = f"""
default_client: {CLIENT}
stages:
  - name: test
    type: agent
    prompt: |
      Hello world
    interpolation:
      plan: artifact.plan
"""
    path = _write_pipeline(tmp_path, yaml_content)
    with pytest.raises(ValueError, match="not referenced"):
        Pipeline.from_yaml(path)


def test_valid_pipeline_passes(tmp_path: Path):
    yaml_content = f"""
default_client: {CLIENT}
stages:
  - name: test
    type: agent
    prompt: |
      Hello {{plan}}
    interpolation:
      plan: artifact.plan
"""
    path = _write_pipeline(tmp_path, yaml_content)
    Pipeline.from_yaml(path)


def test_key_in_prompt_text_passes(tmp_path: Path):
    yaml_content = f"""
default_client: {CLIENT}
stages:
  - name: test
    type: agent
    prompt: |
      Use the {{plan}} to do the thing
    interpolation:
      plan: artifact.plan
"""
    path = _write_pipeline(tmp_path, yaml_content)
    Pipeline.from_yaml(path)


def test_key_in_command_string_passes(tmp_path: Path):
    yaml_content = f"""
default_client: {CLIENT}
stages:
  - name: test
    type: exec
    prompt: |
      run the command
    options:
      cmds:
        - "echo {{plan}}"
    interpolation:
      plan: artifact.plan
"""
    path = _write_pipeline(tmp_path, yaml_content)
    Pipeline.from_yaml(path)


def test_key_in_shell_dollar_form_is_rejected_as_unused(tmp_path: Path):
    yaml_content = f"""
default_client: {CLIENT}
stages:
  - name: test
    type: exec
    prompt: |
      run the command
    options:
      cmds:
        - "echo $plan"
    interpolation:
      plan: artifact.plan
"""
    path = _write_pipeline(tmp_path, yaml_content)
    with pytest.raises(ValueError, match="not referenced"):
        Pipeline.from_yaml(path)


def test_key_in_shell_dollar_brace_form_is_rejected_as_unused(tmp_path: Path):
    yaml_content = f"""
default_client: {CLIENT}
stages:
  - name: test
    type: exec
    prompt: |
      run the command
    options:
      cmds:
        - "echo ${{plan}}"
    interpolation:
      plan: artifact.plan
"""
    path = _write_pipeline(tmp_path, yaml_content)
    with pytest.raises(ValueError, match="not referenced"):
        Pipeline.from_yaml(path)


def test_multiple_keys_one_unused_raises(tmp_path: Path):
    yaml_content = f"""
default_client: {CLIENT}
stages:
  - name: test
    type: agent
    prompt: |
      Hello {{plan}}
    interpolation:
      plan: artifact.plan
      unused_key: artifact.unused
"""
    path = _write_pipeline(tmp_path, yaml_content)
    with pytest.raises(ValueError, match="not referenced"):
        Pipeline.from_yaml(path)
