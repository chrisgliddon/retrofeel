//! Connection management for the retrofeel SQLite database.
//!
//! The DB lives at `<data_dir>/retrofeel.db`. WAL mode is enabled for
//! concurrent reads + a single writer (good for a UI app where Bevy systems
//! read on the main thread and the recording/core threads write).

use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::error::DbError;
use crate::migrations;

/// A handle to the open database. Cheap to clone (`Arc<Mutex<Connection>>`
/// underneath); lock is held only for the duration of each query.
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

impl Db {
    /// Open (or create) the database at `path` and run pending migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DbError> {
        let conn = rusqlite::Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        migrations::run(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Open an in-memory database (for tests).
    pub fn open_in_memory() -> Result<Self, DbError> {
        let conn = rusqlite::Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        migrations::run(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Borrow the underlying connection. Repos use this to run queries.
    pub fn with_conn<R>(&self, f: impl FnOnce(&rusqlite::Connection) -> R) -> R {
        let conn = self.conn.lock().expect("db connection lock poisoned");
        f(&conn)
    }
}
