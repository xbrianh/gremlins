from __future__ import annotations

import argparse
import importlib.resources
import sys


def prompt_for_assistant_main(argv: list[str]) -> int:
    p = argparse.ArgumentParser(
        prog="gremlins prompt-for-assistant",
        description="Print the assistant setup prompt to stdout.",
        epilog="Example: gremlins prompt-for-assistant | pbcopy",
    )
    p.parse_args(argv)

    content = (
        importlib.resources.files("gremlins.prompts.assistant")
        .joinpath("setup.md")
        .read_text(encoding="utf-8")
    )
    sys.stdout.write(content)
    return 0
