from __future__ import annotations

import argparse
import sys

from gremlins.prompts import BUNDLED_PROMPT_DIR


def prompt_for_assistant_main(argv: list[str]) -> int:
    p = argparse.ArgumentParser(
        prog="gremlins prompt-for-assistant",
        description="Print the assistant setup prompt to stdout.",
        epilog="Example: gremlins prompt-for-assistant | pbcopy",
    )
    p.parse_args(argv)

    content = (BUNDLED_PROMPT_DIR / "assistant" / "setup.md").read_text(encoding="utf-8")
    sys.stdout.write(content)
    return 0
