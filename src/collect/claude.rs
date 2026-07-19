use anyhow::Result;
use rusqlite::{Connection, params};
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// Parse "2026-07-15T15:03:44.818Z" to unix seconds without pulling in chrono.
pub fn parse_iso(ts: &str) -> Option<i64> {
    let b = ts.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |s: &str| s.parse::<i64>().ok();
    let (y, mo, d) = (num(&ts[0..4])?, num(&ts[5..7])?, num(&ts[8..10])?);
    let (h, mi, s) = (num(&ts[11..13])?, num(&ts[14..16])?, num(&ts[17..19])?);
    // days-from-civil (Howard Hinnant)
    let y_adj = if mo <= 2 { y - 1 } else { y };
    let era = y_adj.div_euclid(400);
    let yoe = y_adj - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + h * 3600 + mi * 60 + s)
}

fn project_dirs() -> Result<Vec<PathBuf>> {
    let root = dirs::home_dir().unwrap_or_default().join(".claude/projects");
    let mut dirs_out = Vec::new();
    if root.is_dir() {
        for entry in std::fs::read_dir(root)? {
            let p = entry?.path();
            if p.is_dir() {
                dirs_out.push(p);
            }
        }
    }
    Ok(dirs_out)
}

/// Ingest all transcripts. Returns (new commands, new prompts).
pub fn ingest(conn: &Connection) -> Result<(usize, usize)> {
    let mut cmds = 0;
    let mut prompts = 0;
    for dir in project_dirs()? {
        let project = dir.file_name().unwrap_or_default().to_string_lossy().into_owned();
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                let (c, p) = ingest_file(conn, &path, &project)?;
                cmds += c;
                prompts += p;
            }
        }
    }
    Ok((cmds, prompts))
}

fn ingest_file(conn: &Connection, path: &Path, project: &str) -> Result<(usize, usize)> {
    let source_key = format!("claude:{}", path.display());
    let session_key = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
    let start_line: i64 = conn
        .query_row(
            "SELECT cursor FROM source_cursors WHERE source = ?1",
            params![source_key],
            |r| r.get(0),
        )
        .unwrap_or(0);

    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut cmd_stmt = conn.prepare_cached(
        "INSERT OR IGNORE INTO commands (source, raw, head, ts, cwd, project, session_key, seq)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;

    let (mut cmds, mut prompts) = (0usize, 0usize);
    let mut call_rows: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut line_no: i64 = -1;
    for line in reader.lines() {
        line_no += 1;
        let line = match line {
            Ok(l) => l,
            Err(_) => continue, // tolerate non-UTF8 junk lines
        };
        if line_no < start_line {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        let ts = v["timestamp"].as_str().and_then(parse_iso);
        let cwd = v["cwd"].as_str();
        match v["type"].as_str() {
            Some("assistant") => {
                let Some(content) = v["message"]["content"].as_array() else { continue };
                for block in content {
                    if block["type"] == "tool_use" && block["name"] == "Bash"
                        && let Some(cmd) = block["input"]["command"].as_str() {
                            let head = cmd.split_whitespace().next().unwrap_or("");
                            let n = cmd_stmt.execute(params![
                                "claude", cmd, head, ts, cwd, project, session_key, line_no
                            ])?;
                            cmds += n;
                            if n > 0
                                && let Some(id) = block["id"].as_str() {
                                    call_rows.insert(id.to_string(), conn.last_insert_rowid());
                                }
                        }
                }
            }
            Some("user") => {
                // tool results ride in user messages; mark failed commands
                if let Some(blocks) = v["message"]["content"].as_array() {
                    for block in blocks {
                        if block["type"] == "tool_result" && block["is_error"] == true
                            && let Some(row) = block["tool_use_id"]
                                .as_str()
                                .and_then(|id| call_rows.get(id))
                            {
                                let snippet = match &block["content"] {
                                    Value::String(s) => Some(s.as_str()),
                                    Value::Array(bs) => bs
                                        .iter()
                                        .find_map(|b| (b["type"] == "text").then(|| b["text"].as_str()).flatten()),
                                    _ => None,
                                }
                                .map(|s| s.chars().take(300).collect::<String>());
                                conn.execute(
                                    "UPDATE commands SET failed = 1, error_snippet = ?2 WHERE id = ?1",
                                    params![row, snippet],
                                )?;
                            }
                    }
                }
                if v["isMeta"].as_bool() == Some(true) {
                    continue;
                }
                let text = match &v["message"]["content"] {
                    Value::String(s) => Some(s.clone()),
                    Value::Array(blocks) => blocks.iter().find_map(|b| {
                        (b["type"] == "text").then(|| b["text"].as_str().unwrap_or("").to_string())
                    }),
                    _ => None,
                };
                if let Some(text) = text {
                    let t = text.trim();
                    // skip tool results, caveats, and command wrappers
                    if !t.is_empty() && !t.starts_with('<') && !t.starts_with('[') {
                        prompts += cmd_stmt.execute(params![
                            "claude_prompt", t, "", ts, cwd, project, session_key, line_no
                        ])?;
                    }
                }
            }
            _ => {}
        }
    }

    let size = std::fs::metadata(path).map(|m| m.len() as i64).unwrap_or(0);
    conn.execute(
        "INSERT INTO source_cursors (source, cursor, file_size) VALUES (?1, ?2, ?3)
         ON CONFLICT(source) DO UPDATE SET cursor = ?2, file_size = ?3",
        params![source_key, line_no + 1, size],
    )?;
    Ok((cmds, prompts))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_parse() {
        // spot-checked against `date -u -d @1752591824`
        assert_eq!(parse_iso("2026-07-15T15:03:44.818Z"), Some(1784127824));
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
    }
}
