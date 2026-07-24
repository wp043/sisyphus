use anyhow::Result;
use regex::Regex;
use rusqlite::{Connection, params};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::LazyLock;

/// One line of the shell-hook log: written by the zsh preexec/precmd hook
/// that `sisyphus hook zsh` installs.
#[derive(Deserialize)]
struct Entry {
    ts: i64,
    dur: i64,
    exit: i32,
    cwd: String,
    cmd: String,
    /// stderr of a failed command, present only with `hook zsh --errors`
    #[serde(default)]
    err: Option<String>,
}

pub fn log_path() -> PathBuf {
    dirs::data_dir().unwrap_or_default().join("sisyphus/shell.jsonl")
}

fn err_log_path() -> PathBuf {
    log_path().with_file_name("stderr.log")
}

const BASE_SNIPPET: &str = r#"# sisyphus shell hook — `eval "$(sisyphus hook zsh)"` in ~/.zshrc
zmodload zsh/datetime 2>/dev/null
_sisyphus_log="__LOG__"
[[ -d ${_sisyphus_log:h} ]] || mkdir -p "${_sisyphus_log:h}"
_sisyphus_preexec() { _sisyphus_cmd=${1//$'\n'/ }; _sisyphus_start=$EPOCHSECONDS; }
_sisyphus_precmd() {
  local code=$?
  [[ -n $_sisyphus_cmd ]] || return 0
  print -r -- "{\"ts\":$_sisyphus_start,\"dur\":$((EPOCHSECONDS-_sisyphus_start)),\"exit\":$code,\"cwd\":${(qqq)PWD},\"cmd\":${(qqq)_sisyphus_cmd}}" >> "$_sisyphus_log"
  _sisyphus_cmd=
}
autoload -Uz add-zsh-hook
add-zsh-hook preexec _sisyphus_preexec
add-zsh-hook precmd _sisyphus_precmd"#;

// Extends the base hook to capture a failed command's stderr. Teeing stderr
// makes fd 2 a pipe, so live progress bars on stderr render as plain output —
// hence opt-in. Control chars are stripped here so ${(qqq)} stays JSON-valid;
// residual ANSI params are cleaned in Rust at ingest.
const ERR_SNIPPET: &str = r#"# sisyphus shell hook (+stderr capture) — `eval "$(sisyphus hook zsh --errors)"`
# note: routes stderr through a tee; progress bars on stderr may look plain.
# revert with plain `sisyphus hook zsh`.
zmodload zsh/datetime 2>/dev/null
_sisyphus_log="__LOG__"
_sisyphus_err="__ERR__"
[[ -d ${_sisyphus_log:h} ]] || mkdir -p "${_sisyphus_log:h}"
[[ -e $_sisyphus_err ]] || : > "$_sisyphus_err"
exec 2> >(tee -a "$_sisyphus_err" >&2)
_sisyphus_preexec() {
  _sisyphus_cmd=${1//$'\n'/ }
  _sisyphus_start=$EPOCHSECONDS
  _sisyphus_errmark=$(wc -c < "$_sisyphus_err" 2>/dev/null || echo 0)
}
_sisyphus_precmd() {
  local code=$?
  [[ -n $_sisyphus_cmd ]] || return 0
  local err=""
  (( code != 0 )) && err=$(tail -c +$((_sisyphus_errmark+1)) "$_sisyphus_err" 2>/dev/null | tr -cd '[:print:] ' | tail -c 400)
  print -r -- "{\"ts\":$_sisyphus_start,\"dur\":$((EPOCHSECONDS-_sisyphus_start)),\"exit\":$code,\"cwd\":${(qqq)PWD},\"cmd\":${(qqq)_sisyphus_cmd},\"err\":${(qqq)err}}" >> "$_sisyphus_log"
  (( $(wc -c < "$_sisyphus_err" 2>/dev/null || echo 0) > 262144 )) && : > "$_sisyphus_err"
  _sisyphus_cmd=
}
autoload -Uz add-zsh-hook
add-zsh-hook preexec _sisyphus_preexec
add-zsh-hook precmd _sisyphus_precmd"#;

/// The zsh snippet users eval from ~/.zshrc. Logs every command with start
/// time, duration, exit code, and cwd. With `capture_errors`, also records a
/// failed command's stderr (opt-in — see ERR_SNIPPET's caveat).
pub fn zsh_snippet(capture_errors: bool) -> String {
    let log = log_path().to_string_lossy().into_owned();
    if capture_errors {
        ERR_SNIPPET
            .replace("__LOG__", &log)
            .replace("__ERR__", &err_log_path().to_string_lossy())
    } else {
        BASE_SNIPPET.replace("__LOG__", &log)
    }
}

/// Strip ANSI residue left after the hook removed the ESC byte (e.g. "[0m"),
/// so shell error snippets read cleanly and feed failure-signature mining.
fn clean_err(raw: &str) -> Option<String> {
    static CSI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[[0-9;?]*[A-Za-z]").unwrap());
    let cleaned = CSI.replace_all(raw, "");
    let trimmed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    (!trimmed.is_empty()).then_some(trimmed)
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
        "INSERT OR IGNORE INTO commands (source, raw, head, ts, duration_ms, cwd, failed, error_snippet, seq)
         VALUES ('zsh', ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    let mut count = 0;
    for line in String::from_utf8_lossy(new).lines() {
        let Ok(e) = serde_json::from_str::<Entry>(line) else { continue };
        seq += 1;
        let head = e.cmd.split_whitespace().next().unwrap_or("").to_string();
        let err = e.err.as_deref().and_then(clean_err);
        count += stmt.execute(params![
            e.cmd,
            head,
            e.ts,
            e.dur * 1000,
            e.cwd,
            (e.exit != 0) as i64,
            err,
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
        // base line (no err field) still parses
        let e: Entry = serde_json::from_str(
            r#"{"ts":1784000000,"dur":3,"exit":1,"cwd":"/Users/wp/x","cmd":"cargo build"}"#,
        )
        .unwrap();
        assert_eq!(e.cmd, "cargo build");
        assert_eq!(e.exit, 1);
        assert!(e.err.is_none());
        // capture line carries stderr
        let e2: Entry = serde_json::from_str(
            r#"{"ts":1,"dur":2,"exit":1,"cwd":"/x","cmd":"rg foo(","err":"rg: regex parse error"}"#,
        )
        .unwrap();
        assert_eq!(e2.err.as_deref(), Some("rg: regex parse error"));
    }

    #[test]
    fn cleans_ansi_residue() {
        assert_eq!(clean_err("[0m[31merror[0m: boom"), Some("error: boom".into()));
        assert_eq!(clean_err("   "), None);
    }

    #[test]
    fn snippet_variants() {
        let base = zsh_snippet(false);
        assert!(base.contains("add-zsh-hook preexec"));
        assert!(base.contains("shell.jsonl"));
        assert!(!base.contains("tee -a"));
        let errs = zsh_snippet(true);
        assert!(errs.contains("tee -a"));
        assert!(errs.contains("stderr.log"));
        assert!(errs.contains("\\\"err\\\":"));
    }
}
