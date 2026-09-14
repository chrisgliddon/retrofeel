use thiserror::Error;

#[derive(Debug, Error)]
pub enum DbError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("config key {key} could not be deserialized: {source}")]
    ConfigDeserialize {
        key: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("config key {key} could not be serialized: {source}")]
    ConfigSerialize {
        key: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("SQLite pool lock poisoned")]
    Poisoned,
}
