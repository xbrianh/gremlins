from __future__ import annotations

import re


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
