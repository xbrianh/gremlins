//! Helpers for exposing Rust async work as Python coroutine functions.
//!
//! `pyo3_async_runtimes::tokio::future_into_py` binds an `asyncio.Future` to
//! the *current* event loop, so it cannot be created before a loop exists.
//! Stage runners, however, must be constructible outside a running loop:
//! callers write `asyncio.run(runner())`, which evaluates `runner()` before the
//! loop starts.
//!
//! Runners are therefore `#[pyclass(dict)]` instances whose `__call__` is an
//! `async fn` — calling the instance yields a loop-independent coroutine, so no
//! `Future` is created until the coroutine is awaited.
//!
//! That leaves one gap: `inspect.iscoroutinefunction` inspects the callable
//! *object*, and a class instance is not function-like.
//! [`mark_as_coroutine_function`] closes it by lending the instance the
//! attributes of a genuinely compiled coroutine function, exposing the
//! `CO_COROUTINE` flag through `__code__` on every supported interpreter, and
//! by recording the sentinels that `inspect.markcoroutinefunction` and the
//! (deprecated) `asyncio.iscoroutinefunction` look for.

use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::PyDict;

static COROUTINE_MARKER: PyOnceLock<Py<PyAny>> = PyOnceLock::new();

/// Attributes that make a bare callable look like a `types.FunctionType` to
/// `inspect._signature_is_functionlike`. `__name__` is set separately, from the
/// runner's own stage rather than from the throwaway marker.
const FUNCTION_LIKE_ATTRS: [&str; 3] = ["__code__", "__defaults__", "__kwdefaults__"];

/// A genuine coroutine function used as the source of `__code__` and friends.
///
/// Compiled once, lazily, in a private namespace — which doubles as the
/// function's globals, since compiling a definition under null globals raises —
/// and never inserted into `sys.modules`.
fn coroutine_marker(py: Python<'_>) -> PyResult<&Py<PyAny>> {
    COROUTINE_MARKER.get_or_try_init(py, || {
        let namespace = PyDict::new(py);
        py.run(
            c"async def _gremlins_coroutine_marker():\n    pass\n",
            Some(&namespace),
            Some(&namespace),
        )?;
        Ok(namespace
            .get_item("_gremlins_coroutine_marker")?
            .expect("marker defined above")
            .unbind())
    })
}

/// Make `obj` — a `#[pyclass(dict)]` instance — pass
/// `inspect.iscoroutinefunction`, presenting itself as `name`.
pub(crate) fn mark_as_coroutine_function(
    py: Python<'_>,
    obj: &Bound<'_, PyAny>,
    name: &str,
) -> PyResult<()> {
    let marker = coroutine_marker(py)?.bind(py);
    for attr in FUNCTION_LIKE_ATTRS {
        obj.setattr(attr, marker.getattr(attr)?)?;
    }
    obj.setattr("__name__", name)?;
    mark_coroutine_sentinels(py, obj)
}

/// Record the sentinels that the coroutine-function predicates test for.
///
/// `inspect.markcoroutinefunction` owns `_is_coroutine_marker`; asyncio's
/// deprecated predicate owns `_is_coroutine`. Each is probed defensively so the
/// shim keeps working on interpreters that rename or remove them.
fn mark_coroutine_sentinels(py: Python<'_>, obj: &Bound<'_, PyAny>) -> PyResult<()> {
    if let Ok(mark) = py
        .import("inspect")
        .and_then(|inspect| inspect.getattr("markcoroutinefunction"))
    {
        mark.call1((obj,))?;
    }
    if let Ok(sentinel) = py
        .import("asyncio")
        .and_then(|asyncio| asyncio.getattr("coroutines"))
        .and_then(|coroutines| coroutines.getattr("_is_coroutine"))
    {
        obj.setattr("_is_coroutine", sentinel)?;
    }
    Ok(())
}
