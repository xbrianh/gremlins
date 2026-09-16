from __future__ import annotations

import pathlib
from unittest.mock import MagicMock, patch

import pytest
from _gremlins_core.schemas import Pipeline
from _gremlins_core.utils.yaml_io import YamlLoadError

import gremlins.cli as cli
import gremlins.cli.artifacts as mod


def _pipe(name="p", stages=None, land=None):
    p = MagicMock(spec=Pipeline)
    p.name = name
    p.stages = stages or []
    p.land = land
    return p


def _st(name="s", out=None, inp=None, body=None):
    st = MagicMock()
    st.name = name
    st.bind_map = out or {}
    st.interpolation_map = inp or {}
    st.body = body or []
    return st


def test_live_path(capsys, monkeypatch, tmp_path):
    tgt = "live1"
    # Create state_root entry to gate the live path.
    state_dir = tmp_path / "state" / tgt
    state_dir.mkdir(parents=True)
    artifact_dir = tmp_path / "scratch" / tgt / "artifacts"
    artifact_dir.mkdir(parents=True)
    monkeypatch.setattr(mod, "state_root", lambda: tmp_path / "state")
    monkeypatch.setattr(
        mod, "scratch_root", lambda t: tmp_path / "scratch" / (t or "direct")
    )
    rc = mod.artifacts_main([tgt])
    assert rc == 0
    out = capsys.readouterr().out
    assert out.startswith(f"live:{artifact_dir.parent / 'registry.json'}\n")


def test_static_path(capsys, monkeypatch):
    tgt = "stat1"
    pp = pathlib.Path("p.yaml")
    s1 = _st("s1", {"k?": "artifact://x", "m": "artifact://m"}, {"r": "k.sub"})
    s2 = _st("s2", {"k": "artifact://y"})
    land = _st("land", {"l": "artifact://z"})
    pipe = _pipe("myp", [s1, s2], land)
    monkeypatch.setattr(mod, "resolve_pipeline_name", lambda *_: pp)
    with patch.object(Pipeline, "from_yaml", return_value=pipe):
        rc = mod.artifacts_main([tgt])
    assert rc == 0
    out = capsys.readouterr().out
    assert "static:myp" in out
    assert "k artifact://x(artifact) p=s1,s2 c=s1" in out
    assert "m artifact://m(artifact) p=s1 c=-" in out
    assert "l artifact://z(artifact) p=land c=-" in out


def test_error_file_not_found(capsys, monkeypatch):
    with patch(
        "gremlins.cli.artifacts.resolve_pipeline_name",
        side_effect=FileNotFoundError("nf"),
    ):
        rc = mod.artifacts_main(["bad"])
    assert rc == 1
    err = capsys.readouterr().err
    assert err == "error: nf\n"


def test_error_value(capsys, monkeypatch):
    with patch(
        "gremlins.cli.artifacts.resolve_pipeline_name", side_effect=ValueError("ve")
    ):
        rc = mod.artifacts_main(["bad"])
    assert rc == 1
    err = capsys.readouterr().err
    assert err == "error: ve\n"


def test_error_yaml(capsys, monkeypatch):
    with patch(
        "gremlins.cli.artifacts.resolve_pipeline_name", side_effect=YamlLoadError("yl")
    ):
        rc = mod.artifacts_main(["bad"])
    assert rc == 1
    err = capsys.readouterr().err
    assert err == "error: yl\n"


def test_argparse_positional():
    with pytest.raises(SystemExit) as exc:
        mod.artifacts_main([])
    assert exc.value.code == 2


def test_cli_dispatch(monkeypatch):
    calls = []
    monkeypatch.setattr(
        cli, "_DISPATCH", {"artifacts": ("d", lambda a: calls.append(a) or 0)}
    )
    rc = cli.main(["artifacts", "t"])
    assert rc == 0
    assert calls == [["t"]]
