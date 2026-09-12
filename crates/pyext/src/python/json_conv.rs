use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyList, PyTuple};

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
    if obj.is_none() {
        return Ok(serde_json::Value::Null);
    }
    // bool before i64 — Python bool is a subclass of int
    if let Ok(b) = obj.extract::<bool>() {
        return Ok(serde_json::Value::Bool(b));
    }
    if let Ok(i) = obj.extract::<i64>() {
        return Ok(serde_json::Value::Number(i.into()));
    }
    if let Ok(u) = obj.extract::<u64>() {
        return Ok(serde_json::Value::Number(u.into()));
    }
    if let Ok(f) = obj.extract::<f64>() {
        return float_to_value(f);
    }
    if let Ok(s) = obj.extract::<String>() {
        return Ok(serde_json::Value::String(s));
    }
    if let Ok(list) = obj.cast::<PyList>() {
        return seq_to_value(list.iter());
    }
    if let Ok(tuple) = obj.cast::<PyTuple>() {
        return seq_to_value(tuple.iter());
    }
    if let Ok(dict) = obj.cast::<PyDict>() {
        let mut map = serde_json::Map::new();
        for (k, v) in dict.iter() {
            map.insert(key_to_string(&k)?, py_to_value(&v)?);
        }
        return Ok(serde_json::Value::Object(map));
    }
    Err(PyValueError::new_err("value cannot be represented as JSON"))
}

fn seq_to_value<'py, I>(items: I) -> PyResult<serde_json::Value>
where
    I: Iterator<Item = Bound<'py, PyAny>>,
{
    let mut vals = Vec::new();
    for item in items {
        vals.push(py_to_value(&item)?);
    }
    Ok(serde_json::Value::Array(vals))
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
    if let Ok(i) = k.extract::<i64>() {
        return Ok(i.to_string());
    }
    if let Ok(u) = k.extract::<u64>() {
        return Ok(u.to_string());
    }
    if let Ok(f) = k.extract::<f64>() {
        if f.is_nan() {
            return Ok("NaN".to_string());
        }
        if f.is_infinite() {
            return Ok(if f > 0.0 { "Infinity" } else { "-Infinity" }.to_string());
        }
        return serde_json::Number::from_f64(f)
            .map(|n| n.to_string())
            .ok_or_else(|| PyValueError::new_err("unrepresentable dict key"));
    }
    Err(PyValueError::new_err(
        "dict key is not a JSON-serializable scalar",
    ))
}
