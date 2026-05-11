import importlib.resources

from gremlins.cli import main


def test_prompt_for_assistant_matches_bundled_file(capsys):
    rc = main(["prompt-for-assistant"])
    assert rc == 0

    bundled_content = (
        importlib.resources.files("gremlins.prompts.assistant")
        .joinpath("setup.md")
        .read_text(encoding="utf-8")
    )
    assert capsys.readouterr().out == bundled_content
