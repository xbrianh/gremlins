from _gremlins_core.utils.yaml_io import load_bundled_prompt

from gremlins.cli import main


def test_prompt_for_assistant_matches_bundled_file(capsys):
    rc = main(["prompt-for-assistant"])
    assert rc == 0

    bundled_content = load_bundled_prompt("assistant/setup.md")
    out, err = capsys.readouterr()
    assert out == bundled_content
    assert err == ""


def _output() -> str:
    return load_bundled_prompt("assistant/setup.md")


def test_recommends_both_skills():
    out = _output()
    assert "gremlins launch" in out
    assert "gremlins queue" in out


def test_skill_bodies_derived_from_help():
    out = _output()
    assert "--help" in out
    assert "--list" in out
    assert "gremlins launch" in out


def test_no_hardcoded_pipeline_names():
    out = _output()
    for hardcoded in [
        "gremlins launch gh-plain",
        "gremlins launch gh-verbose",
        "gremlins launch local",
    ]:
        assert hardcoded not in out


def test_launch_list_is_dynamic():
    out = _output()
    assert "--list" in out
    assert "gremlins launch" in out


def test_queue_invariants():
    out = _output()
    assert "--gremlin-id" in out
    assert "one launch+land pair" in out.lower()
    assert "gremlins launch" in out
