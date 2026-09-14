//! Helpers for exposing Rust async work as Python coroutine functions.
//!
//! PyO3's `future_into_py` returns an `asyncio.Future` bound to the *current*
//! event loop, so it cannot be created before a loop exists. Stage runners,
//! however, must be constructible outside a running loop — callers do
//! `asyncio.run(runner())`, evaluating `runner()` before the loop starts.
//!
//! We therefore expose runners as `#[pyclass(dict)]` instances whose `__call__`
//! is an `async fn`: PyO3 builds a loop-independent coroutine when the instance
//! is called. To satisfy `inspect.iscoroutinefunction` — which inspects
//! `__code__`, `__name__`, `__defaults__` and `__kwdefaults__` — we copy those
//! attributes from a genuine coroutine function onto the instance.

use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::PyDict;

static COROUTINE_MARKER: PyOnceLock<Py<PyAny>> = PyOnceLock::new();

/// A genuine coroutine function used as the source of `__code__` and friends.
///
/// Defined once, lazily, in a throwaway namespace so it never lands in
/// `sys.modules`.
fn coroutine_marker(py: Python<'_>) -> PyResult<&Py<PyAny>> {
    COROUTINE_MARKER.get_or_try_init(py, || {
        let namespace = PyDict::new(py);
        py.run(
            c"async def _gremlins_coroutine_marker():\n    pass\n",
            None,
            Some(&namespace),
        )?;
        Ok(namespace
            .get_item("_gremlins_coroutine_marker")?
            .expect("marker defined above")
            .unbind())
    })
}

/// Make `obj` (a `#[pyclass(dict)]` instance) pass `inspect.iscoroutinefunction`.
pub(crate) fn mark_as_coroutine_function(py: Python<'_>, obj: &Bound<'_, PyAny>) -> PyResult<()> {
    let marker = coroutine_marker(py)?.bind(py);
    for attr in ["__code__", "__name__", "__defaults__", "__kwdefaults__"] {
        if let Ok(value) = marker.getattr(attr) {
            obj.setattr(attr, value)?;
        }
    }
    Ok(())
}
