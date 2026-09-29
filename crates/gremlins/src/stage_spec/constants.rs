use std::collections::BTreeSet;
use std::sync::LazyLock;

/// Variable names reserved for framework-substitution and excluded from
/// stage interpolation maps.
pub static FRAMEWORK_KEYS: LazyLock<BTreeSet<&'static str>> =
    LazyLock::new(|| BTreeSet::from(["name", "model", "cwd", "base_ref"]));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framework_keys_contains_expected() {
        let keys = &*FRAMEWORK_KEYS;
        assert!(keys.contains("name"));
        assert!(keys.contains("model"));
        assert!(keys.contains("cwd"));
        assert!(keys.contains("base_ref"));
        assert_eq!(keys.len(), 4);
    }

    #[test]
    fn framework_keys_is_sorted() {
        let keys: Vec<_> = FRAMEWORK_KEYS.iter().collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
    }
}
