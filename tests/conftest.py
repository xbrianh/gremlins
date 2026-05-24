import json
import logging
import os
import pathlib
import re
import shutil
import subprocess
import sys
import tempfile

import pytest

from gremlins.clients.fake import FakeClaudeClient
from gremlins.pipeline import Pipeline
from gremlins.stages.github_open_pull_request import GitHubOpenPullRequest
from gremlins.utils.git import HeadAdvanced

os.environ.setdefault("GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME", "main")

REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent.parent
FIXTURES_DIR = pathlib.Path(__file__).resolve().parent / "fixtures"
FAKE_CLAUDE = FIXTURES_DIR / "fake_claude.py"


def _setup_claude_home(home: pathlib.Path) -> None:
    claude_dir = home / ".claude"
    claude_dir.mkdir(parents=True, exist_ok=True)
    for name in ("gremlins", "agents"):
        link = claude_dir / name
        if not link.exists() and not link.is_symlink():
            link.symlink_to(REPO_ROOT / name)


def _init_git_repo(path: pathlib.Path, *, with_origin: bool = False) -> None:
    path.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        ["git", "init", "-b", "main"], cwd=path, check=True, capture_output=True
    )
    subprocess.run(
        ["git", "config", "user.email", "test@test.com"],
        cwd=path,
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "config", "user.name", "Test"],
        cwd=path,
        check=True,
        capture_output=True,
    )
    (path / "README.md").write_text("init\n")
    subprocess.run(
        ["git", "add", "README.md"], cwd=path, check=True, capture_output=True
    )
    subprocess.run(
        ["git", "commit", "-m", "init"], cwd=path, check=True, capture_output=True
    )
    if with_origin:
        bare = path.parent / f"{path.name}.git"
        subprocess.run(
            ["git", "init", "--bare", "-b", "main", str(bare)],
            check=True,
            capture_output=True,
        )
        subprocess.run(
            ["git", "remote", "add", "origin", str(bare)],
            cwd=path,
            check=True,
            capture_output=True,
        )
        subprocess.run(
            ["git", "push", "-u", "origin", "main"],
            cwd=path,
            check=True,
            capture_output=True,
        )


class _Sandbox:
    def __init__(self, root: pathlib.Path) -> None:
        self.root = root
        self.state = root / "state"
        self.work = root / "work"
        self.config = root / "config"
        self.home = root / "home"
        self.project = root / "project"


class _ChildSandbox:
    def __init__(self, root: pathlib.Path, env: dict) -> None:
        self.root = root
        self.state = root / "state"
        self.work = root / "work"
        self.config = root / "config"
        self.home = root / "home"
        self.project = root / "project"
        self.env = env


def _get_gh_token() -> str:
    try:
        r = subprocess.run(
            ["gh", "auth", "token"],
            capture_output=True,
            text=True,
            timeout=5,
        )
        return r.stdout.strip() if r.returncode == 0 else ""
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return ""


# Captured once at import time (before any sandbox HOME override).
# Lets integration tests that spawn copilot/gh authenticate even with HOME
# redirected to the sandbox.
_GH_TOKEN: str = os.environ.get("GH_TOKEN", "") or _get_gh_token()


@pytest.hookimpl(hookwrapper=True)
def pytest_runtest_makereport(item, call):
    outcome = yield
    rep = outcome.get_result()
    if call.when == "call" and rep.failed and not hasattr(rep, "wasxfail"):
        item._sandbox_failed = True


@pytest.fixture(autouse=True)
def sandbox(monkeypatch, request):
    node_id = re.sub(r"[^\w]", "_", request.node.nodeid)[-60:]
    root = pathlib.Path(tempfile.mkdtemp(prefix=f"grem_{node_id}_", dir="/tmp"))
    original_cwd = pathlib.Path.cwd()

    sb = _Sandbox(root)
    for d in (sb.state, sb.work, sb.config, sb.home, sb.project):
        d.mkdir(parents=True)

    monkeypatch.setenv("GREMLINS_SANDBOX_ROOT", str(root))
    monkeypatch.setenv("HOME", str(sb.home))
    if _GH_TOKEN:
        monkeypatch.setenv("GH_TOKEN", _GH_TOKEN)
    monkeypatch.chdir(sb.project)

    _init_git_repo(sb.project)

    request.node._sandbox = sb
    yield sb

    os.chdir(original_cwd)
    if getattr(request.node, "_sandbox_failed", False):
        sys.stderr.write(f"\n[sandbox retained] {root}\n")
    else:
        shutil.rmtree(root)


@pytest.fixture
def lenv(sandbox, monkeypatch):
    """Launcher environment: isolated HOME, state root, git repo, fake claude."""
    from fixtures.shell_env import install_fake_bin

    _setup_claude_home(sandbox.home)

    bin_dir = sandbox.root / "bin"
    install_fake_bin(bin_dir, "claude", FAKE_CLAUDE)

    monkeypatch.setenv("FAKE_CLAUDE_LOG", str(sandbox.root / "fake_claude.log"))
    monkeypatch.setenv("GIT_OPTIONAL_LOCKS", "0")
    monkeypatch.setenv("GREMLINS_TEST_NOOP_PIPELINE", "1")
    old_path = os.environ.get("PATH", "")
    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{old_path}")
    monkeypatch.delenv("PYTHONPATH", raising=False)

    class _Env:
        pass

    e = _Env()
    e.home = sandbox.home
    e.bin_dir = bin_dir
    e.state_root = sandbox.state
    e.repo = sandbox.project
    e.fake_claude_log = sandbox.root / "fake_claude.log"
    return e


@pytest.fixture
def child_sandbox(sandbox, request):
    """Produce explicit subprocess environments for gremlins child processes.

    child_sandbox.share() → env dict sharing the parent test's sandbox.
    child_sandbox.fresh() → _ChildSandbox with an isolated sandbox + env dict.
    """
    _owned: list[pathlib.Path] = []

    def _child_env(overrides: dict) -> dict:
        env = dict(os.environ)
        existing = env.get("PYTHONPATH", "")
        # Two levels up from tests/conftest.py is the repo root containing
        # the gremlins package source — needed so subprocesses import source,
        # not a stale installed copy.
        src_root = str(pathlib.Path(__file__).resolve().parent.parent)
        env["PYTHONPATH"] = (
            src_root + os.pathsep + existing if existing else src_root
        )
        env.update(overrides)
        return env

    def _share() -> dict:
        return _child_env({"GREMLINS_SANDBOX_ROOT": str(sandbox.root), "HOME": str(sandbox.home)})

    def _fresh() -> _ChildSandbox:
        node_id = re.sub(r"[^\w]", "_", request.node.nodeid)[-60:]
        root = pathlib.Path(tempfile.mkdtemp(prefix=f"grem_{node_id}_child_", dir="/tmp"))
        cs = _ChildSandbox(
            root,
            _child_env({"GREMLINS_SANDBOX_ROOT": str(root), "HOME": str(root / "home")}),
        )
        for d in (cs.state, cs.work, cs.config, cs.home, cs.project):
            d.mkdir(parents=True)
        _owned.append(root)
        return cs

    class _Helper:
        share = staticmethod(_share)
        fresh = staticmethod(_fresh)

    yield _Helper

    failed = getattr(request.node, "_sandbox_failed", False)
    for root in _owned:
        if failed:
            sys.stderr.write(f"\n[child sandbox retained] {root}\n")
        else:
            shutil.rmtree(root, ignore_errors=True)


TESTS_DIR = pathlib.Path(__file__).resolve().parent


def gh_pipeline() -> Pipeline:
    return Pipeline(
        name="test",
        path=pathlib.Path("."),
        stages=[GitHubOpenPullRequest("github-open-pull-request", [], {})],
    )


if str(TESTS_DIR) not in sys.path:
    sys.path.insert(0, str(TESTS_DIR))

# Shared minimal event stream used across test modules.
MINIMAL_EVENTS = [
    {"type": "system", "subtype": "init"},
    {"type": "result", "subtype": "success"},
]

# Label the detail reviewer emits (default sonnet model). Shared so the
# orchestrator smoke tests and the GREMLIN_ID-isolation regression tests stay
# in sync if the label scheme changes.
REVIEW_LABELS = {
    "review-code-sonnet",
    "review-code-fake",
}


class ReviewCreatingClient(FakeClaudeClient):
    """FakeClaudeClient that writes the review output file when a review-code
    label is called. Extracts the output path from the prompt so it lands at
    exactly the path run_review_code_stage expects to exist after the reviewer
    finishes. Shared between test_orchestrator_local and test_state_isolation."""

    async def run(self, prompt, *, label, **kwargs):
        if label.startswith("review-code-"):
            m = re.search(r"`([^`]+\.md)`\s+is the canonical", prompt)
            assert m, f"regex did not match review-code prompt for label {label!r}"
            out = pathlib.Path(m.group(1))
            out.parent.mkdir(parents=True, exist_ok=True)
            out.write_text("# Review\n\n## Findings\nNone.\n")
            if label not in self._fixtures:
                assert label in REVIEW_LABELS, (
                    f"unexpected review-code label: {label!r}; "
                    f"expected one of {sorted(REVIEW_LABELS)}"
                )
                self._fixtures[label] = MINIMAL_EVENTS
        return await super().run(prompt, label=label, **kwargs)


def common_local_patches(monkeypatch):
    """Apply monkeypatches shared across local-orchestrator smoke tests."""
    monkeypatch.setattr(
        shutil, "which", lambda n: f"/fake/{n}" if n in ("claude", "git") else None
    )
    monkeypatch.setattr(
        "gremlins.executor.run._install_signal_handlers", lambda c: None
    )
    monkeypatch.setattr("gremlins.executor.run.in_git_repo", lambda: True)
    monkeypatch.setattr(
        "gremlins.stages.implement.proc.run_or_raise",
        lambda cmd, **kwargs: cmd[-1],
    )
    monkeypatch.setattr(
        "gremlins.stages.implement.classify_impl_outcome",
        lambda pre, **kwargs: HeadAdvanced(commit_count=1),
    )
    monkeypatch.setattr(
        "gremlins.stages.implement.commits_since",
        lambda ref, **kwargs: [],
    )


@pytest.fixture(autouse=True)
def _restore_root_logger():
    root = logging.getLogger()
    orig_level = root.level
    orig_handlers = root.handlers[:]
    yield
    root.setLevel(orig_level)
    root.handlers[:] = orig_handlers


@pytest.fixture(autouse=True)
def _isolate_gremlin_id(monkeypatch):
    # If the test process inherits GREMLIN_ID from a parent gremlin (e.g. an
    # implement stage running `python -m pytest`), gremlins.state.set_stage
    # would shell out to set-stage.sh against the parent's state.json and
    # corrupt its `stage` / `sub_stage` fields. Default-deny here; tests that
    # genuinely need GREMLIN_ID set it explicitly via monkeypatch.setenv, which
    # overrides this delenv.
    monkeypatch.delenv("GREMLIN_ID", raising=False)


@pytest.fixture(autouse=True)
def _clear_gremlins_overlay_env(monkeypatch):
    monkeypatch.delenv("GREMLINS_OVERLAY_DIR", raising=False)


@pytest.fixture
def make_state_dir(sandbox):
    """Fixture factory: create a minimal state.json for gremlin_id under the state root.

    Returns a callable: make_state_dir(gremlin_id) -> state_dir_path
    """

    def _factory(gremlin_id: str) -> pathlib.Path:
        state_dir = sandbox.state / gremlin_id
        state_dir.mkdir(parents=True, exist_ok=True)
        (state_dir / "state.json").write_text(
            json.dumps({"id": gremlin_id, "stage": ""})
        )
        return state_dir

    return _factory
