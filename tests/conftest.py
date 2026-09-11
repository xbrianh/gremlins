import dataclasses
import json
import logging
import os
import pathlib
import re
import shutil
import subprocess
import sys
import tempfile
import types
from typing import Any

import pytest
from _gremlins_core.clients import CLIENT_FACTORIES
from _gremlins_core.config import scratch_root

from gremlins.executor.gremlin import Gremlin, State
from gremlins.executor.state import StateData, build_state
from tests.fake_client import FakeClient

os.environ.setdefault("GIT_TEST_DEFAULT_INITIAL_BRANCH_NAME", "main")


def make_gremlin(
    *,
    gremlin_id: str | None = None,
    state_data: StateData | None = None,
    client: Any = None,
    artifact_dir: pathlib.Path | None = None,
    **state_kwargs: Any,
) -> Gremlin:
    """Create a Gremlin with sensible test defaults."""
    if artifact_dir is None:
        temp_root = pathlib.Path(tempfile.mkdtemp())
        artifact_dir = temp_root / "artifacts"
        artifact_dir.mkdir(parents=True, exist_ok=True)
    else:
        temp_root = artifact_dir.parent

    from _gremlins_core.schemas import Pipeline as PipelineData

    if client is None:
        client = FakeClient(fixtures={}, model="fake")

    if state_data is None:
        state_data = StateData(gremlin_id=gremlin_id)

    state = build_state(
        data=state_data,
        client=client,
        artifact_dir=artifact_dir,
        **state_kwargs,
    )

    gremlin = Gremlin(
        [],
        state_dir=temp_root,
        gremlin_id=gremlin_id,
        pipeline_data=PipelineData(name="test", path=pathlib.Path("."), stages=[]),
    )
    gremlin.state = state
    gremlin.registry = state.artifacts
    return gremlin


REPO_ROOT = pathlib.Path(__file__).resolve().parent.parent.parent
FIXTURES_DIR = pathlib.Path(__file__).resolve().parent / "fixtures"
FAKE_CLAUDE = FIXTURES_DIR / "fake_claude.py"


def pytest_configure(config: pytest.Config) -> None:
    def _make_fake_client(
        model: str | None, extra_params: dict[str, str] | None = None
    ) -> object:
        return FakeClient(
            fixtures={},
            model=model or "fake",
        )

    CLIENT_FACTORIES["fake"] = _make_fake_client


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
    (path / "Makefile").write_text("check:\n\t@true\ntest:\n\t@true\n")
    subprocess.run(
        ["git", "add", "README.md", "Makefile"],
        cwd=path,
        check=True,
        capture_output=True,
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
    def __init__(self, root: pathlib.Path, env: dict[str, str]) -> None:
        self.root = root
        self.state = root / "state"
        self.work = root / "work"
        self.config = root / "config"
        self.home = root / "home"
        self.project = root / "project"
        self.env = env


def _get_gh_token() -> str:
    if os.environ.get("CI"):
        return ""
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
def _restore_os_environ():
    """Restore os.environ after each test.

    run_pipeline() clears and rebuilds os.environ (env isolation), which
    corrupts the shared pytest process for direct-call tests. Snapshot the
    full env at setup and restore it at teardown so each test starts clean.
    """
    snapshot = dict(os.environ)
    yield
    os.environ.clear()
    os.environ.update(snapshot)


@pytest.fixture(autouse=True)
def _reset_config() -> None:
    """Clear the _gremlins_core.config singleton between tests.

    Each test gets a unique sandbox, so the previously cached config
    state would point to the old sandbox's config.json.  Resetting
    it here ensures get_config() re-reads from the new sandbox on its
    first call.
    """
    from _gremlins_core.config import clear

    clear()


@pytest.fixture(autouse=True)
def sandbox(monkeypatch, request):
    node_id = re.sub(r"[^\w]", "_", request.node.nodeid)[-60:]
    scratch_dir = os.environ.get("GREMLINS_SCRATCH_DIR", "/tmp")
    os.makedirs(scratch_dir, exist_ok=True)
    root = pathlib.Path(
        tempfile.mkdtemp(
            prefix=f"grem_{node_id}_",
            dir=scratch_dir,
        )
    )
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

    # Copy fixture pipelines into the project overlay so tests that pass
    # bare pipeline names (e.g. "local", "gh") can resolve them.
    fixture_pipelines = TESTS_DIR / "fixtures" / "pipelines"
    if fixture_pipelines.is_dir():
        overlay_pipelines = sb.project / ".gremlins"
        overlay_pipelines.mkdir(parents=True, exist_ok=True)
        for fixture_file in fixture_pipelines.iterdir():
            if fixture_file.is_file():
                shutil.copy2(fixture_file, overlay_pipelines / fixture_file.name)
        # Also copy stage definitions and prompts from the project root
        # so pipeline YAMLs can resolve gremlins:... references.
        # Prompts are copied flat into the overlay dir (not subdir) so that
        # pipelines without prompt_dir can find them relative to the yaml dir.
        # Also copy into prompts/ subdirectory for pipelines that set
        # prompt_dir: prompts.
        src_stages = TESTS_DIR.parent / ".gremlins" / "stages"
        if src_stages.is_dir():
            dst_stages = overlay_pipelines / "stages"
            shutil.copytree(str(src_stages), str(dst_stages), dirs_exist_ok=True)
        src_prompts = TESTS_DIR.parent / ".gremlins" / "prompts"
        if src_prompts.is_dir():
            for prompt_file in src_prompts.iterdir():
                if prompt_file.is_file():
                    shutil.copy2(prompt_file, overlay_pipelines / prompt_file.name)
            dst_prompts_subdir = overlay_pipelines / "prompts"
            dst_prompts_subdir.mkdir(parents=True, exist_ok=True)
            for prompt_file in src_prompts.iterdir():
                if prompt_file.is_file():
                    shutil.copy2(prompt_file, dst_prompts_subdir / prompt_file.name)

    request.node._sandbox = sb
    yield sb

    os.chdir(original_cwd)
    if getattr(request.node, "_sandbox_failed", False):
        sys.stderr.write(f"\n[sandbox retained] {root}\n")
    else:
        shutil.rmtree(root, ignore_errors=True)


@pytest.fixture
def lenv(sandbox, monkeypatch):
    """Launcher environment: isolated HOME, state root, git repo, fake claude."""
    from fixtures.shell_env import install_fake_bin

    _setup_claude_home(sandbox.home)

    bin_dir = sandbox.root / "bin"
    install_fake_bin(bin_dir, "claude", FAKE_CLAUDE)

    monkeypatch.setenv("FAKE_CLAUDE_LOG", str(sandbox.root / "fake_claude.log"))
    monkeypatch.setenv("GIT_OPTIONAL_LOCKS", "0")
    old_path = os.environ.get("PATH", "")
    monkeypatch.setenv("PATH", f"{bin_dir}{os.pathsep}{old_path}")
    src_root = str(pathlib.Path(__file__).resolve().parent.parent)
    existing_pp = os.environ.get("PYTHONPATH", "")
    pp = src_root + os.pathsep + existing_pp if existing_pp else src_root
    monkeypatch.setenv("PYTHONPATH", pp)

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
    """Subprocess envs for tests that spawn gremlins children."""
    _owned: list[pathlib.Path] = []

    def _child_env(overrides: dict) -> dict:
        env = dict(os.environ)
        existing = env.get("PYTHONPATH", "")
        # Prepend repo root so subprocesses import source, not an installed copy.
        src_root = str(pathlib.Path(__file__).resolve().parent.parent)
        env["PYTHONPATH"] = src_root + os.pathsep + existing if existing else src_root
        env.update(overrides)
        return env

    def _share() -> dict:
        return _child_env(
            {
                "GREMLINS_SANDBOX_ROOT": str(sandbox.root),
                "HOME": str(sandbox.home),
                "GREMLINS_PROJECT_ROOT": str(sandbox.project),
            }
        )

    def _fresh() -> _ChildSandbox:
        node_id = re.sub(r"[^\w]", "_", request.node.nodeid)[-60:]
        scratch_dir = os.environ.get("GREMLINS_SCRATCH_DIR", "/tmp")
        os.makedirs(scratch_dir, exist_ok=True)
        root = pathlib.Path(
            tempfile.mkdtemp(
                prefix=f"grem_{node_id}_child_",
                dir=scratch_dir,
            )
        )
        cs = _ChildSandbox(
            root,
            _child_env(
                {
                    "GREMLINS_SANDBOX_ROOT": str(root),
                    "HOME": str(root / "home"),
                    "GREMLINS_PROJECT_ROOT": str(root / "project"),
                }
            ),
        )
        for d in (cs.state, cs.work, cs.config, cs.home, cs.project):
            d.mkdir(parents=True)
        _owned.append(root)
        return cs

    yield types.SimpleNamespace(share=_share, fresh=_fresh)

    failed = getattr(request.node, "_sandbox_failed", False)
    for root in _owned:
        if failed:
            sys.stderr.write(f"\n[child sandbox retained] {root}\n")
        else:
            shutil.rmtree(root, ignore_errors=True)


TESTS_DIR = pathlib.Path(__file__).resolve().parent
PIPELINE_FIXTURES_DIR = TESTS_DIR / "fixtures" / "pipelines"


@pytest.fixture
def pipeline_fixtures_dir() -> pathlib.Path:
    return PIPELINE_FIXTURES_DIR


if str(TESTS_DIR) not in sys.path:
    sys.path.insert(0, str(TESTS_DIR))


@dataclasses.dataclass
class MockGremlin:
    state: State | None = None
    state_dir: pathlib.Path | None = None
    registry: Any = dataclasses.field(default=None)

    def __post_init__(self) -> None:
        if self.state is not None and self.registry is None:
            self.registry = self.state.artifacts
        if self.state_dir is None and self.state is not None:
            self.state_dir = self.state.artifact_dir.parent


def _make_gremlin_wrapper(state: State) -> MockGremlin:
    """Create a MockGremlin from a State for testing stages."""
    return MockGremlin(state=state)


# Shared minimal event stream used across test modules.
MINIMAL_EVENTS = [
    {"type": "system", "subtype": "init"},
    {"type": "result", "subtype": "success"},
]


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
    # If the test process inherits GREMLINS_GREMLIN_ID from a parent gremlin
    # (e.g. an implement stage running `python -m pytest`), gremlins.state.set_stage
    # would shell out to set-stage.sh against the parent's state.json and
    # corrupt its `stage` / `sub_stage` fields. Default-deny here; tests that
    # genuinely need GREMLINS_GREMLIN_ID set it explicitly via monkeypatch.setenv,
    # which overrides this delenv.
    monkeypatch.delenv("GREMLINS_GREMLIN_ID", raising=False)


@pytest.fixture(autouse=True)
def _clear_gremlins_overlay_env(monkeypatch):
    monkeypatch.delenv("GREMLINS_OVERLAY_DIR", raising=False)
    monkeypatch.delenv("GREMLINS_PROJECT_ROOT", raising=False)


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


def make_parent_state(data: StateData) -> State:
    if data.gremlin_id:
        artifact_dir = pathlib.Path(scratch_root(data.gremlin_id)) / "artifacts"
    else:
        artifact_dir = pathlib.Path("/tmp") / "artifacts"
    artifact_dir.mkdir(parents=True, exist_ok=True)
    return build_state(
        data=data,
        client=FakeClient(),
        artifact_dir=artifact_dir,
    )
