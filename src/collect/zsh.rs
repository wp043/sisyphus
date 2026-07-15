use anyhow::Result;
use rusqlite::{Connection, params};
use std::path::Path;

/// zsh "metafies" history: 0x83 is an escape byte, the following byte is XOR'd
/// with 0x20. Undo that before decoding.
fn unmetafy(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut iter = bytes.iter().copied();
    while let Some(b) = iter.next() {
        if b == 0x83 {
            if let Some(next) = iter.next() {
                out.push(next ^ 0x20);
            }
        } else {
            out.push(b);
        }
    }
    out
}

pub struct ZshEntry {
    pub cmd: String,
    pub ts: Option<i64>,
    pub duration_ms: Option<i64>,
}

/// Parse history file contents. Supports both plain and EXTENDED_HISTORY
/// (`: <start>:<elapsed>;<cmd>`) formats; backslash-newline continuations are
/// joined into a single command.
pub fn parse(bytes: &[u8]) -> Vec<ZshEntry> {
    let text = String::from_utf8_lossy(&unmetafy(bytes)).into_owned();
    let mut entries = Vec::new();
    let mut pending = String::new();

    for line in text.split('\n') {
        if line.is_empty() && pending.is_empty() {
            continue;
        }
        pending.push_str(line);
        // odd number of trailing backslashes = escaped newline, command continues
        let trailing = pending.len() - pending.trim_end_matches('\\').len();
        if trailing % 2 == 1 {
            pending.pop();
            pending.push('\n');
            continue;
        }
        let full = std::mem::take(&mut pending);
        if full.trim().is_empty() {
            continue;
        }
        entries.push(parse_entry(&full));
    }
    entries
}

fn parse_entry(line: &str) -> ZshEntry {
    if let Some(rest) = line.strip_prefix(": ") {
        if let Some((meta, cmd)) = rest.split_once(';') {
            if let Some((ts, dur)) = meta.split_once(':') {
                if let (Ok(ts), Ok(dur)) = (ts.trim().parse::<i64>(), dur.trim().parse::<i64>()) {
                    return ZshEntry {
                        cmd: cmd.to_string(),
                        ts: Some(ts),
                        duration_ms: Some(dur * 1000),
                    };
                }
            }
        }
    }
    ZshEntry { cmd: line.to_string(), ts: None, duration_ms: None }
}

/// Incrementally ingest the history file. Returns number of new commands.
pub fn ingest(conn: &Connection, path: &Path) -> Result<usize> {
    let bytes = std::fs::read(path)?;
    let (mut offset, last_size): (i64, i64) = conn
        .query_row(
            "SELECT cursor, file_size FROM source_cursors WHERE source = 'zsh'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, 0));

    // file shrank => history was rewritten/trimmed; re-ingest from scratch
    if (bytes.len() as i64) < last_size {
        conn.execute("DELETE FROM commands WHERE source = 'zsh'", [])?;
        offset = 0;
    }
    let new = &bytes[offset as usize..];
    if new.is_empty() {
        return Ok(0);
    }

    let mut seq: i64 = conn.query_row(
        "SELECT COALESCE(MAX(seq), -1) FROM commands WHERE source = 'zsh'",
        [],
        |r| r.get(0),
    )?;

    let entries = parse(new);
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO commands (source, raw, head, ts, duration_ms, seq)
         VALUES ('zsh', ?1, ?2, ?3, ?4, ?5)",
    )?;
    let mut count = 0;
    for e in &entries {
        seq += 1;
        let head = e.cmd.split_whitespace().next().unwrap_or("").to_string();
        count += stmt.execute(params![e.cmd, head, e.ts, e.duration_ms, seq])?;
    }

    conn.execute(
        "INSERT INTO source_cursors (source, cursor, file_size) VALUES ('zsh', ?1, ?1)
         ON CONFLICT(source) DO UPDATE SET cursor = ?1, file_size = ?1",
        params![bytes.len() as i64],
    )?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_lines() {
        let e = parse(b"ls\ncd src\n");
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].cmd, "ls");
        assert!(e[0].ts.is_none());
    }

    #[test]
    fn extended_history() {
        let e = parse(b": 1720000000:2;cargo build\n");
        assert_eq!(e[0].cmd, "cargo build");
        assert_eq!(e[0].ts, Some(1720000000));
        assert_eq!(e[0].duration_ms, Some(2000));
    }

    #[test]
    fn backslash_continuation() {
        let e = parse(b"npm install -g npm@11.3\\\n.0\nls\n");
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].cmd, "npm install -g npm@11.3\n.0");
        assert_eq!(e[1].cmd, "ls");
    }

    #[test]
    fn metafied_bytes_survive() {
        // 0x83 followed by 0xa5 decodes to 0x85; just ensure no panic and lossy utf8
        let e = parse(&[b'e', b'c', b'h', b'o', b' ', 0x83, 0xa5, b'\n']);
        assert_eq!(e.len(), 1);
        assert!(e[0].cmd.starts_with("echo "));
    }
}
