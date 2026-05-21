from gremlins.cli import main
from gremlins.prompts import BUNDLED_PROMPT_DIR


def test_prompt_for_assistant_matches_bundled_file(capsys):
    rc = main(["prompt-for-assistant"])
    assert rc == 0

    bundled_content = (BUNDLED_PROMPT_DIR / "assistant" / "setup.md").read_text(
        encoding="utf-8"
    )
    out, err = capsys.readouterr()
    assert out == bundled_content
    assert err == ""


def _output() -> str:
    bundled = BUNDLED_PROMPT_DIR / "assistant" / "setup.md"
    return bundled.read_text(encoding="utf-8")


def test_recommends_both_skills():
    out = _output()
    assert "/gremlins-launch" in out
    assert "/gremlins-queue" in out


def test_skill_bodies_derived_from_help():
    out = _output()
    assert "gremlins launch --help" in out
    assert "gremlins launch --list" in out
    assert "gremlins queue --help" in out


def test_no_hardcoded_pipeline_names():
    out = _output()
    # pipeline names are project-specific and must not be baked in except
    # the default gh-terse mentioned for the queue skill
    for hardcoded in [
        "gremlins launch gh-plain",
        "gremlins launch gh-verbose",
        "gremlins launch local",
    ]:
        assert hardcoded not in out


def test_launch_list_is_dynamic():
    out = _output()
    assert "gremlins launch --list" in out
    assert "project-specific" in out or "dynamic" in out or "project" in out


def test_queue_invariants():
    out = _output()
    assert "One runner per session" in out or "one runner per session" in out.lower()
    assert "--gremlin-id" in out
    assert "scope" in out.lower()
    assert "one launch+land pair" in out.lower()
