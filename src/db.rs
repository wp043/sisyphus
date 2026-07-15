use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::PathBuf;

pub fn db_path() -> Result<PathBuf> {
    let dir = dirs::data_dir()
        .context("no data dir")?
        .join("sisyphus");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("sisyphus.db"))
}

pub fn open() -> Result<Connection> {
    let conn = Connection::open(db_path()?)?;
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
        "#,
    )?;
    Ok(conn)
}
