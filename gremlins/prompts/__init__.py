import pathlib

from .loader import load_prompts

BUNDLED_PROMPT_DIR = pathlib.Path(__file__).resolve().parent

__all__ = ["BUNDLED_PROMPT_DIR", "load_prompts"]
