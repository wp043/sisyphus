use anyhow::Result;
use rusqlite::{Connection, params};
use serde_json::Value;

/// Gemini CLI stores only prompt logs (no shell commands) in
/// ~/.gemini/tmp/<project-hash>/logs.json as a JSON array of messages.
pub fn ingest(conn: &Connection) -> Result<usize> {
    let root = dirs::home_dir().unwrap_or_default().join(".gemini/tmp");
    let mut prompts = 0;
    let Ok(entries) = std::fs::read_dir(&root) else { return Ok(0) };
    for entry in entries.flatten() {
        let log = entry.path().join("logs.json");
        if !log.is_file() {
            continue;
        }
        let source_key = format!("gemini:{}", log.display());
        let done: i64 = conn
            .query_row(
                "SELECT cursor FROM source_cursors WHERE source = ?1",
                params![source_key],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let Ok(text) = std::fs::read_to_string(&log) else { continue };
        let Ok(Value::Array(msgs)) = serde_json::from_str::<Value>(&text) else { continue };
        let session = entry.file_name().to_string_lossy().into_owned();

        let mut stmt = conn.prepare_cached(
            "INSERT OR IGNORE INTO commands (source, raw, head, ts, session_key, seq)
             VALUES ('gemini_prompt', ?1, '', ?2, ?3, ?4)",
        )?;
        for (i, m) in msgs.iter().enumerate().skip(done as usize) {
            if m["type"] == "user"
                && let Some(t) = m["message"].as_str() {
                    let ts = m["timestamp"].as_str().and_then(crate::collect::claude::parse_iso);
                    prompts += stmt.execute(params![t.trim(), ts, session, i as i64])?;
                }
        }
        conn.execute(
            "INSERT INTO source_cursors (source, cursor, file_size) VALUES (?1, ?2, ?3)
             ON CONFLICT(source) DO UPDATE SET cursor = ?2, file_size = ?3",
            params![source_key, msgs.len() as i64, text.len() as i64],
        )?;
    }
    Ok(prompts)
}
