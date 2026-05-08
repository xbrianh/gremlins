from gremlins import PACKAGE_ROOT
from gremlins.prompts.loader import load_prompts

BUNDLED_PROMPT_DIR = PACKAGE_ROOT / "prompts"

__all__ = ["BUNDLED_PROMPT_DIR", "load_prompts"]
