use anyhow::Result;
use regex::Regex;
use rusqlite::Connection;
use std::sync::LazyLock;

static HEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-f]{7,40}$").unwrap());
static NUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d+$").unwrap());
static VER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^v?\d+\.\d+(\.\d+)?").unwrap());
static URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^https?://").unwrap());

/// Turn a concrete command into a reusable template: keep program names,
/// subcommands, and flags; collapse arguments that vary between runs
/// (paths, URLs, hashes, versions, bare numbers) into placeholders.
pub fn template(cmd: &str) -> String {
    // multi-command lines are templated per segment so `cd x && make` groups
    let first_line = cmd.lines().next().unwrap_or("");
    first_line
        .split_whitespace()
        .enumerate()
        .map(|(i, tok)| normalize_token(tok, i))
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalize_token(tok: &str, index: usize) -> String {
    if index == 0 || tok.starts_with('-') || matches!(tok, "&&" | "||" | "|" | ";" | ">") {
        return tok.to_string();
    }
    // URLs template as <path> too: `git clone <repo>` should group the same
    // whether the repo was given as https or a local path
    if URL.is_match(tok) {
        return "<path>".into();
    }
    let unquoted = tok.trim_matches(|c| c == '"' || c == '\'');
    if unquoted.starts_with('/')
        || unquoted.starts_with("./")
        || unquoted.starts_with("~/")
        || unquoted.starts_with("../")
        || (unquoted.contains('/') && !unquoted.contains("://"))
    {
        return "<path>".into();
    }
    if HEX.is_match(tok) {
        return "<hash>".into();
    }
    if VER.is_match(tok) || tok.contains('@') && tok.rsplit('@').next().map(|v| VER.is_match(v)).unwrap_or(false) {
        return "<ver>".into();
    }
    if NUM.is_match(tok) {
        return "<n>".into();
    }
    tok.to_string()
}

/// Fill the template column for any rows that don't have one yet.
pub fn run(conn: &Connection) -> Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT id, raw FROM commands WHERE template IS NULL AND source != 'claude_prompt'",
    )?;
    let rows: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    let mut update = conn.prepare("UPDATE commands SET template = ?2 WHERE id = ?1")?;
    let n = rows.len();
    for (id, raw) in rows {
        update.execute(rusqlite::params![id, template(&raw)])?;
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapses_variable_args() {
        assert_eq!(template("git checkout a1b2c3d4e9f"), "git checkout <hash>");
        assert_eq!(template("npm install -g npm@11.3.0"), "npm install -g <ver>");
        assert_eq!(template("cd ../foo"), "cd <path>");
        assert_eq!(template("curl https://x.dev/health"), "curl <path>");
        assert_eq!(template("kill 12345"), "kill <n>");
    }

    #[test]
    fn keeps_structure() {
        assert_eq!(template("pnpm run dev"), "pnpm run dev");
        assert_eq!(template("cargo build --release"), "cargo build --release");
    }
}
