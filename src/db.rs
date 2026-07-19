use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::PathBuf;

pub fn db_path() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("SISYPHUS_DB") {
        return Ok(PathBuf::from(p));
    }
    let dir = dirs::data_dir()
        .context("no data dir")?
        .join("sisyphus");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("sisyphus.db"))
}

/// True when using the real default DB (no --db flag or SISYPHUS_DB override).
pub fn is_default_db() -> bool {
    std::env::var("SISYPHUS_DB").is_err()
}

pub fn open() -> Result<Connection> {
    let conn = Connection::open(db_path()?)?;
    init_schema(&conn)?;
    Ok(conn)
}

/// Apply schema + migrations. Public so tests can use in-memory connections.
pub fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        PRAGMA journal_mode = WAL;

        CREATE TABLE IF NOT EXISTS commands (
            id          INTEGER PRIMARY KEY,
            source      TEXT NOT NULL,          -- 'zsh' | 'claude'
            raw         TEXT NOT NULL,
            template    TEXT,                   -- normalized form, filled by normalizer
            head        TEXT,                   -- first word of the command
            ts          INTEGER,                -- unix seconds; NULL when the source has no timestamps
            duration_ms INTEGER,
            cwd         TEXT,
            project     TEXT,                   -- claude project slug
            session_key TEXT,                   -- claude transcript file stem; NULL for zsh
            seq         INTEGER NOT NULL,       -- order within (source, session_key)
            UNIQUE(source, session_key, seq)
        );
        CREATE INDEX IF NOT EXISTS idx_commands_head ON commands(head);
        CREATE INDEX IF NOT EXISTS idx_commands_template ON commands(template);

        -- one row per ingestion source ('zsh' or 'claude:<file>'); cursor is a byte
        -- offset (zsh) or line count (claude) so re-ingestion is incremental
        CREATE TABLE IF NOT EXISTS source_cursors (
            source TEXT PRIMARY KEY,
            cursor INTEGER NOT NULL,
            file_size INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS patterns (
            id           INTEGER PRIMARY KEY,
            kind         TEXT NOT NULL,         -- 'sequence' | 'fixloop'
            template_seq TEXT NOT NULL UNIQUE,  -- JSON array of templates
            count        INTEGER NOT NULL,
            score        REAL NOT NULL,
            first_ts     INTEGER,
            last_ts      INTEGER
        );

        CREATE TABLE IF NOT EXISTS decisions (
            pattern_id    INTEGER PRIMARY KEY REFERENCES patterns(id),
            decision      TEXT NOT NULL,        -- 'accepted' | 'ignored'
            artifact_path TEXT,
            ts            INTEGER NOT NULL
        );

        -- patterns the ambient scan has already notified about
        CREATE TABLE IF NOT EXISTS notified (
            pattern_id INTEGER PRIMARY KEY REFERENCES patterns(id),
            ts         INTEGER NOT NULL
        );
        "#,
    )?;
    // migrations for DBs created before these columns existed
    migrate(conn);
    Ok(())
}

fn migrate(conn: &Connection) {
    let _ = conn.execute("ALTER TABLE commands ADD COLUMN failed INTEGER", []);
    // where history stood when the user decided — everything after is "since"
    let _ = conn.execute("ALTER TABLE decisions ADD COLUMN at_command_id INTEGER", []);
    let _ = conn.execute("ALTER TABLE decisions ADD COLUMN count_at_decision INTEGER", []);
    // first ~300 chars of failure output, for fix-loop draft context
    let _ = conn.execute("ALTER TABLE commands ADD COLUMN error_snippet TEXT", []);
}

/// Record the user's decision on a pattern, snapshotting where history stands
/// so `evolve` can later see what happened after.
pub fn decide(conn: &Connection, pattern_id: i64, decision: &str, path: Option<String>) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let max_cmd: i64 = conn.query_row("SELECT COALESCE(MAX(id), 0) FROM commands", [], |r| r.get(0))?;
    let count: i64 = conn
        .query_row("SELECT count FROM patterns WHERE id = ?1", [pattern_id], |r| r.get(0))
        .unwrap_or(0);
    conn.execute(
        "INSERT OR REPLACE INTO decisions
         (pattern_id, decision, artifact_path, ts, at_command_id, count_at_decision)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![pattern_id, decision, path, now, max_cmd, count],
    )?;
    Ok(())
}
