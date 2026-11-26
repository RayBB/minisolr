//! Error type shared by all modules.

use std::fmt;

/// Everything that can go wrong while loading data or answering a request.
#[derive(Debug)]
pub enum Error {
    /// Reading a file or socket failed.
    Io(std::io::Error),
    /// A document or fixture was not valid JSON.
    Json(serde_json::Error),
    /// Tantivy refused an operation.
    Tantivy(tantivy::TantivyError),
    /// A schema or enums file was not well-formed XML.
    Xml(roxmltree::Error),
    /// The schema is well-formed but inconsistent (for example an undefined field type).
    Schema(String),
    /// The request uses a Solr feature the prototype does not implement.
    Unsupported(String),
    /// Anything else, with a message.
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::Json(e) => write!(f, "json error: {e}"),
            Self::Tantivy(e) => write!(f, "tantivy error: {e}"),
            Self::Xml(e) => write!(f, "xml error: {e}"),
            Self::Schema(msg) => write!(f, "schema error: {msg}"),
            Self::Unsupported(what) => write!(f, "unsupported: {what}"),
            Self::Other(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Json(e) => Some(e),
            Self::Tantivy(e) => Some(e),
            Self::Xml(e) => Some(e),
            Self::Schema(_) | Self::Unsupported(_) | Self::Other(_) => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

impl From<tantivy::TantivyError> for Error {
    fn from(e: tantivy::TantivyError) -> Self {
        Self::Tantivy(e)
    }
}

impl From<roxmltree::Error> for Error {
    fn from(e: roxmltree::Error) -> Self {
        Self::Xml(e)
    }
}
