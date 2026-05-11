import pytest

from gremlins.runner import run_stages


def test_run_stages_executes_all_in_order():
    log = []
    stages = [
        ("a", lambda: log.append("a")),
        ("b", lambda: log.append("b")),
        ("c", lambda: log.append("c")),
    ]
    run_stages(stages)
    assert log == ["a", "b", "c"]


def test_run_stages_resume_from_skips_earlier():
    log = []
    stages = [
        ("a", lambda: log.append("a")),
        ("b", lambda: log.append("b")),
        ("c", lambda: log.append("c")),
    ]
    run_stages(stages, resume_from="b")
    assert log == ["b", "c"]


def test_run_stages_resume_from_unknown_raises():
    stages = [("a", lambda: None), ("b", lambda: None)]
    with pytest.raises(ValueError, match="unknown resume stage"):
        run_stages(stages, resume_from="z")


def test_run_stages_stops_at_first_exception():
    log = []

    def failing():
        raise RuntimeError("boom")

    stages = [
        ("a", lambda: log.append("a")),
        ("b", failing),
        ("c", lambda: log.append("c")),
    ]
    with pytest.raises(RuntimeError, match="boom"):
        run_stages(stages)
    assert log == ["a"]
