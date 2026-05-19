from __future__ import annotations

import re


def pr_arg_to_ref(arg: str) -> str:
    """Convert a PR number or GitHub PR URL to pull/<N>/head."""
    m = re.search(r"/pull/(\d+)", arg)
    if m:
        return f"pull/{m.group(1)}/head"
    if re.fullmatch(r"\d+", arg.strip()):
        return f"pull/{arg.strip()}/head"
    raise ValueError(f"cannot parse PR arg: {arg!r}")
