/// Attributes shared by composite stages (Sequence, Parallel) and
/// duck-typed test stages.
#[derive(Debug, Clone, PartialEq)]
pub struct StageAttrs {
    pub name: String,
    pub stage_type: String,
    pub path: String,
    pub client_explicit: bool,
    pub skip_if_exists: String,
}

impl StageAttrs {
    pub fn new(name: String) -> Self {
        StageAttrs {
            name,
            stage_type: String::new(),
            path: String::new(),
            client_explicit: false,
            skip_if_exists: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_attrs_defaults() {
        let s = StageAttrs::new("my-stage".into());
        assert_eq!(s.name, "my-stage");
        assert_eq!(s.stage_type, "");
        assert_eq!(s.path, "");
        assert!(!s.client_explicit);
        assert_eq!(s.skip_if_exists, "");
    }
}
