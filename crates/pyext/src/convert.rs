//! The Python ↔ serde_yaml bridge.
//!
//! [`pyval_to_serde`] carries Python values *in* — the direction the schema
//! constructors need when a caller hands them a dict built in Python. Its
//! mirror [`serde_to_pyval`] carries parsed YAML *out*, so a document read in
//! Rust can be returned to Python with its shapes intact. The round trip is
//! expected to preserve numbers exactly: `i64` stays an `int`, never a string,
//! and `f64` stays a `float`, never an `int`.

use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyList};

use gremlins::core::discovery::DiscoveryError;

pub fn discovery_error_to_pyerr(e: DiscoveryError) -> pyo3::PyErr {
    pyo3::exceptions::PyFileNotFoundError::new_err(e.to_string())
}

pub fn pyval_to_serde(obj: &Bound<'_, PyAny>) -> PyResult<serde_yaml::Value> {
    if obj.is_none() {
        Ok(serde_yaml::Value::Null)
    } else if let Ok(s) = obj.extract::<String>() {
        Ok(serde_yaml::Value::String(s))
    } else if let Ok(b) = obj.extract::<bool>() {
        Ok(serde_yaml::Value::Bool(b))
    } else if let Ok(i) = obj.extract::<i64>() {
        Ok(serde_yaml::Value::Number(serde_yaml::Number::from(i)))
    } else if let Ok(u) = obj.extract::<u64>() {
        // After `i64`: a Python `int` in `(i64::MAX, u64::MAX]` is still an
        // integer, and must not be widened to a lossy float below.
        Ok(serde_yaml::Value::Number(serde_yaml::Number::from(u)))
    } else if let Ok(f) = obj.extract::<f64>() {
        // After the integer branches, so an `int` is never widened to a
        // float: `f64` extraction accepts integers too, and would otherwise
        // swallow them.
        Ok(serde_yaml::Value::Number(serde_yaml::Number::from(f)))
    } else if let Ok(d) = obj.cast::<PyDict>() {
        let mut mapping = serde_yaml::Mapping::new();
        for (k, v) in d.iter() {
            let k_str: String = k.extract()?;
            mapping.insert(serde_yaml::Value::String(k_str), pyval_to_serde(&v)?);
        }
        Ok(serde_yaml::Value::Mapping(mapping))
    } else if let Ok(l) = obj.cast::<PyList>() {
        let mut seq = Vec::new();
        for item in l.iter() {
            seq.push(pyval_to_serde(&item)?);
        }
        Ok(serde_yaml::Value::Sequence(seq))
    } else {
        Ok(serde_yaml::Value::String(obj.to_string()))
    }
}

/// Convert a [`serde_yaml::Value`] into the equivalent Python object.
///
/// Mapping keys are rendered as strings: the schema layer only ever keys by
/// name, and coercing here keeps an exotic key (`1: x`) from raising deep
/// inside a dump rather than surfacing as an odd name.
pub fn serde_to_pyval(py: Python<'_>, value: &serde_yaml::Value) -> PyResult<Py<PyAny>> {
    match value {
        serde_yaml::Value::Null => Ok(py.None()),
        serde_yaml::Value::Bool(b) => Ok(PyBool::new(py, *b).to_owned().into_any().unbind()),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(i.into_pyobject(py)?.into_any().unbind())
            } else if let Some(u) = n.as_u64() {
                // An unsigned value above `i64::MAX` is still an integer; keep
                // it exact as a Python `int` rather than a lossy float.
                Ok(u.into_pyobject(py)?.into_any().unbind())
            } else if let Some(f) = n.as_f64() {
                Ok(f.into_pyobject(py)?.into_any().unbind())
            } else {
                Ok(py.None())
            }
        }
        serde_yaml::Value::String(s) => Ok(s.into_pyobject(py)?.into_any().unbind()),
        serde_yaml::Value::Sequence(seq) => {
            let list = PyList::empty(py);
            for item in seq {
                list.append(serde_to_pyval(py, item)?)?;
            }
            Ok(list.into_any().unbind())
        }
        serde_yaml::Value::Mapping(mapping) => {
            let dict = PyDict::new(py);
            for (k, v) in mapping {
                let key = match k {
                    serde_yaml::Value::String(s) => s.clone(),
                    other => format!("{other:?}"),
                };
                dict.set_item(key, serde_to_pyval(py, v)?)?;
            }
            Ok(dict.into_any().unbind())
        }
        serde_yaml::Value::Tagged(tagged) => serde_to_pyval(py, &tagged.value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_preserve_type_and_precision() {
        Python::attach(|py| {
            let value: serde_yaml::Value = serde_yaml::from_str("i: 7\nf: 1.5\n").unwrap();

            let converted = serde_to_pyval(py, &value).unwrap();
            assert!(converted
                .bind(py)
                .get_item("i")
                .unwrap()
                .is_instance_of::<pyo3::types::PyInt>());
            assert!(converted
                .bind(py)
                .get_item("f")
                .unwrap()
                .is_instance_of::<pyo3::types::PyFloat>());
            assert_eq!(
                converted
                    .bind(py)
                    .get_item("f")
                    .unwrap()
                    .extract::<f64>()
                    .unwrap(),
                1.5
            );
        });
    }

    #[test]
    fn large_unsigned_integers_stay_exact() {
        Python::attach(|py| {
            // `u64::MAX` is beyond `i64::MAX`, so it exercises the unsigned
            // branch in both directions of the bridge.
            let big = u64::MAX;
            let source = serde_yaml::Value::Number(serde_yaml::Number::from(big));

            let pyval = serde_to_pyval(py, &source).unwrap();
            assert!(pyval.bind(py).is_instance_of::<pyo3::types::PyInt>());
            assert_eq!(pyval.bind(py).extract::<u64>().unwrap(), big);

            assert_eq!(pyval_to_serde(pyval.bind(py)).unwrap(), source);
        });
    }

    #[test]
    fn pyval_serde_round_trip_keeps_nested_shapes() {
        Python::attach(|py| {
            let source: serde_yaml::Value =
                serde_yaml::from_str("a:\n  b:\n    - 1\n    - two\nc: true\nd: 1.5\n").unwrap();

            let pyval = serde_to_pyval(py, &source).unwrap();
            let back = pyval_to_serde(pyval.bind(py)).unwrap();

            assert_eq!(back, source);
        });
    }
}
