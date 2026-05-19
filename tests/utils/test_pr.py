import pytest

from gremlins.utils.pr import pr_arg_to_ref


def test_bare_number():
    assert pr_arg_to_ref("697") == "pull/697/head"


def test_full_url():
    assert pr_arg_to_ref("https://github.com/owner/repo/pull/697") == "pull/697/head"


def test_url_with_trailing_path():
    assert (
        pr_arg_to_ref("https://github.com/owner/repo/pull/42/files") == "pull/42/head"
    )


def test_invalid_raises():
    with pytest.raises(ValueError):
        pr_arg_to_ref("not-a-pr")


def test_invalid_bare_word_raises():
    with pytest.raises(ValueError):
        pr_arg_to_ref("main")


def test_url_with_invalid_suffix_raises():
    with pytest.raises(ValueError):
        pr_arg_to_ref("https://github.com/owner/repo/pull/123abc")
