use anyhow::Result;
use rusqlite::{Connection, params};
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

fn session_files() -> Vec<PathBuf> {
    let root = dirs::home_dir().unwrap_or_default().join(".codex/sessions");
    let mut files = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                files.push(p);
            }
        }
    }
    files
}

/// Extract the shell command from a codex function_call payload. Newer codex
/// uses `exec_command` with a `cmd` string; older versions used `shell` with
/// a `command` argv array (typically ["bash","-lc","<cmd>"]).
fn extract_command(payload: &Value) -> Option<String> {
    let name = payload["name"].as_str()?;
    if !matches!(name, "exec_command" | "shell" | "local_shell" | "container.exec") {
        return None;
    }
    let args: Value = serde_json::from_str(payload["arguments"].as_str()?).ok()?;
    if let Some(cmd) = args["cmd"].as_str() {
        return Some(cmd.to_string());
    }
    if let Some(argv) = args["command"].as_array() {
        let parts: Vec<&str> = argv.iter().filter_map(|v| v.as_str()).collect();
        return match parts.as_slice() {
            [_, "-lc" | "-c", script] => Some(script.to_string()),
            _ => Some(parts.join(" ")),
        };
    }
    None
}

/// Parse Codex's exec-output wrapper into (exit code, real output body).
/// Format: "Chunk ID: …\nWall time: …\nProcess exited with code N\n
/// Original token count: …\nOutput:\n<body>". Returns None for non-exec
/// outputs (e.g. web-search results) that carry no exit code.
fn parse_result(output: &str) -> Option<(i32, String)> {
    let code: i32 = output
        .lines()
        .find_map(|l| l.strip_prefix("Process exited with code "))?
        .trim()
        .parse()
        .ok()?;
    let body = output
        .split_once("Output:\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    Some((code, body))
}

pub fn ingest(conn: &Connection) -> Result<(usize, usize)> {
    let (mut cmds, mut prompts) = (0usize, 0usize);
    for path in session_files() {
        let (c, p) = ingest_file(conn, &path)?;
        cmds += c;
        prompts += p;
    }
    Ok((cmds, prompts))
}

fn ingest_file(conn: &Connection, path: &Path) -> Result<(usize, usize)> {
    let source_key = format!("codex:{}", path.display());
    let session_key = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
    let start_line: i64 = conn
        .query_row(
            "SELECT cursor FROM source_cursors WHERE source = ?1",
            params![source_key],
            |r| r.get(0),
        )
        .unwrap_or(0);

    let reader = BufReader::new(std::fs::File::open(path)?);
    let mut stmt = conn.prepare_cached(
        "INSERT OR IGNORE INTO commands (source, raw, head, ts, cwd, project, session_key, seq)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;

    let (mut cmds, mut prompts) = (0usize, 0usize);
    let mut cwd: Option<String> = None;
    let mut call_rows: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut line_no: i64 = -1;
    for line in reader.lines() {
        line_no += 1;
        let Ok(line) = line else { continue };
        if line_no < start_line {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        let ts = v["timestamp"].as_str().and_then(crate::collect::claude::parse_iso);
        let payload = &v["payload"];
        match (v["type"].as_str(), payload["type"].as_str()) {
            (Some("session_meta"), _) => {
                cwd = payload["cwd"].as_str().map(String::from);
            }
            (Some("response_item"), Some("function_call")) => {
                if let Some(cmd) = extract_command(payload) {
                    let head = cmd.split_whitespace().next().unwrap_or("").to_string();
                    let n = stmt.execute(params![
                        "codex", cmd, head, ts, cwd, Option::<String>::None, session_key, line_no
                    ])?;
                    cmds += n;
                    if n > 0
                        && let Some(id) = payload["call_id"].as_str() {
                            call_rows.insert(id.to_string(), conn.last_insert_rowid());
                        }
                }
            }
            (Some("response_item"), Some("function_call_output")) => {
                // Codex wraps exec output as "…Process exited with code N…
                // Output:\n<body>". Judge failure by the real exit code, not a
                // substring (which matched every "exited with code 0" success).
                if let (Some((code, body)), Some(row)) = (
                    payload["output"].as_str().and_then(parse_result),
                    payload["call_id"].as_str().and_then(|id| call_rows.get(id)),
                ) && code != 0
                {
                    let snippet: Option<String> = {
                        let s: String = body.trim().chars().take(300).collect();
                        (!s.is_empty()).then_some(s)
                    };
                    conn.execute(
                        "UPDATE commands SET failed = 1, error_snippet = ?2 WHERE id = ?1",
                        params![row, snippet],
                    )?;
                }
            }
            (Some("event_msg"), Some("user_message")) => {
                if let Some(text) = payload["message"].as_str() {
                    let t = text.trim();
                    if !t.is_empty() {
                        prompts += stmt.execute(params![
                            "codex_prompt", t, "", ts, cwd, Option::<String>::None, session_key, line_no
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
    use serde_json::json;

    #[test]
    fn extracts_exec_command() {
        let p = json!({"name":"exec_command","arguments":"{\"cmd\":\"pwd\",\"workdir\":\"/x\"}"});
        assert_eq!(extract_command(&p).as_deref(), Some("pwd"));
    }

    #[test]
    fn extracts_legacy_shell_argv() {
        let p = json!({"name":"shell","arguments":"{\"command\":[\"bash\",\"-lc\",\"cargo build\"]}"});
        assert_eq!(extract_command(&p).as_deref(), Some("cargo build"));
    }

    #[test]
    fn ignores_other_tools() {
        let p = json!({"name":"apply_patch","arguments":"{}"});
        assert_eq!(extract_command(&p), None);
    }

    #[test]
    fn exit_code_zero_is_not_a_failure() {
        let out = "Chunk ID: abc\nWall time: 0.5 seconds\nProcess exited with code 0\nOriginal token count: 85\nOutput:\nfile1\nfile2\n";
        assert_eq!(parse_result(out), Some((0, "file1\nfile2\n".into())));
    }

    #[test]
    fn nonzero_exit_captures_body() {
        let out = "Chunk ID: x\nWall time: 8.7 seconds\nProcess exited with code 1\nOriginal token count: 42\nOutput:\nerror[E0502]: cannot borrow `x`\n";
        let (code, body) = parse_result(out).unwrap();
        assert_eq!(code, 1);
        assert!(body.contains("E0502"));
    }

    #[test]
    fn non_exec_output_has_no_code() {
        assert_eq!(parse_result("[{\"type\":\"input_text\"}]"), None);
    }
}
