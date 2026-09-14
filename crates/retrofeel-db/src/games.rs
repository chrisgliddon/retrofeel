//! Game metadata: the `games` table.
//!
//! Separates the metadata identity (game title, system, description, cover art)
//! from the physical ROM file. One game can have multiple ROM dumps via the
//! `roms.game_id` FK. Populated by hash-based metadata lookup (OpenVGDB) or
//! by filename fallback.

use crate::error::DbError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameRow {
    pub id: i64,
    pub system_id: String,
    pub title: String,
    pub description: Option<String>,
    pub genre: Option<String>,
    pub publisher: Option<String>,
    pub developer: Option<String>,
    pub release_date: Option<String>,
    pub cover_url: Option<String>,
    pub cover_path: Option<String>,
    pub created_at: u64,
    pub sort_title: Option<String>,
    pub user_title: Option<String>,
    pub metadata_locked: bool,
    pub favorite: bool,
    pub rating: u8,
    pub imported_at: Option<u64>,
    pub last_played: Option<u64>,
    pub play_count: u64,
    pub play_time_seconds: u64,
    pub artwork_provider: Option<String>,
    pub artwork_revision: Option<String>,
}

pub struct GamesRepo<'a> {
    db: &'a crate::Db,
}

impl<'a> GamesRepo<'a> {
    pub fn new(db: &'a crate::Db) -> Self {
        Self { db }
    }

    /// Upsert a game by (system_id, title). Returns the row id.
    pub fn upsert(&self, row: &GameRow) -> Result<i64, DbError> {
        self.db.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO games
                 (id, system_id, title, description, genre, publisher, developer,
                  release_date, cover_url, cover_path, created_at, sort_title,
                  user_title, metadata_locked, favorite, rating, imported_at,
                  last_played, play_count, play_time_seconds, artwork_provider,
                  artwork_revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                         ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)",
                rusqlite::params![
                    row.id,
                    row.system_id,
                    row.title,
                    row.description,
                    row.genre,
                    row.publisher,
                    row.developer,
                    row.release_date,
                    row.cover_url,
                    row.cover_path.as_ref().map(|p| p.to_string()),
                    row.created_at as i64,
                    row.sort_title,
                    row.user_title,
                    row.metadata_locked,
                    row.favorite,
                    row.rating,
                    row.imported_at.map(|value| value as i64),
                    row.last_played.map(|value| value as i64),
                    row.play_count as i64,
                    row.play_time_seconds as i64,
                    row.artwork_provider,
                    row.artwork_revision,
                ],
            )?;
            Ok(row.id)
        })
    }

    /// Insert a new game (auto-increment id). Returns the new row id.
    #[allow(clippy::too_many_arguments)]
    pub fn insert(
        &self,
        system_id: &str,
        title: &str,
        description: Option<&str>,
        genre: Option<&str>,
        publisher: Option<&str>,
        developer: Option<&str>,
        release_date: Option<&str>,
        cover_url: Option<&str>,
        cover_path: Option<&str>,
        created_at: u64,
    ) -> Result<i64, DbError> {
        self.db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO games
                 (system_id, title, description, genre, publisher, developer,
                  release_date, cover_url, cover_path, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                rusqlite::params![
                    system_id,
                    title,
                    description,
                    genre,
                    publisher,
                    developer,
                    release_date,
                    cover_url,
                    cover_path,
                    created_at as i64,
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    /// Find a game by its id.
    pub fn get(&self, id: i64) -> Result<Option<GameRow>, DbError> {
        self.db.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, system_id, title, description, genre, publisher, developer,
                        release_date, cover_url, cover_path, created_at, sort_title,
                        user_title, metadata_locked, favorite, rating, imported_at,
                        last_played, play_count, play_time_seconds, artwork_provider,
                        artwork_revision
                 FROM games WHERE id = ?1",
            )?;
            let mut rows = stmt.query_map([id], decode_game)?;
            if let Some(row) = rows.next() {
                Ok(Some(row?))
            } else {
                Ok(None)
            }
        })
    }

    /// Find games by system, ordered by title.
    pub fn by_system(&self, system_id: &str) -> Result<Vec<GameRow>, DbError> {
        self.db.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, system_id, title, description, genre, publisher, developer,
                        release_date, cover_url, cover_path, created_at, sort_title,
                        user_title, metadata_locked, favorite, rating, imported_at,
                        last_played, play_count, play_time_seconds, artwork_provider,
                        artwork_revision
                 FROM games WHERE system_id = ?1 ORDER BY title",
            )?;
            let rows = stmt.query_map([system_id], decode_game)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
    }

    /// Find a game by (system_id, title) — the natural key for dedup.
    pub fn find_by_system_and_title(
        &self,
        system_id: &str,
        title: &str,
    ) -> Result<Option<GameRow>, DbError> {
        self.db.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, system_id, title, description, genre, publisher, developer,
                        release_date, cover_url, cover_path, created_at, sort_title,
                        user_title, metadata_locked, favorite, rating, imported_at,
                        last_played, play_count, play_time_seconds, artwork_provider,
                        artwork_revision
                 FROM games WHERE system_id = ?1 AND title = ?2",
            )?;
            let mut rows = stmt.query_map(rusqlite::params![system_id, title], decode_game)?;
            if let Some(row) = rows.next() {
                Ok(Some(row?))
            } else {
                Ok(None)
            }
        })
    }

    pub fn set_favorite(&self, id: i64, favorite: bool) -> Result<(), DbError> {
        self.db.with_conn(|conn| {
            conn.execute(
                "UPDATE games SET favorite = ?2 WHERE id = ?1",
                rusqlite::params![id, favorite],
            )?;
            Ok(())
        })
    }

    pub fn set_rating(&self, id: i64, rating: u8) -> Result<(), DbError> {
        self.db.with_conn(|conn| {
            conn.execute(
                "UPDATE games SET rating = ?2 WHERE id = ?1",
                rusqlite::params![id, rating],
            )?;
            Ok(())
        })
    }

    pub fn record_play_session(
        &self,
        id: i64,
        played_at: u64,
        seconds: u64,
    ) -> Result<(), DbError> {
        self.db.with_conn(|conn| {
            conn.execute(
                "UPDATE games
                 SET last_played = ?2,
                     play_count = play_count + 1,
                     play_time_seconds = play_time_seconds + ?3
                 WHERE id = ?1",
                rusqlite::params![id, played_at as i64, seconds as i64],
            )?;
            Ok(())
        })
    }

    pub fn recently_added(&self, limit: usize) -> Result<Vec<GameRow>, DbError> {
        self.recent("COALESCE(imported_at, created_at)", limit)
    }

    pub fn recently_played(&self, limit: usize) -> Result<Vec<GameRow>, DbError> {
        self.recent("last_played", limit)
    }

    fn recent(&self, order_column: &str, limit: usize) -> Result<Vec<GameRow>, DbError> {
        self.db.with_conn(|conn| {
            let sql = format!(
                "SELECT id, system_id, title, description, genre, publisher, developer,
                        release_date, cover_url, cover_path, created_at, sort_title,
                        user_title, metadata_locked, favorite, rating, imported_at,
                        last_played, play_count, play_time_seconds, artwork_provider,
                        artwork_revision
                 FROM games
                 WHERE {order_column} IS NOT NULL
                 ORDER BY {order_column} DESC, title COLLATE NOCASE
                 LIMIT ?1"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map([limit as i64], decode_game)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
    }
}

fn decode_game(row: &rusqlite::Row<'_>) -> rusqlite::Result<GameRow> {
    Ok(GameRow {
        id: row.get(0)?,
        system_id: row.get(1)?,
        title: row.get(2)?,
        description: row.get(3)?,
        genre: row.get(4)?,
        publisher: row.get(5)?,
        developer: row.get(6)?,
        release_date: row.get(7)?,
        cover_url: row.get(8)?,
        cover_path: row.get(9)?,
        created_at: row.get::<_, i64>(10)? as u64,
        sort_title: row.get(11)?,
        user_title: row.get(12)?,
        metadata_locked: row.get(13)?,
        favorite: row.get(14)?,
        rating: row.get(15)?,
        imported_at: row.get::<_, Option<i64>>(16)?.map(|value| value as u64),
        last_played: row.get::<_, Option<i64>>(17)?.map(|value| value as u64),
        play_count: row.get::<_, i64>(18)? as u64,
        play_time_seconds: row.get::<_, i64>(19)? as u64,
        artwork_provider: row.get(20)?,
        artwork_revision: row.get(21)?,
    })
}
