use std::collections::HashMap;

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

use crate::schemas::error::SchemaError;
use gremlins::schemas::loader as core_loader;

pub const STAGE_TYPES: &[(&str, &str, &str)] = &[
    ("agent", "_gremlins_core.stages", "Agent"),
    ("loop", "_gremlins_core.stages", "Loop"),
    ("parallel", "gremlins.stages.parallel", "ParallelStage"),
    ("sequence", "_gremlins_core.stages", "Sequence"),
    ("exec", "_gremlins_core.stages", "Exec"),
];

fn lookup_stage_class(
    stage_type: &str,
    name: &str,
) -> Result<(&'static str, &'static str), SchemaError> {
    for &(st, module, class) in STAGE_TYPES {
        if st == stage_type {
            return Ok((module, class));
        }
    }
    Err(SchemaError::Stage {
        name: name.to_string(),
        msg: format!("unknown type {stage_type:?}"),
    })
}

pub fn parse_stage(py: Python<'_>, d: &Bound<'_, PyDict>, depth: usize) -> PyResult<Py<PyAny>> {
    if d.contains("parallel")? {
        let cls = py
            .import("gremlins.stages.parallel")?
            .getattr("ParallelStage")?;
        let stage: Py<PyAny> = cls.call_method1("with_dict", (d, depth))?.extract()?;
        let name: String = d
            .get_item("name")?
            .and_then(|v| v.extract().ok())
            .unwrap_or_else(|| "<parallel>".to_string());
        let skip_if_exists = parse_skip_if_exists(d, &name)?;
        stage.setattr(py, "raw_dict", d)?;
        stage.setattr(py, "skip_if_exists", skip_if_exists)?;
        return Ok(stage);
    }

    let name: String = d
        .get_item("name")?
        .and_then(|v| v.extract().ok())
        .unwrap_or_default();

    if d.contains("max_concurrent")? {
        return Err(pyo3::exceptions::PyValueError::new_err(format!(
            "stage {name:?}: 'max_concurrent' is only valid on parallel groups"
        )));
    }

    let stage_type: Option<String> = d.get_item("type")?.and_then(|v| v.extract().ok());

    let stage_type = match stage_type {
        Some(t) if !t.is_empty() => t,
        _ => {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "stage {name:?}: must have a 'type' field"
            )));
        }
    };

    // First try the built-in Rust constant
    if let Ok((module, class)) = lookup_stage_class(&stage_type, &name) {
        let cls = py.import(module)?.getattr(class)?;
        let stage: Py<PyAny> = cls.call_method1("with_dict", (d, depth))?.extract()?;
        let skip_if_exists = parse_skip_if_exists(d, &name)?;
        stage.setattr(py, "raw_dict", d)?;
        stage.setattr(py, "skip_if_exists", skip_if_exists)?;
        return Ok(stage);
    }

    // Fall back to the Python STAGE_TYPES dict (which may have dynamically
    // registered types, e.g. test fixtures).
    let stage_types: Bound<'_, PyDict> = py
        .import("_gremlins_core.schemas")?
        .getattr("STAGE_TYPES")?
        .cast_into()?;
    match stage_types.get_item(&stage_type)? {
        Some(cls) => {
            let stage: Py<PyAny> = cls.call_method1("with_dict", (d, depth))?.extract()?;
            let skip_if_exists = parse_skip_if_exists(d, &name)?;
            stage.setattr(py, "raw_dict", d)?;
            stage.setattr(py, "skip_if_exists", skip_if_exists)?;
            Ok(stage)
        }
        None => Err(pyo3::exceptions::PyValueError::new_err(format!(
            "stage {name:?}: unknown type {stage_type:?}"
        ))),
    }
}

fn parse_skip_if_exists(d: &Bound<'_, PyDict>, name: &str) -> PyResult<String> {
    let raw = d.get_item("skip_if_exists")?;
    match raw {
        None => Ok(String::new()),
        Some(v) => {
            let s: String = v.extract().map_err(|_| {
                pyo3::exceptions::PyValueError::new_err(format!(
                    "stage {name:?}: 'skip_if_exists' must be a string, got {} type",
                    v.get_type()
                        .name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| "?".to_string())
                ))
            })?;
            if s.is_empty() {
                Ok(String::new())
            } else {
                Ok(s)
            }
        }
    }
}

pub fn parse_stages(
    py: Python<'_>,
    raw: &Bound<'_, PyList>,
    depth: usize,
) -> PyResult<Vec<Py<PyAny>>> {
    fill_names(raw)?;
    let mut stages = Vec::new();
    for item in raw.iter() {
        let d: &Bound<'_, PyDict> = item.cast()?;
        stages.push(parse_stage(py, d, depth)?);
    }
    Ok(stages)
}

pub fn fill_names(raw_stages: &Bound<'_, PyList>) -> PyResult<()> {
    let mut entries: Vec<core_loader::StageEntry> = Vec::new();
    for item in raw_stages.iter() {
        let d: &Bound<'_, PyDict> = item.cast()?;
        let name: Option<String> = d
            .get_item("name")
            .ok()
            .flatten()
            .and_then(|v| v.extract::<String>().ok())
            .filter(|n| !n.is_empty());
        let auto_name: Option<String> = d
            .get_item("_auto_name")
            .ok()
            .flatten()
            .and_then(|v| v.str().ok())
            .and_then(|s| s.to_str().ok().map(|s| s.to_string()))
            .filter(|n| !n.is_empty());
        d.del_item("_auto_name").ok();
        let stage_type: Option<String> = d
            .get_item("type")
            .ok()
            .flatten()
            .and_then(|v| v.extract::<String>().ok())
            .filter(|t| !t.is_empty());
        let is_parallel = d.contains("parallel").unwrap_or(false);
        entries.push(core_loader::StageEntry {
            name,
            auto_name,
            stage_type,
            is_parallel,
        });
    }

    core_loader::fill_names(&mut entries).map_err(|e: gremlins::schemas::error::SchemaError| {
        pyo3::exceptions::PyValueError::new_err(e.to_string())
    })?;

    for (i, entry) in entries.iter().enumerate() {
        if let Some(ref name) = entry.name {
            let item = raw_stages.get_item(i)?;
            let d: &Bound<'_, PyDict> = item.cast()?;
            d.set_item("name", name)?;
        }
    }

    Ok(())
}

pub fn check_duplicate_producers(
    stages: &Bound<'_, PyList>,
    extra_out: Option<&Bound<'_, PyDict>>,
) -> PyResult<()> {
    let nodes = py_stages_to_nodes(stages)?;
    let extra = match extra_out {
        Some(dict) => {
            let mut m = HashMap::new();
            for (k, v) in dict.iter() {
                let key: String = k.extract()?;
                let val: String = v.extract()?;
                m.insert(key, val);
            }
            m
        }
        None => HashMap::new(),
    };
    core_loader::check_duplicate_producers(&nodes, &extra).map_err(
        |e: gremlins::schemas::error::SchemaError| {
            pyo3::exceptions::PyValueError::new_err(e.to_string())
        },
    )
}

pub fn check_unresolved_consumers(
    stages: &Bound<'_, PyList>,
    launch_cmds: &Bound<'_, PyList>,
    cli_out: Option<&Bound<'_, PyDict>>,
) -> PyResult<()> {
    let nodes = py_stages_to_nodes(stages)?;
    let launch_cmds_vec: Vec<String> = launch_cmds
        .iter()
        .filter_map(|v| v.extract::<String>().ok())
        .collect();
    let cli_out_map = match cli_out {
        Some(dict) => {
            let mut m = HashMap::new();
            for (k, v) in dict.iter() {
                let key: String = k.extract()?;
                let val: String = v.extract()?;
                m.insert(key, val);
            }
            m
        }
        None => HashMap::new(),
    };
    core_loader::check_unresolved_consumers(&nodes, &launch_cmds_vec, &cli_out_map).map_err(
        |e: gremlins::schemas::error::SchemaError| {
            pyo3::exceptions::PyValueError::new_err(e.to_string())
        },
    )
}

pub fn py_stages_to_nodes(stages: &Bound<'_, PyList>) -> PyResult<Vec<core_loader::StageNode>> {
    let mut nodes = Vec::new();
    for item in stages.iter() {
        let name: String = item.getattr("name")?.extract()?;
        let stage_type: String = item
            .getattr("type")
            .ok()
            .and_then(|v| v.extract::<String>().ok())
            .unwrap_or_default();
        let skip_if_exists: String = item
            .getattr("skip_if_exists")
            .ok()
            .and_then(|v| v.extract::<String>().ok())
            .unwrap_or_default();

        let mut bind_map = HashMap::new();
        if let Ok(bm) = item.getattr("bind_map") {
            if let Ok(bm_dict) = bm.cast::<PyDict>() {
                for (k, v) in bm_dict.iter() {
                    let key: String = k.extract()?;
                    let val: String = v.extract()?;
                    bind_map.insert(key, val);
                }
            }
        }

        let mut interpolation_map = HashMap::new();
        if let Ok(im) = item.getattr("interpolation_map") {
            if let Ok(im_dict) = im.cast::<PyDict>() {
                for (k, v) in im_dict.iter() {
                    let key: String = k.extract()?;
                    let val: String = v.extract()?;
                    interpolation_map.insert(key, val);
                }
            }
        }

        let mut body = Vec::new();
        if let Ok(body_attr) = item.getattr("body") {
            if let Ok(body_list) = body_attr.cast::<PyList>() {
                body = py_stages_to_nodes(body_list)?;
            }
        }

        nodes.push(core_loader::StageNode {
            name,
            stage_type,
            bind_map,
            interpolation_map,
            skip_if_exists,
            body,
        });
    }
    Ok(nodes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo3::types::{PyDict, PyList, PyTuple};

    #[test]
    fn test_fill_names_basic() {
        Python::attach(|py| {
            let items1 =
                PyList::new(py, vec![PyTuple::new(py, vec!["type", "agent"]).unwrap()]).unwrap();
            let dict1 = PyDict::from_sequence(&items1).unwrap();
            let items2 =
                PyList::new(py, vec![PyTuple::new(py, vec!["type", "agent"]).unwrap()]).unwrap();
            let dict2 = PyDict::from_sequence(&items2).unwrap();
            let list = PyList::new(py, [dict1, dict2]).unwrap();
            fill_names(&list).unwrap();
            let item0 = list.get_item(0).unwrap();
            let item1 = list.get_item(1).unwrap();
            let d1: &Bound<'_, PyDict> = item0.cast().unwrap();
            let d2: &Bound<'_, PyDict> = item1.cast().unwrap();
            assert_eq!(
                d1.get_item("name")
                    .unwrap()
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "agent"
            );
            assert_eq!(
                d2.get_item("name")
                    .unwrap()
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "agent-2"
            );
        });
    }

    #[test]
    fn test_fill_names_parallel() {
        Python::attach(|py| {
            let dict = PyDict::new(py);
            dict.set_item("parallel", PyList::empty(py)).unwrap();
            let list = PyList::new(py, [dict]).unwrap();
            fill_names(&list).unwrap();
            let item0 = list.get_item(0).unwrap();
            let d: &Bound<'_, PyDict> = item0.cast().unwrap();
            assert_eq!(
                d.get_item("name")
                    .unwrap()
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "parallel"
            );
        });
    }

    #[test]
    fn test_fill_names_explicit_wins() {
        Python::attach(|py| {
            let items = PyList::new(
                py,
                vec![
                    PyTuple::new(py, vec!["type", "agent"]).unwrap(),
                    PyTuple::new(py, vec!["name", "custom"]).unwrap(),
                ],
            )
            .unwrap();
            let dict = PyDict::from_sequence(&items).unwrap();
            let list = PyList::new(py, [dict]).unwrap();
            fill_names(&list).unwrap();
            let item0 = list.get_item(0).unwrap();
            let d: &Bound<'_, PyDict> = item0.cast().unwrap();
            assert_eq!(
                d.get_item("name")
                    .unwrap()
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "custom"
            );
        });
    }
}
