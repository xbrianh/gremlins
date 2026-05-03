"""Import all stage modules so each self-registers into the stage registry."""

from . import (  # noqa: F401
    address_code,
    commit_pr,
    ghaddress,
    ghreview,
    implement,
    plan,
    request_copilot,
    review_code,
    test,
    wait_ci,
    wait_copilot,
)
