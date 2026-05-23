from __future__ import annotations

import json
import re
from typing import Any


def to_str(value: Any) -> str:
    if isinstance(value, bytes):
        return value.decode("utf-8")
    if isinstance(value, (dict, list)):
        return json.dumps(value, indent=2)
    return str(value)


def slugify(text: str, max_len: int = 40) -> str:
    slug = re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")
    slug = re.sub(r"-+", "-", slug)
    if len(slug) > max_len:
        trimmed = slug[:max_len].rstrip("-")
        head, _, _ = trimmed.rpartition("-")
        if head and len(head) >= 20:
            trimmed = head
        slug = trimmed
    return slug


def read_markdown_title(path: str) -> str:
    try:
        with open(path, encoding="utf-8") as f:
            for line in f:
                m = re.match(r"^#\s+(.+)", line)
                if m:
                    return m.group(1).strip()
    except (OSError, UnicodeDecodeError):
        pass
    return ""
