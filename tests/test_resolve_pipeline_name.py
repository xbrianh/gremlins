import os
import pathlib

import pytest

from gremlins.pipeline import BUNDLED_PIPELINE_DIR, resolve_pipeline_name


def test_hit_bundled(tmp_path: pathlib.Path) -> None:
    bundled_name = next(BUNDLED_PIPELINE_DIR.glob("*.yaml")).stem
    result = resolve_pipeline_name(bundled_name, tmp_path)
    assert result == (BUNDLED_PIPELINE_DIR / f"{bundled_name}.yaml").resolve()


def test_hit_project_local(tmp_path: pathlib.Path) -> None:
    pipelines_dir = tmp_path / ".gremlins"
    pipelines_dir.mkdir(parents=True)
    (pipelines_dir / "mypipe.yaml").write_text("name: mypipe\nstages: []\n")
    result = resolve_pipeline_name("mypipe", tmp_path)
    assert result == (pipelines_dir / "mypipe.yaml").resolve()


def test_project_shadows_bundled(tmp_path: pathlib.Path) -> None:
    bundled_name = next(BUNDLED_PIPELINE_DIR.glob("*.yaml")).stem
    pipelines_dir = tmp_path / ".gremlins"
    pipelines_dir.mkdir(parents=True)
    shadow = pipelines_dir / f"{bundled_name}.yaml"
    shadow.write_text("name: shadow\nstages: []\n")
    result = resolve_pipeline_name(bundled_name, tmp_path)
    assert result == shadow.resolve()


def test_miss_raises_with_suggestions(tmp_path: pathlib.Path) -> None:
    pipelines_dir = tmp_path / ".gremlins"
    pipelines_dir.mkdir(parents=True)
    (pipelines_dir / "alpha.yaml").write_text("name: alpha\nstages: []\n")
    with pytest.raises(FileNotFoundError) as exc_info:
        resolve_pipeline_name("nonexistent", tmp_path)
    msg = str(exc_info.value)
    assert "nonexistent" in msg
    assert "alpha" in msg
    bundled_name = next(BUNDLED_PIPELINE_DIR.glob("*.yaml")).stem
    assert bundled_name in msg


@pytest.fixture(scope="module", autouse=True)
def _preset_overlay_env():
    prev = os.environ.get("GREMLINS_OVERLAY_DIR")
    os.environ["GREMLINS_OVERLAY_DIR"] = "/tmp/sentinel-overlay"
    yield
    if prev is None:
        os.environ.pop("GREMLINS_OVERLAY_DIR", None)
    else:
        os.environ["GREMLINS_OVERLAY_DIR"] = prev


def test_overlay_env_cleared_by_autouse_fixture() -> None:
    assert os.environ.get("GREMLINS_OVERLAY_DIR") is None
