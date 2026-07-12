use pyo3::prelude::*;

/// A placeholder function to verify the PyO3 module loads correctly.
#[pyfunction]
fn sum(a: i64, b: i64) -> i64 {
    a + b
}

/// The version of the native extension.
#[pyfunction]
fn __version__() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The `_gremlins_core` native extension module.
#[pymodule(name = "_gremlins_core")]
fn _gremlins_core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(sum, m)?)?;
    m.add_function(wrap_pyfunction!(__version__, m)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sum() {
        assert_eq!(sum(2, 3), 5);
        assert_eq!(sum(-1, 1), 0);
        assert_eq!(sum(0, 0), 0);
    }
}
