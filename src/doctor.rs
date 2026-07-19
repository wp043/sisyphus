use anyhow::Result;
use rusqlite::Connection;

fn check(ok: bool, label: &str, fix: &str) {
    if ok {
        println!("  ✓ {label}");
    } else {
        println!("  ✗ {label}\n      fix: {fix}");
    }
}

/// Sanity-check the setup that makes sisyphus effective day to day.
pub fn run(conn: &Connection) -> Result<()> {
    let home = dirs::home_dir().unwrap_or_default();

    println!("environment");
    let claude_ok = std::process::Command::new("claude")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    check(claude_ok, "claude CLI on PATH (needed for drafting)", "install Claude Code");

    let zshrc = std::fs::read_to_string(home.join(".zshrc")).unwrap_or_default();
    let ext_hist = zshrc.to_uppercase().contains("EXTENDED_HISTORY")
        || std::fs::read(home.join(".zsh_history"))
            .map(|b| b.starts_with(b": "))
            .unwrap_or(false);
    check(
        ext_hist,
        "zsh EXTENDED_HISTORY (timestamps make mining smarter)",
        "echo 'setopt EXTENDED_HISTORY' >> ~/.zshrc",
    );

    check(
        zshrc.contains("sisyphus hook zsh") || crate::collect::hook::log_path().exists(),
        "shell hook installed (exit codes + durations for every command)",
        "echo 'eval \"$(sisyphus hook zsh)\"' >> ~/.zshrc",
    );

    let local_bin_on_path = std::env::var("PATH")
        .map(|p| p.split(':').any(|d| d == home.join(".local/bin").to_string_lossy()))
        .unwrap_or(false);
    check(
        local_bin_on_path,
        "~/.local/bin on PATH (where accepted scripts install)",
        "echo 'export PATH=\"$HOME/.local/bin:$PATH\"' >> ~/.zshrc",
    );

    let aliases = home.join(".config/sisyphus/aliases.zsh");
    if aliases.exists() {
        check(
            zshrc.contains("sisyphus/aliases.zsh"),
            "accepted aliases sourced in ~/.zshrc",
            "echo 'source ~/.config/sisyphus/aliases.zsh' >> ~/.zshrc",
        );
    }

    println!("\nambient");
    check(
        home.join("Library/LaunchAgents/dev.sisyphus.scan.plist").exists(),
        "hourly background scan installed",
        "sisyphus watch --install",
    );

    println!("\ndata");
    for (label, src) in [("zsh", "zsh"), ("claude", "claude"), ("codex", "codex")] {
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM commands WHERE source = ?1",
            [src],
            |r| r.get(0),
        )?;
        check(n > 0, &format!("{label} history ingested ({n} commands)"), "sisyphus ingest");
    }
    let undecided: i64 = conn.query_row(
        "SELECT COUNT(*) FROM patterns p WHERE NOT EXISTS
         (SELECT 1 FROM decisions d WHERE d.pattern_id = p.id)",
        [],
        |r| r.get(0),
    )?;
    println!("  · {undecided} undecided pattern(s) waiting in `sisyphus report`");
    Ok(())
}
