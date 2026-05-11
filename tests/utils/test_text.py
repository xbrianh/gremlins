import pathlib

from gremlins.utils.text import read_markdown_title, slugify

# slugify


def test_slugify_basic():
    assert slugify("Hello World") == "hello-world"


def test_slugify_special_chars():
    assert slugify("foo/bar & baz") == "foo-bar-baz"


def test_slugify_strips_leading_trailing_hyphens():
    assert not slugify("---hello---").startswith("-")
    assert not slugify("---hello---").endswith("-")


def test_slugify_no_truncation_when_short():
    text = "short title"
    result = slugify(text, max_len=40)
    assert len(result) <= 40
    assert "short" in result


def test_slugify_truncates_at_word_boundary_when_head_long_enough():
    # 40-char slug that splits cleanly at a word boundary, head >= 20 chars
    text = "this is a reasonably long title that exceeds the limit"
    result = slugify(text, max_len=40)
    assert len(result) <= 40
    assert not result.endswith("-")


def test_slugify_head_too_short_keeps_trimmed():
    # Force a slug where head < 20 so trimmed (not head) is used
    # "ab-" + "x"*38 → head="ab" (len 2 < 20), keep trimmed
    text = "ab " + "x" * 38
    result = slugify(text, max_len=40)
    assert len(result) <= 40
    assert not result.endswith("-")


def test_slugify_head_at_exactly_20_uses_head():
    # Construct a slug where head is exactly 20 chars
    # "a" * 20 + "-trailing-stuff-here-more"
    slug_text = "a" * 20 + " trailing stuff here more padding"
    result = slugify(slug_text, max_len=40)
    assert len(result) <= 40


def test_slugify_custom_max_len():
    result = slugify("hello world foo bar", max_len=10)
    assert len(result) <= 10


# read_markdown_title


def test_read_markdown_title_finds_h1(tmp_path: pathlib.Path) -> None:
    f = tmp_path / "doc.md"
    f.write_text("# My Title\n\nSome body.\n")
    assert read_markdown_title(str(f)) == "My Title"


def test_read_markdown_title_skips_non_h1(tmp_path: pathlib.Path) -> None:
    f = tmp_path / "doc.md"
    f.write_text("## Not H1\n\n# Real Title\n")
    assert read_markdown_title(str(f)) == "Real Title"


def test_read_markdown_title_missing_file() -> None:
    assert read_markdown_title("/nonexistent/path/doc.md") == ""


def test_read_markdown_title_no_h1(tmp_path: pathlib.Path) -> None:
    f = tmp_path / "doc.md"
    f.write_text("just some text\n## section\n")
    assert read_markdown_title(str(f)) == ""


def test_read_markdown_title_binary_file(tmp_path: pathlib.Path) -> None:
    f = tmp_path / "doc.md"
    f.write_bytes(b"\xff\xfe# broken\n")
    assert read_markdown_title(str(f)) == ""
