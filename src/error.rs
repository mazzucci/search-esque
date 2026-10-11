//! One error shape for everything the crate refuses.
//!
//! A refusal always names the `path` in the request it is about, in the same dotted form
//! Elasticsearch users read (`query.bool.filter[1].wildcard`), what is wrong, and where it
//! helps, a `hint` and the list of what is `supported` in that place. The intent is that
//! an agent or a person can fix the request from the error alone.

use serde::Serialize;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Error {
    pub path: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub supported: Vec<String>,
}

impl Error {
    pub fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Error {
            path: path.into(),
            message: message.into(),
            hint: None,
            supported: Vec::new(),
        }
    }

    /// The same error, anchored at a more specific path.
    pub fn at(mut self, path: impl Into<String>) -> Self {
        self.path = path.into();
        self
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn supported(mut self, list: &[&str]) -> Self {
        self.supported = list.iter().map(|s| s.to_string()).collect();
        self
    }

    /// The wire form: `{"error": {"path": ..., "message": ..., "hint": ..., "supported": [...]}}`.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "error": self })
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)?;
        if let Some(h) = &self.hint {
            write!(f, " ({})", h)?;
        }
        if !self.supported.is_empty() {
            write!(f, "; supported: {}", self.supported.join(", "))?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
