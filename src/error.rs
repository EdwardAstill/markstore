use thiserror::Error;

#[derive(Error, Debug)]
pub enum MksError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("Serialization error: {0}")]
    Serialize(#[from] serde_json::Error),

    #[error("Document not found: {0}")]
    NotFound(String),

    #[error("Graph node not found: '{0}'")]
    NodeNotFound(String),

    #[error("No path between '{0}' and '{1}'")]
    NoPath(String, String),

    #[error("Store not initialized at {0} — run `mks init` first")]
    NotInitialized(String),

    #[error("Glob error: {0}")]
    Glob(#[from] glob::PatternError),

    #[error("{0}")]
    Other(String),
}

pub type MksResult<T> = Result<T, MksError>;
