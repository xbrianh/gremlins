//! The fixed set of task kinds a `Task` tool invocation can name.
//!
//! `TaskKind` is the single source of truth for the three allowed spellings.
//! Tool dispatch, `task-models` config parsing, and task-model
//! selection all validate through this one enum — none of them keep an
//! independent string list.

/// The canonical labels a `Task` tool call can carry as its `kind`.
///
/// These are the exact spellings accepted in `settings.yaml` `task-models`
/// keys and in the `Task` tool's `kind` argument. Matching is exact and
/// case-sensitive: no case-folding, no suffix matching, no fallback from an
/// unknown value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskKind {
    Fork,
    Scout,
    Implement,
}

impl TaskKind {
    /// The canonical string form of this kind.
    pub fn as_str(self) -> &'static str {
        match self {
            TaskKind::Fork => "Fork",
            TaskKind::Scout => "Scout",
            TaskKind::Implement => "Implement",
        }
    }

    /// Parse a `TaskKind` from its exact canonical spelling.
    ///
    /// Returns `None` for any other value — including case variants, suffixes,
    /// and arbitrary description text.
    pub fn parse(s: &str) -> Option<TaskKind> {
        match s {
            "Fork" => Some(TaskKind::Fork),
            "Scout" => Some(TaskKind::Scout),
            "Implement" => Some(TaskKind::Implement),
            _ => None,
        }
    }

    /// The canonical spellings that may be configured through `task-models`.
    /// `Fork` is intentionally excluded: it always inherits the parent model
    /// and is rejected as a configuration key.
    pub const CONFIGURABLE: [TaskKind; 2] = [TaskKind::Scout, TaskKind::Implement];

    /// The three canonical spellings, in declaration order.
    pub const ALL: [TaskKind; 3] = [TaskKind::Fork, TaskKind::Scout, TaskKind::Implement];
}

impl std::fmt::Display for TaskKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_spellings_round_trip() {
        for kind in TaskKind::ALL {
            assert_eq!(TaskKind::parse(kind.as_str()), Some(kind));
        }
    }

    #[test]
    fn parse_is_strict() {
        assert_eq!(TaskKind::parse("scout"), None);
        assert_eq!(TaskKind::parse("Scout*"), None);
        assert_eq!(TaskKind::parse("Scout the codebase"), None);
        assert_eq!(TaskKind::parse(""), None);
        assert_eq!(TaskKind::parse("Bogus"), None);
    }

    #[test]
    fn fork_spelling() {
        assert_eq!(TaskKind::parse("Fork"), Some(TaskKind::Fork));
        assert_eq!(TaskKind::Fork.as_str(), "Fork");
    }

    #[test]
    fn configurable_excludes_fork() {
        assert_eq!(TaskKind::CONFIGURABLE.len(), 2);
        assert!(!TaskKind::CONFIGURABLE.contains(&TaskKind::Fork));
        for kind in TaskKind::CONFIGURABLE {
            assert_eq!(TaskKind::parse(kind.as_str()), Some(kind));
        }
    }
}
