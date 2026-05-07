"""Boss pipeline alias."""

from __future__ import annotations

from gremlins.orchestrators.local import local_main


def boss_main(argv: list[str], *, gr_id: str | None = None) -> int:
    return local_main(argv, gr_id=gr_id)
