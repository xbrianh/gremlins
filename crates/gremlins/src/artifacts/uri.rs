use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Uri {
    pub scheme: String,
    pub path: String,
}

impl Uri {
    pub fn new(scheme: String, path: String) -> Self {
        Uri { scheme, path }
    }

    pub fn parse(s: &str) -> Result<Self, UriError> {
        let (scheme, path) = s
            .split_once("://")
            .ok_or_else(|| UriError::MissingSeparator(s.to_string()))?;
        Ok(Uri {
            scheme: scheme.to_string(),
            path: path.to_string(),
        })
    }
}

impl fmt::Display for Uri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}://{}", self.scheme, self.path)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UriError {
    #[error("invalid URI (missing '://'): {0:?}")]
    MissingSeparator(String),
    #[error("unknown scheme {scheme:?}; known schemes: {known:?}")]
    UnknownScheme { scheme: String, known: Vec<String> },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_roundtrip_artifact() {
        let uri = Uri::parse("artifact://plan.md").unwrap();
        assert_eq!(uri.scheme, "artifact");
        assert_eq!(uri.path, "plan.md");
        assert_eq!(uri.to_string(), "artifact://plan.md");
    }

    #[test]
    fn test_parse_roundtrip_artifact_nested() {
        let uri = Uri::parse("artifact://commit/abc123").unwrap();
        assert_eq!(uri.to_string(), "artifact://commit/abc123");
    }

    #[test]
    fn test_parse_unknown_scheme() {
        // All schemes are now accepted by parse()
        let uri = Uri::parse("file://session/foo.md").unwrap();
        assert_eq!(uri.scheme, "file");
    }

    #[test]
    fn test_parse_missing_separator() {
        assert!(Uri::parse("no-slashes").is_err());
    }

    #[test]
    fn test_display_roundtrip() {
        let uri = Uri::new("artifact".to_string(), "plan.md".to_string());
        assert_eq!(uri.to_string(), "artifact://plan.md");
    }
}
