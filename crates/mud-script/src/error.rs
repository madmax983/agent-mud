//! Script errors.

use thiserror::Error;

/// Why scripts could not be loaded or reloaded.
#[derive(Debug, Error)]
pub enum ScriptError {
    /// Every problem the checks found, at once.
    #[error("the scripts are invalid:\n  - {}", .0.join("\n  - "))]
    Invalid(Vec<String>),
    /// A file could not be read.
    #[error("reading {path}: {source}")]
    Io {
        /// The path.
        path: String,
        /// The cause.
        source: std::io::Error,
    },
    /// The compiled-in pack has no folder to reload from.
    #[error("the bundled pack cannot be reloaded; set [world] pack to a folder")]
    Bundled,
}

impl ScriptError {
    /// The problems, one per line, for an HTTP answer or the CLI.
    #[must_use]
    pub fn problems(&self) -> Vec<String> {
        match self {
            Self::Invalid(p) => p.clone(),
            other => vec![other.to_string()],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_problem_is_listed() {
        let e = ScriptError::Invalid(vec!["a".into(), "b".into()]);
        assert_eq!(e.to_string(), "the scripts are invalid:\n  - a\n  - b");
        assert_eq!(e.problems(), ["a", "b"]);
        assert_eq!(
            ScriptError::Bundled.problems(),
            ["the bundled pack cannot be reloaded; set [world] pack to a folder"]
        );
    }
}
