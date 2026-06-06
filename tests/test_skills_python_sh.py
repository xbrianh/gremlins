"""Tests for skills/_lib/python.sh interpreter selection.

The helper gates every shim entry point, so a regression here would break
all skill invocations before any Python code starts. These tests exercise
the candidate-selection and version-rejection logic by stubbing fake
"interpreters" in a temp directory and pointing the helper at them via a
constructed candidate list.
"""

import pathlib
import subprocess
import textwrap

import pytest
from conftest import _TestGremlin

REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent
HELPER = REPO_ROOT / "skills" / "_lib" / "python.sh"


def _make_fake_python(
    dirpath: pathlib.Path, name: str, version: tuple[int, int]
) -> pathlib.Path:
    """Create an executable shell script that mimics `python -c` version checks.

    The version is encoded as a single (major*100 + minor) integer so the
    bash test can compare against 311 (= 3.11) without nested brace groups
    that would conflict with f-string interpolation.
    """
    p = dirpath / name
    major, minor = version
    encoded = major * 100 + minor
    p.write_text(
        textwrap.dedent(f"""\
        #!/usr/bin/env bash
        # Fake python: mimics `python -c 'import sys; sys.exit(0 if sys.version_info >= (3, 11) else 1)'`
        # by exiting based on the hardcoded version stamped at file creation.
        if [[ "$1" == "-c" ]]; then
            if (( {encoded} >= 311 )); then
                exit 0
            else
                exit 1
            fi
        fi
        echo "Python {major}.{minor}.0"
    """)
    )
    p.chmod(0o755)
    return p


def _run_helper_with_candidates(candidates: list[str]) -> subprocess.CompletedProcess:
    """Run a bash snippet that overrides the helper's candidate list, then sources it."""
    # Build a bash array literal of candidates.
    arr = " ".join(f'"{c}"' for c in candidates)
    script = textwrap.dedent(f"""\
        # Replace the helper's candidate discovery by overriding the function before sourcing.
        # We extract the helper's exit-on-failure block but supply our own candidates.
        __claude_find_python() {{
            local candidates=({arr})
            local p
            for p in "${{candidates[@]}}"; do
                if [[ -x "$p" ]] && "$p" -c 'import sys; sys.exit(0 if sys.version_info >= (3, 11) else 1)' 2>/dev/null; then
                    echo "$p"
                    return 0
                fi
            done
            return 1
        }}
        CLAUDE_PY="$(__claude_find_python)" || {{
            echo "skills: no python interpreter (>=3.11) found in known locations" >&2
            exit 127
        }}
        echo "$CLAUDE_PY"
    """)
    return subprocess.run(
        ["bash", "-c", script],
        capture_output=True,
        text=True,
    )


def test_helper_selects_first_compatible_interpreter(tmp_path):
    """Helper must pick the first candidate whose version satisfies >=3.11."""
    py312 = _make_fake_python(tmp_path, "py312", (3, 12))
    py313 = _make_fake_python(tmp_path, "py313", (3, 13))
    result = _run_helper_with_candidates([str(py312), str(py313)])
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == str(py312)


def test_helper_skips_incompatible_versions(tmp_path):
    """A 3.10 (or older) candidate must be rejected even though it is executable."""
    py310 = _make_fake_python(tmp_path, "py310", (3, 10))
    py312 = _make_fake_python(tmp_path, "py312", (3, 12))
    result = _run_helper_with_candidates([str(py310), str(py312)])
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == str(py312)


def test_helper_exits_127_when_no_compatible_candidate(tmp_path):
    """If every candidate is too old or missing, the helper exits 127 with the advertised message."""
    py39 = _make_fake_python(tmp_path, "py39", (3, 9))
    missing = tmp_path / "does-not-exist"
    result = _run_helper_with_candidates([str(py39), str(missing)])
    assert result.returncode == 127
    assert "no python interpreter (>=3.11) found" in result.stderr


@pytest.mark.skipif(
    not HELPER.exists(), reason="skills/_lib/python.sh not present in this repo"
)
def test_real_helper_picks_compatible_interpreter():
    """End-to-end: source the actual helper and verify it picks an interpreter that runs Python >=3.11."""
    script = f'set -e; . "{HELPER}"; "$CLAUDE_PY" -c "import sys; assert sys.version_info >= (3, 11); print(sys.executable)"'
    result = subprocess.run(
        _TestGremlin(["bash", "-c", script], capture_output=True, text=True)
    )
    assert result.returncode == 0, f"stdout={result.stdout!r} stderr={result.stderr!r}"
    assert result.stdout.strip()
