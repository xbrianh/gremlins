import pathlib
import sys

_CODE_STYLE_PATH = pathlib.Path(__file__).resolve().parent / "code_style.md"


def load_code_style() -> str:
    """Return the canonical coding-style block. Dies if the file is missing or empty."""
    if not _CODE_STYLE_PATH.exists() or _CODE_STYLE_PATH.stat().st_size == 0:
        sys.stderr.write(f"error: missing or empty code style file: {_CODE_STYLE_PATH}\n")
        sys.stderr.flush()
        sys.exit(1)
    return _CODE_STYLE_PATH.read_text(encoding="utf-8").rstrip()
