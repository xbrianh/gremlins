//! The fixed set of task kinds a `Task` tool invocation can name.
//!
//! `TaskKind` is the single source of truth for the nine allowed spellings.
//! Tool dispatch, `task-clients` config parsing, and task-client model
//! selection all validate through this one enum — none of them keep an
//! independent string list.

/// The canonical labels a `Task` tool call can carry as its `kind`.
///
/// These are the exact spellings accepted in `settings.yaml` `task-clients`
/// keys and in the `Task` tool's `kind` argument. Matching is exact and
/// case-sensitive: no case-folding, no suffix matching, no fallback from an
/// unknown value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskKind {
    Scout,
    Implement,
    Research,
    Verify,
    Fix,
    Review,
    Write,
    Refactor,
    Fork,
}

impl TaskKind {
    /// The canonical string form of this kind.
    pub fn as_str(self) -> &'static str {
        match self {
            TaskKind::Scout => "Scout",
            TaskKind::Implement => "Implement",
            TaskKind::Research => "Research",
            TaskKind::Verify => "Verify",
            TaskKind::Fix => "Fix",
            TaskKind::Review => "Review",
            TaskKind::Write => "Write",
            TaskKind::Refactor => "Refactor",
            TaskKind::Fork => "Fork",
        }
    }

    /// Parse a `TaskKind` from its exact canonical spelling.
    ///
    /// Returns `None` for any other value — including case variants, suffixes,
    /// and arbitrary description text.
    pub fn parse(s: &str) -> Option<TaskKind> {
        match s {
            "Scout" => Some(TaskKind::Scout),
            "Implement" => Some(TaskKind::Implement),
            "Research" => Some(TaskKind::Research),
            "Verify" => Some(TaskKind::Verify),
            "Fix" => Some(TaskKind::Fix),
            "Review" => Some(TaskKind::Review),
            "Write" => Some(TaskKind::Write),
            "Refactor" => Some(TaskKind::Refactor),
            "Fork" => Some(TaskKind::Fork),
            _ => None,
        }
    }

    /// The nine canonical spellings, in declaration order.
    pub const ALL: [TaskKind; 9] = [
        TaskKind::Scout,
        TaskKind::Implement,
        TaskKind::Research,
        TaskKind::Verify,
        TaskKind::Fix,
        TaskKind::Review,
        TaskKind::Write,
        TaskKind::Refactor,
        TaskKind::Fork,
    ];
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
}
