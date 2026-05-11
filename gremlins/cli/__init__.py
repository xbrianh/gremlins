"""Top-level dispatch for ``python -m gremlins.cli``."""

from __future__ import annotations

import sys

from gremlins.cli.fleet import (
    ack_main,
    close_main,
    fleet_main,
    land_main,
    log_main,
    rescue_main,
    rm_main,
    skip_main,
    stop_main,
)
from gremlins.cli.init import init_main
from gremlins.cli.launch import launch_main
from gremlins.cli.resume import resume_main
from gremlins.cli.review_address import address_main, review_main


def main(argv: list[str] | None = None) -> int:
    if argv is None:
        argv = sys.argv[1:]

    sub = argv[0] if argv else ""
    rest = argv[1:]

    if sub == "launch":
        return launch_main(rest)
    if sub == "init":
        return init_main(rest)
    if sub == "review":
        return review_main(rest)
    if sub == "address":
        return address_main(rest)
    if sub == "resume":
        return resume_main(rest)
    if sub == "stop":
        return stop_main(rest)
    if sub == "rescue":
        return rescue_main(rest)
    if sub == "land":
        return land_main(rest)
    if sub == "rm":
        return rm_main(rest)
    if sub == "close":
        return close_main(rest)
    if sub == "log":
        return log_main(rest)
    if sub == "ack":
        return ack_main(rest)
    if sub == "skip":
        return skip_main(rest)

    return fleet_main(argv)


if __name__ == "__main__":
    sys.exit(main())
