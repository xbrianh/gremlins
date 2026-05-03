import pathlib


def load_prompts(paths: list[pathlib.Path]) -> str:
    """Concatenate prompt files with double newlines; rstrip the result.

    Raises FileNotFoundError on a missing file, ValueError on an empty one.
    """
    parts: list[str] = []
    for path in paths:
        if not path.exists():
            raise FileNotFoundError(f"prompt file not found: {path}")
        text = path.read_text(encoding="utf-8")
        if not text.strip():
            raise ValueError(f"prompt file is empty: {path}")
        parts.append(text)
    return "\n\n".join(parts).rstrip()
