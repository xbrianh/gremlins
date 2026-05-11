from gremlins.cli import main
from gremlins.prompts import BUNDLED_PROMPT_DIR


def test_prompt_for_assistant_matches_bundled_file(capsys):
    rc = main(["prompt-for-assistant"])
    assert rc == 0

    bundled_content = (BUNDLED_PROMPT_DIR / "assistant" / "setup.md").read_text(encoding="utf-8")
    out, err = capsys.readouterr()
    assert out == bundled_content
    assert err == ""
