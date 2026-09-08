import json
import os
import pathlib
import signal
import subprocess
import sys
import types
from unittest.mock import patch

import pytest

from gremlins.executor.run import (
    _HANDLED_SIGS,
    _install_signal_handlers,
    _prepend_overlay_bin_to_path,
)
from tests.fake_client import FakeClient


class _TrackingClient(FakeClient):
    def __init__(self):
        super().__init__()
        self.reap_calls = 0

    def reap_all(self):
        self.reap_calls += 1


@pytest.fixture(autouse=True)
def _restore_signals():
    old = {s: signal.getsignal(s) for s in _HANDLED_SIGS}
    yield
    for s, h in old.items():
        signal.signal(s, h)


@pytest.mark.parametrize("sig", _HANDLED_SIGS)
def test_signal_handler_reaps_and_redelivers(sig):
    client = _TrackingClient()
    with patch("gremlins.executor.run.atexit.register"):
        gremlin = types.SimpleNamespace(state=None)
        _install_signal_handlers([client], gremlin)
    handler = signal.getsignal(sig)

    killed: list[tuple[int, int]] = []
    with patch.object(os, "kill", side_effect=lambda pid, s: killed.append((pid, s))):
        handler(sig, None)

    assert client.reap_calls == 1
    assert killed == [(os.getpid(), sig)]
    # handler should have reset to SIG_DFL so the next delivery is default
    assert signal.getsignal(sig) is signal.SIG_DFL


def test_atexit_log_logs_when_stage_set(caplog):
    registered: list = []
    with patch("gremlins.executor.run.atexit.register", side_effect=registered.append):
        gremlin = types.SimpleNamespace(state=None)
        _install_signal_handlers([], gremlin)

    assert len(registered) == 1
    atexit_fn = registered[0]

    with patch(
        "gremlins.executor.run._load_stage_attempt",
        return_value=("my-stage", "attempt-1"),
    ):
        with caplog.at_level("WARNING"):
            atexit_fn()

    assert "exiting via atexit" in caplog.text
    assert "my-stage" in caplog.text
    assert "attempt-1" in caplog.text


def test_atexit_log_silent_on_clean_exit(caplog):
    registered: list = []
    with patch("gremlins.executor.run.atexit.register", side_effect=registered.append):
        gremlin = types.SimpleNamespace(state=None)
        _install_signal_handlers([], gremlin)

    atexit_fn = registered[0]

    with patch("gremlins.executor.run._load_stage_attempt", return_value=("", "")):
        with caplog.at_level("WARNING"):
            atexit_fn()

    assert "exiting via atexit" not in caplog.text


# ---------------------------------------------------------------------------
# env isolation integration tests
# ---------------------------------------------------------------------------

REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent


def _run_isolation_subprocess(
    tmp_path: pathlib.Path,
    *,
    project_root: str,
    state_root: str,
    env_script: str | None,
    extra_parent_vars: dict[str, str] | None = None,
) -> dict[str, str]:
    """Run a subprocess that exercises the run_pipeline env isolation logic.

    Returns the resulting os.environ dict after isolation.
    """
    result_file = tmp_path / "_env_result.json"
    test_script = tmp_path / "_test_iso.py"

    code = "import os, json, pathlib\n"
    if extra_parent_vars:
        for k, v in extra_parent_vars.items():
            code += f"os.environ[{k!r}] = {v!r}\n"

    code += f"""
from gremlins.env_file import source_env_string

_project_root = {project_root!r}
state_dir = pathlib.Path({state_root!r}) / "test-gremlin"

_system = {{
    "GREMLINS_GREMLIN_ID": "test-gremlin",
    "GREMLINS_PROJECT_ROOT": _project_root,
    "GREMLINS_OVERLAY_DIR": str(state_dir / ".gremlins"),
    "GREMLINS_WORKTREE_PATH": "",
    "GREMLINS_ARTIFACT_DIR": str(pathlib.Path({state_root!r}) / "scratch" / "test-gremlin" / "artifacts"),
    "GREMLIN_WORKSPACE_DIR": "",
    "GREMLIN_STATE_DIR": str(state_dir),
}}

_base = dict(os.environ)
_base.update(_system)

"""
    if env_script is not None:
        code += f"""
_env = source_env_string({env_script!r}, base_env=_base, cwd=pathlib.Path(_project_root))
"""
    else:
        code += "\n_env = dict(_base)\n"

    code += f"""
os.environ.clear()
os.environ.update(_env)
os.environ.update(_system)

json.dump(dict(os.environ), open({str(result_file)!r}, "w"))
"""

    test_script.write_text(code)

    env = dict(os.environ)
    env["PYTHONPATH"] = str(REPO_ROOT)
    r = subprocess.run(
        [sys.executable, str(test_script)],
        capture_output=True,
        text=True,
        timeout=15,
        env=env,
    )
    assert r.returncode == 0, (
        f"subprocess failed:\nstderr: {r.stderr}\nstdout: {r.stdout}"
    )
    return json.loads(result_file.read_text())


def test_env_isolation_no_file(sandbox, tmp_path):
    """Without bootstrap.env, the full parent environment passes through."""
    result = _run_isolation_subprocess(
        tmp_path,
        project_root=str(sandbox.project),
        state_root=str(sandbox.state),
        env_script=None,
        extra_parent_vars={"MY_VAR": "my_value"},
    )
    # Parent vars pass through when there's no env script.
    assert result["MY_VAR"] == "my_value"
    assert result["GREMLINS_GREMLIN_ID"] == "test-gremlin"
    assert "GREMLINS_PROJECT_ROOT" in result


def test_env_isolation_with_file(sandbox, tmp_path):
    """With bootstrap.env, custom vars appear and parent env is available."""
    result = _run_isolation_subprocess(
        tmp_path,
        project_root=str(sandbox.project),
        state_root=str(sandbox.state),
        env_script="export CUSTOM_VAR=hello\n",
        extra_parent_vars={"PARENT_VAR": "still_here"},
    )
    assert result["CUSTOM_VAR"] == "hello"
    assert result["PARENT_VAR"] == "still_here"
    assert result["GREMLINS_GREMLIN_ID"] == "test-gremlin"


def test_system_vars_cannot_be_overridden(sandbox, tmp_path):
    """bootstrap.env cannot override system vars."""
    result = _run_isolation_subprocess(
        tmp_path,
        project_root=str(sandbox.project),
        state_root=str(sandbox.state),
        env_script="export GREMLINS_GREMLIN_ID=fake\n",
    )
    # System vars are re-injected after sourcing, so the real value survives.
    assert result["GREMLINS_GREMLIN_ID"] == "test-gremlin"


def test_system_vars_cannot_be_unset(sandbox, tmp_path):
    """bootstrap.env cannot unset system vars."""
    result = _run_isolation_subprocess(
        tmp_path,
        project_root=str(sandbox.project),
        state_root=str(sandbox.state),
        env_script="unset GREMLINS_PROJECT_ROOT\n",
    )
    # System vars are re-injected after sourcing, so the var is still present.
    assert "GREMLINS_PROJECT_ROOT" in result
    assert result["GREMLINS_PROJECT_ROOT"] == str(sandbox.project)


# ---------------------------------------------------------------------------
# .gremlins/bin PATH injection tests
# ---------------------------------------------------------------------------


def test_gremlins_bin_prepended_to_path(sandbox):
    """When .gremlins/bin exists, it is prepended to PATH."""
    overlay_dir = str(sandbox.state / "test-gremlin" / ".gremlins")
    bin_dir = pathlib.Path(overlay_dir) / "bin"
    bin_dir.mkdir(parents=True)

    os.environ["PATH"] = "/usr/bin:/bin"
    _prepend_overlay_bin_to_path(overlay_dir)

    first_entry = os.environ["PATH"].split(os.pathsep)[0]
    assert pathlib.Path(first_entry).resolve() == bin_dir.resolve()


def test_gremlins_bin_absent_no_path_change(sandbox):
    """When .gremlins/bin doesn't exist, PATH is unaffected."""
    overlay_dir = str(sandbox.state / "test-gremlin" / ".gremlins")
    assert not (pathlib.Path(overlay_dir) / "bin").exists()

    os.environ["PATH"] = "/usr/bin:/bin"
    _prepend_overlay_bin_to_path(overlay_dir)

    assert os.environ["PATH"] == "/usr/bin:/bin"
