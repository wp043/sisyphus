use anyhow::Result;
use rusqlite::{Connection, params};
use serde::Deserialize;
use std::path::PathBuf;

/// One line of the shell-hook log: written by the zsh preexec/precmd hook
/// that `sisyphus hook zsh` installs.
#[derive(Deserialize)]
struct Entry {
    ts: i64,
    dur: i64,
    exit: i32,
    cwd: String,
    cmd: String,
}

pub fn log_path() -> PathBuf {
    dirs::data_dir().unwrap_or_default().join("sisyphus/shell.jsonl")
}

/// The zsh snippet users eval from ~/.zshrc. Logs every command with start
/// time, duration, exit code, and cwd — the fields plain HISTFILE can't give.
pub fn zsh_snippet() -> String {
    format!(
        r#"# sisyphus shell hook — `eval "$(sisyphus hook zsh)"` in ~/.zshrc
zmodload zsh/datetime 2>/dev/null
_sisyphus_log={log:?}
[[ -d ${{_sisyphus_log:h}} ]] || mkdir -p "${{_sisyphus_log:h}}"
_sisyphus_preexec() {{ _sisyphus_cmd=${{1//$'\n'/ }}; _sisyphus_start=$EPOCHSECONDS; }}
_sisyphus_precmd() {{
  local code=$?
  [[ -n $_sisyphus_cmd ]] || return 0
  print -r -- "{{\"ts\":$_sisyphus_start,\"dur\":$((EPOCHSECONDS-_sisyphus_start)),\"exit\":$code,\"cwd\":${{(qqq)PWD}},\"cmd\":${{(qqq)_sisyphus_cmd}}}}" >> "$_sisyphus_log"
  _sisyphus_cmd=
}}
autoload -Uz add-zsh-hook
add-zsh-hook preexec _sisyphus_preexec
add-zsh-hook precmd _sisyphus_precmd"#,
        log = log_path()
    )
}

/// Ingest the hook log (byte-offset cursor, like the plain history file).
/// Rows land as source 'zsh' with timestamps, durations, and failure flags —
/// strictly richer than HISTFILE parsing.
pub fn ingest(conn: &Connection) -> Result<usize> {
    let path = log_path();
    if !path.exists() {
        return Ok(0);
    }
    let bytes = std::fs::read(&path)?;
    let (mut offset, last_size): (i64, i64) = conn
        .query_row(
            "SELECT cursor, file_size FROM source_cursors WHERE source = 'zsh_hook'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, 0));
    if (bytes.len() as i64) < last_size {
        offset = 0; // log was rotated/truncated
    }
    let new = &bytes[offset as usize..];

    let mut seq: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(seq), 3000000) FROM commands WHERE source = 'zsh' AND seq >= 3000000",
            [],
            |r| r.get(0),
        )
        .unwrap_or(3_000_000);
    let mut stmt = conn.prepare_cached(
        "INSERT OR IGNORE INTO commands (source, raw, head, ts, duration_ms, cwd, failed, seq)
         VALUES ('zsh', ?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    let mut count = 0;
    for line in String::from_utf8_lossy(new).lines() {
        let Ok(e) = serde_json::from_str::<Entry>(line) else { continue };
        seq += 1;
        let head = e.cmd.split_whitespace().next().unwrap_or("").to_string();
        count += stmt.execute(params![
            e.cmd,
            head,
            e.ts,
            e.dur * 1000,
            e.cwd,
            (e.exit != 0) as i64,
            seq
        ])?;
    }
    conn.execute(
        "INSERT INTO source_cursors (source, cursor, file_size) VALUES ('zsh_hook', ?1, ?1)
         ON CONFLICT(source) DO UPDATE SET cursor = ?1, file_size = ?1",
        params![bytes.len() as i64],
    )?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hook_line() {
        let e: Entry = serde_json::from_str(
            r#"{"ts":1784000000,"dur":3,"exit":1,"cwd":"/Users/wp/x","cmd":"cargo build"}"#,
        )
        .unwrap();
        assert_eq!(e.cmd, "cargo build");
        assert_eq!(e.exit, 1);
    }

    #[test]
    fn snippet_mentions_eval_target() {
        let s = zsh_snippet();
        assert!(s.contains("add-zsh-hook preexec"));
        assert!(s.contains("shell.jsonl"));
    }
}
