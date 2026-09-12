"""Edge-case coverage for the native Python<->JSON conversions.

The `json` module round-trips were replaced with Rust helpers
(`crates/pyext/src/python/json_conv.rs`). These lock in the semantics that
`json.dumps`/`json.loads` used to provide: tuples become arrays, scalar dict
keys are stringified, and integers outside i64 survive intact.
"""

import json
import pathlib

import pytest
from _gremlins_core.executor import read_state_json, write_state
from _gremlins_core.stages import Exec


def _round_trip(tmp_path: pathlib.Path, data: dict) -> dict:
    write_state(tmp_path, data)
    return read_state_json(tmp_path / "state.json")


def test_tuple_becomes_array(tmp_path):
    assert _round_trip(tmp_path, {"v": (1, 2, 3)})["v"] == [1, 2, 3]


def test_nested_tuple_in_dict(tmp_path):
    out = _round_trip(tmp_path, {"v": {"inner": ("a", "b")}})
    assert out["v"]["inner"] == ["a", "b"]


def test_non_string_dict_keys_stringified(tmp_path):
    keys = {3: "x", 4.5: "y", True: "z", None: "n"}
    assert _round_trip(tmp_path, {"v": keys})["v"] == json.loads(json.dumps(keys))


def test_large_unsigned_ints_survive(tmp_path):
    out = _round_trip(tmp_path, {"big": 2**63, "ubig": 2**64 - 1})
    assert out["big"] == 2**63 and isinstance(out["big"], int)
    assert out["ubig"] == 2**64 - 1 and isinstance(out["ubig"], int)


def test_exec_with_dict_tuples_in_options():
    stage = Exec.with_dict({"name": "s", "options": {"list": (1, 2)}})
    assert stage is not None


def test_exec_with_dict_int_keys_in_options():
    stage = Exec.with_dict({"name": "s", "options": {"m": {1: "a"}}})
    assert stage is not None


def test_exec_with_dict_rejects_nonserializable():
    with pytest.raises(ValueError):
        Exec.with_dict({"name": "s", "options": {"obj": object()}})
