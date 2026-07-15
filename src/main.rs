mod collect {
    pub mod claude;
    pub mod codex;
    pub mod gemini;
    pub mod zsh;
}
mod db;
mod draft;
mod mine;
mod normalize;

use std::io::{IsTerminal, Write};

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "sisyphus", about = "Finds the boulders you keep pushing", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Ingest new history from zsh, Claude Code, Codex, and Gemini
    Ingest,
    /// Show what has been ingested
    Stats,
    /// Mine history for repeated workflows and show the top candidates
    Report {
        /// How many patterns to show
        #[arg(short, long, default_value_t = 10)]
        limit: usize,
    },
    /// Draft an automation for one pattern and print it (no install)
    Draft {
        /// Pattern id as shown by `report`
        id: i64,
    },
}

fn main() -> Result<()> {
    // die quietly on `sisyphus stats | head` instead of panicking on EPIPE
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    let conn = db::open()?;
    match cli.cmd {
        Cmd::Ingest => {
            let zsh_path = dirs::home_dir().unwrap_or_default().join(".zsh_history");
            let zsh_new = if zsh_path.exists() {
                collect::zsh::ingest(&conn, &zsh_path)?
            } else {
                0
            };
            let (claude_new, claude_prompts) = collect::claude::ingest(&conn)?;
            let (codex_new, codex_prompts) = collect::codex::ingest(&conn)?;
            let gemini_prompts = collect::gemini::ingest(&conn)?;
            let templated = normalize::run(&conn)?;
            println!(
                "ingested: {zsh_new} zsh, {claude_new} claude, {codex_new} codex commands; \
                 {} prompts ({templated} templated)",
                claude_prompts + codex_prompts + gemini_prompts
            );
        }
        Cmd::Stats => stats(&conn)?,
        Cmd::Report { limit } => report(&conn, limit)?,
        Cmd::Draft { id } => {
            let (seq, count): (String, i64) = conn.query_row(
                "SELECT template_seq, count FROM patterns WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let templates: Vec<String> = serde_json::from_str(&seq)?;
            let d = draft::draft_pattern(&conn, &templates, count as usize)?;
            println!("── {} ({}) — {}\n{}", d.name, d.kind, d.summary, d.content);
        }
    }
    Ok(())
}

fn report(conn: &rusqlite::Connection, limit: usize) -> Result<()> {
    let patterns = mine::mine(conn)?;
    let top = mine::store_and_rank(conn, &patterns, limit)?;
    if top.is_empty() {
        println!("no undecided patterns found — ingest more history or lower thresholds");
        return Ok(());
    }
    for (rank, (id, idx)) in top.iter().enumerate() {
        let p = &patterns[*idx];
        println!("\n⚡ #{} (pattern {id}) — seen {}× · score {:.0}", rank + 1, p.count, p.score);
        for (i, tpl) in p.templates.iter().enumerate() {
            let arrow = if i == 0 { " " } else { "→" };
            println!("   {arrow} {tpl}");
        }
    }
    if !std::io::stdin().is_terminal() {
        println!("\n(non-interactive: run `sisyphus report` in a terminal to draft automations)");
        return Ok(());
    }
    println!();
    for (rank, (id, idx)) in top.iter().enumerate() {
        let p = &patterns[*idx];
        match ask(&format!(
            "pattern #{} ({}×): [d]raft with claude / [i]gnore forever / [s]kip / [q]uit? ",
            rank + 1,
            p.count
        ))? {
            'd' => review_draft(conn, *id, p)?,
            'i' => decide(conn, *id, "ignored", None)?,
            'q' => break,
            _ => {}
        }
    }
    Ok(())
}

fn review_draft(conn: &rusqlite::Connection, id: i64, p: &mine::Pattern) -> Result<()> {
    println!("  drafting via claude -p …");
    let mut d = match draft::draft_pattern(conn, &p.templates, p.count) {
        Ok(d) => d,
        Err(e) => {
            println!("  draft failed: {e:#}");
            return Ok(());
        }
    };
    loop {
        println!("\n── {} ({}) — {}", d.name, d.kind, d.summary);
        println!("{}", "─".repeat(60));
        println!("{}", d.content);
        println!("{}", "─".repeat(60));
        match ask("[a]ccept / [e]dit / [i]gnore forever / [s]kip? ")? {
            'a' => {
                match draft::install(&d) {
                    Ok(path) => {
                        decide(conn, id, "accepted", Some(path.display().to_string()))?;
                        println!("  ✓ installed: {}", path.display());
                        if d.kind == "alias" {
                            println!("  add to ~/.zshrc once: source ~/.config/sisyphus/aliases.zsh");
                        }
                    }
                    Err(e) => println!("  install failed: {e:#}"),
                }
                return Ok(());
            }
            'e' => {
                if let Some(edited) = edit_in_editor(&d.content)? {
                    d.content = edited;
                }
            }
            'i' => {
                decide(conn, id, "ignored", None)?;
                return Ok(());
            }
            _ => return Ok(()),
        }
    }
}

fn edit_in_editor(content: &str) -> Result<Option<String>> {
    let path = std::env::temp_dir().join(format!("sisyphus-draft-{}.txt", std::process::id()));
    std::fs::write(&path, content)?;
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let status = std::process::Command::new(&editor).arg(&path).status()?;
    if !status.success() {
        println!("  editor exited non-zero, keeping original");
        return Ok(None);
    }
    let edited = std::fs::read_to_string(&path)?;
    let _ = std::fs::remove_file(&path);
    Ok(Some(edited))
}

fn decide(conn: &rusqlite::Connection, id: i64, decision: &str, path: Option<String>) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    conn.execute(
        "INSERT OR REPLACE INTO decisions (pattern_id, decision, artifact_path, ts) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![id, decision, path, now],
    )?;
    Ok(())
}

fn ask(prompt: &str) -> Result<char> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().chars().next().unwrap_or('s').to_ascii_lowercase())
}

fn stats(conn: &rusqlite::Connection) -> Result<()> {
    let count = |src: &str| -> Result<i64> {
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM commands WHERE source = ?1",
            [src],
            |r| r.get(0),
        )?)
    };
    let zsh = count("zsh")?;
    let claude = count("claude")?;
    let prompts = count("claude_prompt")?;
    let sessions: i64 = conn.query_row(
        "SELECT COUNT(DISTINCT session_key) FROM commands WHERE source = 'claude'",
        [],
        |r| r.get(0),
    )?;
    println!("commands   zsh: {zsh}   claude: {claude} (across {sessions} sessions)   prompts: {prompts}");

    println!("\ntop command templates:");
    let mut stmt = conn.prepare(
        "SELECT template, COUNT(*) c FROM commands
         WHERE template IS NOT NULL GROUP BY template ORDER BY c DESC LIMIT 15",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    })?;
    for row in rows {
        let (tpl, c) = row?;
        println!("  {c:>5}×  {tpl}");
    }
    Ok(())
}
