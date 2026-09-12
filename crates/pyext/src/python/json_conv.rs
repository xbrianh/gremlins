use std::collections::HashSet;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyInt, PyList, PyTuple};

/// serde_json::Value -> Python, preserving ints up to u64 exactly.
pub fn value_to_py(py: Python<'_>, v: &serde_json::Value) -> PyResult<Py<PyAny>> {
    match v {
        serde_json::Value::Null => Ok(py.None()),
        serde_json::Value::Bool(b) => Ok(PyBool::new(py, *b).to_owned().into_any().unbind()),
        serde_json::Value::Number(n) => number_to_py(py, n),
        serde_json::Value::String(s) => Ok(s.into_pyobject(py)?.into_any().unbind()),
        serde_json::Value::Array(arr) => {
            let items: Vec<Py<PyAny>> = arr
                .iter()
                .map(|item| value_to_py(py, item))
                .collect::<PyResult<_>>()?;
            Ok(PyList::new(py, items)?.into())
        }
        serde_json::Value::Object(obj) => {
            let dict = PyDict::new(py);
            for (k, v) in obj {
                dict.set_item(k, value_to_py(py, v)?)?;
            }
            Ok(dict.into())
        }
    }
}

fn number_to_py(py: Python<'_>, n: &serde_json::Number) -> PyResult<Py<PyAny>> {
    if let Some(i) = n.as_i64() {
        Ok(i.into_pyobject(py)?.into_any().unbind())
    } else if let Some(u) = n.as_u64() {
        Ok(u.into_pyobject(py)?.into_any().unbind())
    } else if let Some(f) = n.as_f64() {
        Ok(f.into_pyobject(py)?.into_any().unbind())
    } else {
        Err(PyValueError::new_err("unsupported JSON number"))
    }
}

/// Python -> serde_json::Value, matching `json.dumps` for the supported types:
/// tuples become arrays and non-string scalar dict keys are stringified.
pub fn py_to_value(obj: &Bound<'_, PyAny>) -> PyResult<serde_json::Value> {
    py_to_value_inner(obj, &mut HashSet::new())
}

fn py_to_value_inner(
    obj: &Bound<'_, PyAny>,
    active: &mut HashSet<usize>,
) -> PyResult<serde_json::Value> {
    if obj.is_none() {
        return Ok(serde_json::Value::Null);
    }
    // bool before int — Python bool is a subclass of int
    if let Ok(b) = obj.extract::<bool>() {
        return Ok(serde_json::Value::Bool(b));
    }
    if let Ok(i) = obj.extract::<i64>() {
        return Ok(serde_json::Value::Number(i.into()));
    }
    if let Ok(u) = obj.extract::<u64>() {
        return Ok(serde_json::Value::Number(u.into()));
    }
    // `extract::<f64>` also accepts ints, so reject out-of-range ints explicitly
    // rather than rounding them into a different JSON value.
    if obj.is_instance_of::<PyInt>() {
        return Err(PyValueError::new_err(
            "integer too large to represent in JSON",
        ));
    }
    if let Ok(f) = obj.extract::<f64>() {
        return float_to_value(f);
    }
    if let Ok(s) = obj.extract::<String>() {
        return Ok(serde_json::Value::String(s));
    }
    if let Ok(list) = obj.cast::<PyList>() {
        return descend(obj, active, |active| seq_to_value(list.iter(), active));
    }
    if let Ok(tuple) = obj.cast::<PyTuple>() {
        return descend(obj, active, |active| seq_to_value(tuple.iter(), active));
    }
    if let Ok(dict) = obj.cast::<PyDict>() {
        return descend(obj, active, |active| dict_to_value(dict, active));
    }
    Err(PyValueError::new_err("value cannot be represented as JSON"))
}

/// Tracks `obj` as an active container while running `f`, rejecting cycles
/// (matching `json.dumps`'s "Circular reference detected") while still allowing
/// the same container to appear multiple times when it is not self-referential.
fn descend<T>(
    obj: &Bound<'_, PyAny>,
    active: &mut HashSet<usize>,
    f: impl FnOnce(&mut HashSet<usize>) -> PyResult<T>,
) -> PyResult<T> {
    let ptr = obj.as_ptr() as usize;
    if !active.insert(ptr) {
        return Err(PyValueError::new_err("Circular reference detected"));
    }
    let out = f(active);
    active.remove(&ptr);
    out
}

fn seq_to_value<'py, I>(items: I, active: &mut HashSet<usize>) -> PyResult<serde_json::Value>
where
    I: Iterator<Item = Bound<'py, PyAny>>,
{
    let mut vals = Vec::new();
    for item in items {
        vals.push(py_to_value_inner(&item, active)?);
    }
    Ok(serde_json::Value::Array(vals))
}

fn dict_to_value(
    dict: &Bound<'_, PyDict>,
    active: &mut HashSet<usize>,
) -> PyResult<serde_json::Value> {
    let mut map = serde_json::Map::new();
    for (k, v) in dict.iter() {
        map.insert(key_to_string(&k)?, py_to_value_inner(&v, active)?);
    }
    Ok(serde_json::Value::Object(map))
}

fn float_to_value(f: f64) -> PyResult<serde_json::Value> {
    if !f.is_finite() {
        return Err(PyValueError::new_err(
            "JSON does not support NaN or Infinity",
        ));
    }
    serde_json::Number::from_f64(f)
        .map(serde_json::Value::Number)
        .ok_or_else(|| PyValueError::new_err("cannot represent float as JSON"))
}

/// Stringify a dict key the way `json.dumps` does: via Python's `int`/`float`
/// repr so that arbitrary-precision ints and exponent-form floats (e.g.
/// `1e-06`) keep their exact spelling.
pub fn key_to_string(k: &Bound<'_, PyAny>) -> PyResult<String> {
    if let Ok(s) = k.extract::<String>() {
        return Ok(s);
    }
    if k.is_none() {
        return Ok("null".to_string());
    }
    if let Ok(b) = k.extract::<bool>() {
        return Ok(if b { "true" } else { "false" }.to_string());
    }
    if k.is_instance_of::<PyInt>() {
        return Ok(k.repr()?.to_string());
    }
    if k.is_instance_of::<PyFloat>() {
        let f: f64 = k.extract()?;
        if f.is_nan() {
            return Ok("NaN".to_string());
        }
        if f.is_infinite() {
            return Ok(if f > 0.0 { "Infinity" } else { "-Infinity" }.to_string());
        }
        return Ok(k.repr()?.to_string());
    }
    Err(PyValueError::new_err(
        "dict key is not a JSON-serializable scalar",
    ))
}
