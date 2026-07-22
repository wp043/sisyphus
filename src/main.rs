mod collect {
    pub mod claude;
    pub mod codex;
    pub mod gemini;
    pub mod hook;
    pub mod zsh;
}
mod db;
mod doctor;
mod draft;
mod mine;
mod normalize;
mod seed;
mod serve;
mod theme;
mod tui;

use std::io::{IsTerminal, Write};

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "sisyphus", about = "Finds the boulders you keep pushing", version)]
struct Cli {
    /// Use this database file instead of the default (also: SISYPHUS_DB env)
    #[arg(long, global = true)]
    db: Option<std::path::PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Ingest new history from zsh, Claude Code, Codex, and Gemini
    Ingest,
    /// Show what has been ingested
    Stats,
    /// Mine history for repeated workflows and review them (full-screen TUI)
    Report {
        /// How many patterns to show per kind
        #[arg(short, long, default_value_t = 10)]
        limit: usize,
        /// Draft ALL undecided patterns with claude and install every draft
        #[arg(long)]
        auto: bool,
        /// Plain line-by-line output instead of the TUI
        #[arg(long)]
        plain: bool,
        /// Only show patterns from repos whose name contains this substring
        #[arg(long)]
        project: Option<String>,
        /// Emit the mined patterns as JSON (no drafting)
        #[arg(long)]
        json: bool,
        /// Cluster prompts by meaning via claude (opt-in; sends prompts to claude)
        #[arg(long)]
        semantic: bool,
    },
    /// Draft an automation for one pattern and print it (no install)
    Draft {
        /// Pattern id as shown by `report`
        id: i64,
    },
    /// A glanceable summary: friction by repo, top boulders, adoption
    Digest,
    /// Show whether accepted automations are actually being used
    Gain,
    /// Act on adoption feedback: revise unused artifacts, resurface ignored
    /// patterns that kept growing
    Evolve,
    /// Ingest + mine silently; macOS-notify if a new high-value pattern appeared
    Scan,
    /// Serve the local web dashboard
    Serve {
        #[arg(short, long, default_value_t = 5757)]
        port: u16,
        /// Don't open the browser automatically
        #[arg(long)]
        no_open: bool,
    },
    /// Check that everything sisyphus relies on is set up
    Doctor,
    /// Print a shell completion script (e.g. `sisyphus completions zsh`)
    Completions {
        shell: clap_complete::Shell,
    },
    /// Print the shell hook that logs timestamps/durations/exit codes.
    /// Install with: eval "$(sisyphus hook zsh)" in ~/.zshrc
    Hook {
        /// Shell to emit a hook for (only zsh currently)
        shell: String,
    },
    /// Fill the DB with synthetic multi-week history (for development/demo).
    /// Refuses to touch the real default DB.
    Seed {
        #[arg(long, default_value_t = 21)]
        days: i64,
    },
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
    if let Some(db) = &cli.db {
        // route through the env var so per-request opens (serve) see it too
        unsafe { std::env::set_var("SISYPHUS_DB", db) };
    }
    let conn = db::open()?;
    match cli.cmd {
        Cmd::Ingest => {
            let s = ingest_all(&conn)?;
            if s.collapsed > 0 {
                println!("collapsed {} overlapping (replayed) session file(s)", s.collapsed);
            }
            println!(
                "ingested: {} zsh, {} claude, {} codex commands; {} prompts ({} templated)",
                s.zsh, s.claude, s.codex, s.prompts, s.templated
            );
        }
        Cmd::Stats => stats(&conn)?,
        Cmd::Report { limit, auto, plain, project, json, semantic } => {
            let proj = project.as_deref();
            if json {
                let cands = mine::candidates(&conn, limit, proj, semantic)?;
                println!("{}", serde_json::to_string_pretty(&cands)?);
            } else if auto {
                report_auto(&conn, limit, proj, semantic)?;
            } else if plain || !std::io::stdin().is_terminal() {
                report(&conn, limit, proj, semantic)?;
            } else {
                tui::run(&conn, limit, proj, semantic)?;
            }
        }
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
        Cmd::Digest => digest(&conn)?,
        Cmd::Gain => gain(&conn)?,
        Cmd::Evolve => evolve(&conn)?,
        Cmd::Serve { port, no_open } => serve::run(port, !no_open)?,
        Cmd::Doctor => doctor::run(&conn)?,
        Cmd::Completions { shell } => {
            clap_complete::generate(shell, &mut Cli::command(), "sisyphus", &mut std::io::stdout());
        }
        Cmd::Hook { shell } => match shell.as_str() {
            "zsh" => println!("{}", collect::hook::zsh_snippet()),
            other => anyhow::bail!("unsupported shell {other:?} — only zsh for now"),
        },
        Cmd::Seed { days } => {
            if db::is_default_db() {
                anyhow::bail!(
                    "refusing to seed your real database — pass --db, e.g.\n  sisyphus --db /tmp/demo.db seed && sisyphus --db /tmp/demo.db report"
                );
            }
            let n = seed::run(&conn, days)?;
            println!("seeded {n} synthetic commands over {days} days");
        }
        Cmd::Scan => scan(&conn)?,
        Cmd::Watch { install, uninstall } => watch(install, uninstall)?,
    }
    Ok(())
}

struct IngestStats {
    zsh: usize,
    claude: usize,
    codex: usize,
    prompts: usize,
    templated: usize,
    collapsed: usize,
}

/// Pull new history from every source, normalize, and de-overlap. Shared by the
/// `ingest` command and the ambient `scan` so the background job needs no shell.
fn ingest_all(conn: &rusqlite::Connection) -> Result<IngestStats> {
    // once the shell hook is logging, its records supersede HISTFILE
    // (same commands, richer fields) — don't ingest both
    let hook_new = collect::hook::ingest(conn)?;
    let zsh_path = dirs::home_dir().unwrap_or_default().join(".zsh_history");
    let zsh = if !collect::hook::log_path().exists() && zsh_path.exists() {
        collect::zsh::ingest(conn, &zsh_path)?
    } else {
        hook_new
    };
    let (claude, claude_prompts) = collect::claude::ingest(conn)?;
    let (codex, codex_prompts) = collect::codex::ingest(conn)?;
    let gemini_prompts = collect::gemini::ingest(conn)?;
    let templated = normalize::run(conn)?;
    let collapsed = mine::dedupe_sessions(conn)?;
    Ok(IngestStats {
        zsh,
        claude,
        codex,
        prompts: claude_prompts + codex_prompts + gemini_prompts,
        templated,
        collapsed,
    })
}

fn kind_icon(kind: &str) -> &'static str {
    match kind {
        "sequence" => "⚡",
        "fixloop" => "🔁",
        "failure" => "🩹",
        "intent" => "💡",
        "prompt" => "💬",
        _ => "•",
    }
}

/// One-glance morning summary: how much manual friction is mineable, where it
/// lives, the biggest boulders, and whether past automations are sticking.
fn digest(conn: &rusqlite::Connection) -> Result<()> {
    let live = "COALESCE(session_key,'') NOT IN (SELECT session_key FROM superseded)";
    let count = |sql: String| -> Result<i64> { Ok(conn.query_row(&sql, [], |r| r.get(0))?) };
    let cmds = count(format!("SELECT COUNT(*) FROM commands WHERE source NOT LIKE '%_prompt' AND {live}"))?;
    let prompts = count(format!("SELECT COUNT(*) FROM commands WHERE source LIKE '%_prompt' AND {live}"))?;

    let cands = mine::candidates(conn, 50, None, false)?;
    let repos: std::collections::HashSet<&str> =
        cands.iter().filter_map(|c| c.project.as_deref()).collect();
    println!(
        "\n  {cmds} commands · {prompts} prompts mined · {} undecided patterns across {} repo(s)\n",
        cands.len(),
        repos.len()
    );

    // friction by project (total estimated minutes + pattern count)
    let mut by_proj: std::collections::HashMap<String, (f64, usize)> = std::collections::HashMap::new();
    for c in &cands {
        let key = c.project.clone().unwrap_or_else(|| "(unattributed)".into());
        let e = by_proj.entry(key).or_default();
        e.0 += c.score;
        e.1 += 1;
    }
    if !by_proj.is_empty() {
        println!("  friction by repo:");
        let mut rows: Vec<_> = by_proj.into_iter().collect();
        rows.sort_by(|a, b| b.1.0.total_cmp(&a.1.0));
        for (proj, (secs, n)) in rows.iter().take(6) {
            println!("    {proj:<20} ~{:>3.0} min · {n} pattern(s)", secs / 60.0);
        }
        println!();
    }

    if !cands.is_empty() {
        println!("  top boulders:");
        for c in cands.iter().take(5) {
            let step = c.templates.first().map(String::as_str).unwrap_or("");
            let step: String = step.chars().take(52).collect();
            println!("    {:>6}  {} {step}", c.cost_label().replace("by hand", "").trim(), kind_icon(&c.kind));
        }
        println!();
    }

    let accepted = count("SELECT COUNT(*) FROM decisions WHERE decision = 'accepted'".into())?;
    let not_sticking = evolve_findings(conn)?
        .iter()
        .filter(|f| matches!(f.kind, FindingKind::NotAdopted))
        .count();
    println!("  adoption: {accepted} accepted · {not_sticking} not sticking");
    if !cands.is_empty() {
        println!("  → run `sisyphus report` to draft the top ones");
    }
    Ok(())
}

fn gain(conn: &rusqlite::Connection) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT d.artifact_path, p.template_seq, p.kind, COALESCE(d.at_command_id, 0)
         FROM decisions d JOIN patterns p ON p.id = d.pattern_id
         WHERE d.decision = 'accepted' AND d.artifact_path IS NOT NULL",
    )?;
    let rows: Vec<(String, String, String, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<std::result::Result<_, _>>()?;
    if rows.is_empty() {
        println!("no accepted automations yet — run `sisyphus report`");
        return Ok(());
    }
    let mut total_steps_saved = 0i64;
    for (path, seq, kind, at_id) in &rows {
        let name = std::path::Path::new(path)
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let templates: Vec<String> = serde_json::from_str(seq).unwrap_or_default();
        let steps = templates.len().max(1) as i64;
        let uses = db::artifact_uses(conn, &name, 0)?;
        let saved = uses * (steps - 1).max(0);
        total_steps_saved += saved;
        println!("{name:<24} used {uses}× · replaces {steps} steps · {saved} manual steps avoided");

        // for intent/prompt skills, show whether the agent's work for that ask
        // actually shrank after the skill was adopted
        if matches!(kind.as_str(), "intent" | "prompt") {
            let ask = templates
                .first()
                .map(|t| t.strip_prefix("ask: ").unwrap_or(t))
                .unwrap_or("");
            let (before, after, nb, na) = mine::ask_impact(conn, ask, *at_id)?;
            if nb > 0 && na > 0 {
                let delta = if before > 0.0 { (before - after) / before * 100.0 } else { 0.0 };
                println!(
                    "{:24}   impact: agent ran {before:.1} → {after:.1} cmds/ask after adoption ({delta:+.0}%)",
                    ""
                );
            }
        }
    }
    println!("\ntotal manual steps avoided: {total_steps_saved}");
    Ok(())
}

fn scan(conn: &rusqlite::Connection) -> Result<()> {
    const NOTIFY_THRESHOLD: f64 = 8.0;
    // pull fresh history first, so the background job is self-contained
    ingest_all(conn)?;
    let cands = mine::candidates(conn, 5, None, false)?;
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
    let evolve_count = evolve_findings(conn)?.len();
    if fresh.is_empty() && evolve_count == 0 {
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
    let msg = match (fresh.first(), evolve_count) {
        (Some(top), 0) => format!(
            "{} automatable pattern(s) — top: {} ({}×). Run `sisyphus report`.",
            fresh.len(),
            top.templates.join(" → ").chars().take(80).collect::<String>(),
            top.count
        ),
        (Some(_), n) => format!(
            "{} new pattern(s) + {n} automation(s) need attention. Run `sisyphus report` and `sisyphus evolve`.",
            fresh.len()
        ),
        (None, n) => format!("{n} accepted automation(s) aren't sticking. Run `sisyphus evolve`."),
    };
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
    const LABEL: &str = "dev.sisyphus.scan";
    let plist_path = dirs::home_dir()
        .context("no home dir")?
        .join(format!("Library/LaunchAgents/{LABEL}.plist"));
    // modern launchd is per-user-domain: gui/<uid>, driven by bootstrap/bootout
    // (the old load/unload are deprecated and fail with "Input/output error")
    let uid = unsafe { libc::getuid() };
    let domain = format!("gui/{uid}");
    let launchctl = |args: &[&str]| std::process::Command::new("launchctl").args(args).output();

    if uninstall {
        let _ = launchctl(&["bootout", &format!("{domain}/{LABEL}")]);
        let _ = std::fs::remove_file(&plist_path);
        println!("watch removed");
        return Ok(());
    }
    if !install {
        // is it actually loaded, not just present on disk?
        let loaded = launchctl(&["print", &format!("{domain}/{LABEL}")])
            .map(|o| o.status.success())
            .unwrap_or(false);
        println!(
            "watch: {}",
            if loaded { "running" } else { "not installed — run `sisyphus watch --install`" }
        );
        return Ok(());
    }

    let bin = std::env::current_exe()?;
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
    <key>Label</key><string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array><string>{bin}</string><string>scan</string></array>
    <key>StartInterval</key><integer>3600</integer>
    <key>RunAtLoad</key><false/>
</dict></plist>
"#,
        bin = bin.display()
    );
    std::fs::create_dir_all(plist_path.parent().unwrap())?;
    std::fs::write(&plist_path, &plist)?;

    // replace any prior instance, then bootstrap the fresh one
    let _ = launchctl(&["bootout", &format!("{domain}/{LABEL}")]);
    let out = launchctl(&["bootstrap", &domain, &plist_path.to_string_lossy()])?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("launchctl bootstrap failed: {}", err.trim());
    }
    println!("✓ hourly background scan installed ({})", plist_path.display());
    println!("  note: uses the current binary path — reinstall after `cargo install`");
    Ok(())
}

/// Draft every undecided pattern in parallel (3 claude workers) and install
/// whatever parses cleanly. The trust-the-machine mode.
fn report_auto(conn: &rusqlite::Connection, limit: usize, project: Option<&str>, semantic: bool) -> Result<()> {
    let cands = mine::candidates(conn, limit, project, semantic)?;
    if cands.is_empty() {
        println!("no undecided patterns — nothing to draft");
        return Ok(());
    }
    println!("drafting {} pattern(s) with 3 claude workers…\n", cands.len());
    let prompts: Vec<String> = cands
        .iter()
        .map(|c| draft::prepare_prompt(conn, &c.kind, &c.templates, c.count))
        .collect::<Result<_>>()?;

    let (tx, rx) = std::sync::mpsc::channel();
    let (mut ok, mut failed) = (0usize, 0usize);
    std::thread::scope(|s| -> Result<()> {
        for w in 0..3usize {
            let tx = tx.clone();
            let prompts = &prompts;
            s.spawn(move || {
                for idx in (w..prompts.len()).step_by(3) {
                    let _ = tx.send((idx, draft::run_claude(&prompts[idx])));
                }
            });
        }
        drop(tx);
        for (idx, result) in rx {
            let c = &cands[idx];
            match result {
                Ok(d) => match draft::install(&d) {
                    Ok(path) => {
                        decide(conn, c.id, "accepted", Some(path.display().to_string()))?;
                        ok += 1;
                        let committed = draft::commit_if_enabled(&path, &d.name, &d.kind)?
                            .map(|s| format!(" ({s})"))
                            .unwrap_or_default();
                        println!("✓ {} ({}) — {}\n    → {}{committed}", d.name, d.kind, d.summary, path.display());
                    }
                    Err(e) => {
                        failed += 1;
                        println!("✗ {} — install failed: {e:#}", d.name);
                    }
                },
                Err(e) => {
                    failed += 1;
                    println!("✗ pattern {} — draft failed: {e:#}", c.id);
                }
            }
        }
        Ok(())
    })?;
    println!("\n{ok} installed, {failed} failed — `sisyphus gain` will track adoption");
    if ok > 0 {
        println!("aliases (if any) need: source ~/.config/sisyphus/aliases.zsh");
    }
    Ok(())
}

fn report(conn: &rusqlite::Connection, limit: usize, project: Option<&str>, semantic: bool) -> Result<()> {
    let cands = mine::candidates(conn, limit, project, semantic)?;
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
                "failure" => "🩹 recurring failures (the same error across commands)",
                "intent" => "💡 recurring intents (your ask + what the agent then runs)",
                "prompt" => "💬 things you keep asking AI tools",
                _ => &c.kind,
            };
            println!("\n{header}");
        }
        let proj = c.project.as_deref().map(|p| format!(" · {p}")).unwrap_or_default();
        println!("\n  #{} (pattern {}){proj} — seen {}× · {}", rank + 1, c.id, c.count, c.cost_label());
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
                        if let Some(s) = draft::commit_if_enabled(&path, &d.name, &d.kind)? {
                            println!("  ✓ {s}");
                        }
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

use db::decide;

struct EvolveFinding {
    pattern_id: i64,
    kind: FindingKind,
    templates: Vec<String>,
    artifact_path: Option<String>,
    uses: i64,
    manual_since: usize,
}

enum FindingKind {
    NotAdopted,   // accepted, but manual sequence continues and artifact is unused
    Resurfaced,   // ignored, but the pattern kept growing
}

fn evolve_findings(conn: &rusqlite::Connection) -> Result<Vec<EvolveFinding>> {
    let mut stmt = conn.prepare(
        "SELECT d.pattern_id, d.decision, d.artifact_path, COALESCE(d.at_command_id, 0),
                COALESCE(d.count_at_decision, 0), p.template_seq, p.kind
         FROM decisions d JOIN patterns p ON p.id = d.pattern_id",
    )?;
    let rows: Vec<(i64, String, Option<String>, i64, i64, String, String)> = stmt
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
        })?
        .collect::<std::result::Result<_, _>>()?;

    let mut findings = Vec::new();
    for (pattern_id, decision, artifact_path, at_id, _count_at, seq, kind) in rows {
        let templates: Vec<String> = serde_json::from_str(&seq)?;
        // command-based patterns measure recurrence as sequence repeats;
        // intent/prompt patterns aren't commands, so measure how often the ask
        // itself recurred since the decision
        let manual_since = if kind == "intent" || kind == "prompt" {
            let ask = templates[0].strip_prefix("ask: ").unwrap_or(&templates[0]);
            mine::ask_recurrence_since(conn, ask, at_id)?
        } else {
            mine::occurrences_since(conn, &templates, at_id)?
        };
        match decision.as_str() {
            "accepted" => {
                let Some(path) = &artifact_path else { continue };
                let name = std::path::Path::new(path)
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                let uses = db::artifact_uses(conn, &name, at_id)?;
                if manual_since >= 2 && uses == 0 {
                    findings.push(EvolveFinding {
                        pattern_id,
                        kind: FindingKind::NotAdopted,
                        templates,
                        artifact_path: artifact_path.clone(),
                        uses,
                        manual_since,
                    });
                }
            }
            "ignored" => {
                if manual_since >= 4 {
                    findings.push(EvolveFinding {
                        pattern_id,
                        kind: FindingKind::Resurfaced,
                        templates,
                        artifact_path: None,
                        uses: 0,
                        manual_since,
                    });
                }
            }
            _ => {}
        }
    }
    Ok(findings)
}

fn evolve(conn: &rusqlite::Connection) -> Result<()> {
    let findings = evolve_findings(conn)?;
    if findings.is_empty() {
        println!("nothing to act on — automations are being used and ignored patterns stayed quiet");
        return Ok(());
    }
    let interactive = std::io::stdin().is_terminal();
    for f in &findings {
        match f.kind {
            FindingKind::NotAdopted => {
                let path = f.artifact_path.as_deref().unwrap_or("?");
                println!(
                    "\n✗ not adopted: {path}\n  used {}×, but you did the manual sequence {} more time(s):",
                    f.uses, f.manual_since
                );
                for t in &f.templates {
                    println!("    {t}");
                }
                if !interactive {
                    continue;
                }
                match ask("  [r]evise with claude / [x] retire artifact & reopen pattern / [s]kip? ")? {
                    'r' => {
                        println!("  diagnosing via claude -p …");
                        match draft::revise_artifact(conn, path, &f.templates, f.uses, f.manual_since) {
                            Ok(rev) if rev.action == "revise" => {
                                println!("  diagnosis: {}", rev.reason);
                                println!("{}", "─".repeat(60));
                                println!("{}", rev.content);
                                println!("{}", "─".repeat(60));
                                if ask("  [a]pply revision / [s]kip? ")? == 'a' {
                                    std::fs::write(path, &rev.content)?;
                                    decide(conn, f.pattern_id, "accepted", f.artifact_path.clone())?;
                                    println!("  ✓ revised in place: {path}");
                                }
                            }
                            Ok(rev) => {
                                println!("  claude recommends retiring it: {}", rev.reason);
                                if ask("  [x] retire / [s]kip? ")? == 'x' {
                                    retire(conn, f)?;
                                }
                            }
                            Err(e) => println!("  revision failed: {e:#}"),
                        }
                    }
                    'x' => retire(conn, f)?,
                    _ => {}
                }
            }
            FindingKind::Resurfaced => {
                println!(
                    "\n↩ you ignored this, but it happened {} more time(s) since:",
                    f.manual_since
                );
                for t in &f.templates {
                    println!("    {t}");
                }
                if interactive
                    && ask("  [r]eopen (shows in next report) / [k]eep ignored? ")? == 'r'
                {
                    conn.execute("DELETE FROM decisions WHERE pattern_id = ?1", [f.pattern_id])?;
                    println!("  ✓ reopened");
                }
            }
        }
    }
    Ok(())
}

fn retire(conn: &rusqlite::Connection, f: &EvolveFinding) -> Result<()> {
    if let Some(path) = &f.artifact_path
        && std::path::Path::new(path).exists() {
            std::fs::remove_file(path)?;
            println!("  ✓ removed {path}");
        }
    conn.execute("DELETE FROM decisions WHERE pattern_id = ?1", [f.pattern_id])?;
    println!("  ✓ pattern reopened for a fresh draft");
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
