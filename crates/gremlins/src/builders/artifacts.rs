//! Thin newtypes for interpolation values and bind targets.
//!
//! These carry the string representation the stage types already store, so
//! callers can write `content("artifact://plan.md")` and
//! `artifact("artifact://plan.md")` without worrying about the internal
//! `content(...)` wrapper syntax.

/// An interpolation value — the right-hand side of an interpolation map entry.
///
/// ```ignore
/// content("artifact://plan.md")
/// // stores: content("artifact://plan.md")
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterpolationValue(pub String);

/// An output target — the right-hand side of an outputs map entry.
///
/// ```ignore
/// output("artifact://plan.md")
/// // stores: artifact://plan.md
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputTarget(pub String);

/// Build an interpolation value with `content()` wrapping.
///
/// ```ignore
/// content("artifact://plan.md")  // → content("artifact://plan.md")
/// ```
pub fn content(uri: impl Into<String>) -> InterpolationValue {
    let uri = uri.into();
    InterpolationValue(format!("content(\"{uri}\")"))
}

/// Build an output target (bare artifact URI).
///
/// ```ignore
/// output("artifact://plan.md")  // → artifact://plan.md
/// ```
pub fn output(uri: impl Into<String>) -> OutputTarget {
    OutputTarget(uri.into())
}

impl From<InterpolationValue> for String {
    fn from(v: InterpolationValue) -> Self {
        v.0
    }
}

impl From<OutputTarget> for String {
    fn from(v: OutputTarget) -> Self {
        v.0
    }
}

impl From<&str> for InterpolationValue {
    fn from(s: &str) -> Self {
        InterpolationValue(s.to_string())
    }
}

impl From<String> for InterpolationValue {
    fn from(s: String) -> Self {
        InterpolationValue(s)
    }
}

impl From<&str> for OutputTarget {
    fn from(s: &str) -> Self {
        OutputTarget(s.to_string())
    }
}

impl From<String> for OutputTarget {
    fn from(s: String) -> Self {
        OutputTarget(s)
    }
}
