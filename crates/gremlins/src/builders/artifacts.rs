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

/// A bind target — the right-hand side of a bind map entry.
///
/// ```ignore
/// artifact("artifact://plan.md")
/// // stores: artifact://plan.md
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindTarget(pub String);

/// Build an interpolation value with `content()` wrapping.
///
/// ```ignore
/// content("artifact://plan.md")  // → content("artifact://plan.md")
/// ```
pub fn content(uri: impl Into<String>) -> InterpolationValue {
    let uri = uri.into();
    InterpolationValue(format!("content(\"{uri}\")"))
}

/// Build a bind target (bare artifact URI).
///
/// ```ignore
/// artifact("artifact://plan.md")  // → artifact://plan.md
/// ```
pub fn artifact(uri: impl Into<String>) -> BindTarget {
    BindTarget(uri.into())
}

impl From<InterpolationValue> for String {
    fn from(v: InterpolationValue) -> Self {
        v.0
    }
}

impl From<BindTarget> for String {
    fn from(v: BindTarget) -> Self {
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

impl From<&str> for BindTarget {
    fn from(s: &str) -> Self {
        BindTarget(s.to_string())
    }
}

impl From<String> for BindTarget {
    fn from(s: String) -> Self {
        BindTarget(s)
    }
}
