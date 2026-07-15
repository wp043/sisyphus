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

use anyhow::{Context, Result};
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
    /// Show whether accepted automations are actually being used
    Gain,
    /// Ingest + mine silently; macOS-notify if a new high-value pattern appeared
    Scan,
    /// Manage the hourly background scan (launchd)
    Watch {
        #[arg(long)]
        install: bool,
        #[arg(long)]
        uninstall: bool,
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
            let (kind, seq, count): (String, String, i64) = conn.query_row(
                "SELECT kind, template_seq, count FROM patterns WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            let templates: Vec<String> = serde_json::from_str(&seq)?;
            let d = draft::draft_pattern(&conn, &kind, &templates, count as usize)?;
            println!("── {} ({}) — {}\n{}", d.name, d.kind, d.summary, d.content);
        }
        Cmd::Gain => gain(&conn)?,
        Cmd::Scan => scan(&conn)?,
        Cmd::Watch { install, uninstall } => watch(install, uninstall)?,
    }
    Ok(())
}

fn gain(conn: &rusqlite::Connection) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT d.artifact_path, p.template_seq, p.count FROM decisions d
         JOIN patterns p ON p.id = d.pattern_id
         WHERE d.decision = 'accepted' AND d.artifact_path IS NOT NULL",
    )?;
    let rows: Vec<(String, String, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<_, _>>()?;
    if rows.is_empty() {
        println!("no accepted automations yet — run `sisyphus report`");
        return Ok(());
    }
    let mut total_steps_saved = 0i64;
    for (path, seq, _) in &rows {
        let name = std::path::Path::new(path)
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let steps: i64 = serde_json::from_str::<Vec<String>>(seq).map(|v| v.len() as i64).unwrap_or(1);
        let uses: i64 = conn.query_row(
            "SELECT COUNT(*) FROM commands WHERE head = ?1",
            [&name],
            |r| r.get(0),
        )?;
        let saved = uses * (steps - 1).max(0);
        total_steps_saved += saved;
        println!("{name:<24} used {uses}× · replaces {steps} steps · {saved} manual steps avoided");
    }
    println!("\ntotal manual steps avoided: {total_steps_saved}");
    Ok(())
}

fn scan(conn: &rusqlite::Connection) -> Result<()> {
    const NOTIFY_THRESHOLD: f64 = 8.0;
    let cands = mine::candidates(conn, 5)?;
    let mut fresh = Vec::new();
    for c in &cands {
        if c.score < NOTIFY_THRESHOLD {
            continue;
        }
        let seen: bool = conn
            .query_row("SELECT 1 FROM notified WHERE pattern_id = ?1", [c.id], |_| Ok(true))
            .unwrap_or(false);
        if !seen {
            fresh.push(c);
        }
    }
    if fresh.is_empty() {
        return Ok(());
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    for c in &fresh {
        conn.execute(
            "INSERT OR IGNORE INTO notified (pattern_id, ts) VALUES (?1, ?2)",
            rusqlite::params![c.id, now],
        )?;
    }
    let top = fresh[0];
    let msg = format!(
        "{} automatable pattern(s) found — top: {} ({}×). Run `sisyphus report`.",
        fresh.len(),
        top.templates.join(" → ").chars().take(80).collect::<String>(),
        top.count
    );
    println!("{msg}");
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "display notification \"{}\" with title \"sisyphus\"",
            msg.replace('"', "'")
        );
        let _ = std::process::Command::new("osascript").args(["-e", &script]).status();
    }
    Ok(())
}

fn watch(install: bool, uninstall: bool) -> Result<()> {
    let plist_path = dirs::home_dir()
        .context("no home dir")?
        .join("Library/LaunchAgents/dev.sisyphus.scan.plist");
    if uninstall {
        let _ = std::process::Command::new("launchctl")
            .args(["unload", &plist_path.display().to_string()])
            .status();
        let _ = std::fs::remove_file(&plist_path);
        println!("watch removed");
        return Ok(());
    }
    if !install {
        println!(
            "installed: {}",
            if plist_path.exists() { "yes" } else { "no — run `sisyphus watch --install`" }
        );
        return Ok(());
    }
    let bin = std::env::current_exe()?;
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
    <key>Label</key><string>dev.sisyphus.scan</string>
    <key>ProgramArguments</key>
    <array><string>/bin/sh</string><string>-c</string>
    <string>"{bin}" ingest >/dev/null 2>&1 && "{bin}" scan</string></array>
    <key>StartInterval</key><integer>3600</integer>
    <key>RunAtLoad</key><false/>
</dict></plist>
"#,
        bin = bin.display()
    );
    std::fs::create_dir_all(plist_path.parent().unwrap())?;
    std::fs::write(&plist_path, plist)?;
    let status = std::process::Command::new("launchctl")
        .args(["load", &plist_path.display().to_string()])
        .status()?;
    if status.success() {
        println!("✓ hourly background scan installed ({})", plist_path.display());
        println!("  note: uses the current binary path — reinstall after `cargo install`");
    } else {
        anyhow::bail!("launchctl load failed");
    }
    Ok(())
}

fn report(conn: &rusqlite::Connection, limit: usize) -> Result<()> {
    let cands = mine::candidates(conn, limit)?;
    if cands.is_empty() {
        println!("no undecided patterns found — ingest more history or lower thresholds");
        return Ok(());
    }
    let mut last_kind = "";
    for (rank, c) in cands.iter().enumerate() {
        if c.kind != last_kind {
            last_kind = &c.kind;
            let header = match c.kind.as_str() {
                "sequence" => "⚡ repeated workflows",
                "fixloop" => "🔁 fix-loops (execute → fail → fix → retry)",
                "prompt" => "💬 things you keep asking AI tools",
                _ => &c.kind,
            };
            println!("\n{header}");
        }
        println!("\n  #{} (pattern {}) — seen {}× · score {:.0}", rank + 1, c.id, c.count, c.score);
        for (i, tpl) in c.templates.iter().enumerate() {
            let arrow = if i == 0 { " " } else { "→" };
            println!("     {arrow} {tpl}");
        }
    }
    if !std::io::stdin().is_terminal() {
        println!("\n(non-interactive: run `sisyphus report` in a terminal to draft automations)");
        return Ok(());
    }
    println!();
    for (rank, c) in cands.iter().enumerate() {
        match ask(&format!(
            "#{} ({}×): [d]raft with claude / [i]gnore forever / [s]kip / [q]uit? ",
            rank + 1,
            c.count
        ))? {
            'd' => review_draft(conn, c)?,
            'i' => decide(conn, c.id, "ignored", None)?,
            'q' => break,
            _ => {}
        }
    }
    Ok(())
}

fn review_draft(conn: &rusqlite::Connection, c: &mine::Candidate) -> Result<()> {
    let (id, p) = (c.id, c);
    println!("  drafting via claude -p …");
    let mut d = match draft::draft_pattern(conn, &c.kind, &p.templates, p.count) {
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
