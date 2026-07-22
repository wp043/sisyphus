use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Deserialize)]
pub struct Draft {
    pub name: String,
    pub kind: String, // "script" | "alias" | "skill"
    pub summary: String,
    pub content: String,
}

/// Up to 3 concrete example invocations per template, so the drafter sees
/// real arguments instead of just placeholders.
fn examples(conn: &Connection, templates: &[String]) -> Result<String> {
    let mut out = String::new();
    let mut stmt = conn.prepare(
        "SELECT DISTINCT raw FROM commands WHERE template = ?1 LIMIT 3",
    )?;
    for tpl in templates {
        out.push_str(&format!("template: {tpl}\n"));
        let rows = stmt.query_map([tpl], |r| r.get::<_, String>(0))?;
        for raw in rows.flatten() {
            let first = raw.lines().next().unwrap_or(&raw);
            out.push_str(&format!("  e.g. {first}\n"));
        }
    }
    Ok(out)
}

fn build_prompt(kind: &str, templates: &[String], count: usize, examples: &str) -> String {
    let situation = match kind {
        "fixloop" => format!(
            "This command was re-run {count} times across the developer's AI-agent sessions, failing and being fixed in between — a manual execute→fix→retry loop:\n\n{}\n\nDraft a Claude Code skill (kind \"skill\") that automates the whole loop: run the command, read the errors, fix the cause, re-run, max 3 retries, then report. SKILL.md format with `name:` and `description:` frontmatter, where description states WHEN to auto-trigger.",
            templates.join("\n")
        ),
        "intent" => {
            let ask = templates[0].strip_prefix("ask: ").unwrap_or(&templates[0]);
            format!(
                "The developer has asked their AI coding agent essentially this {count} separate times:\n\n\"{ask}\"\n\nand each time, the agent ended up running these same steps:\n{}\n\nDraft a Claude Code skill (kind \"skill\") that performs this recurring intent directly — encode the known steps above so the agent executes them immediately instead of rediscovering them every session. SKILL.md format with `name:` and `description:` frontmatter, where description states WHEN to auto-trigger.",
                templates[1..].join("\n")
            )
        }
        "prompt" => format!(
            "The developer has typed essentially this same request to AI coding tools {count} separate times:\n\n\"{}\"\n\nDraft a Claude Code skill (kind \"skill\") that captures this recurring intent so it becomes a one-word command. SKILL.md format with `name:` and `description:` frontmatter.",
            templates.join("\n")
        ),
        "failure" => format!(
            "Draft a Claude Code skill (kind \"skill\") that helps the developer avoid or quickly fix a recurring error (details below). The skill's description must state WHEN to auto-trigger (e.g. before running the kind of command that causes it), and its body should give the concrete prevention/fix — for example, for a ripgrep regex parse error, use `rg -F` for literal search or escape metacharacters. Signature: {}",
            templates.first().map(String::as_str).unwrap_or("")
        ),
        _ => format!(
            "The developer has manually repeated this command sequence {count} times (mined from their real shell/agent history):\n\n{}",
            templates.join("\n")
        ),
    };
    format!(
        r#"You are drafting an automation for a developer.

{situation}

Concrete examples of each step from their history:
{examples}

Draft the best automation. Rules:
- kind "alias" for a trivial one-liner; kind "script" for a multi-step shell workflow (zsh, `set -euo pipefail`, positional args or flags for the parts that vary between runs — the <path>/<url>/<ver> placeholders); kind "skill" ONLY if the workflow inherently needs an AI agent to do judgment work.
- Scripts must be safe to re-run: no destructive commands beyond what the user's own sequence does, sensible error messages.
- name: short kebab-case.
- summary: one sentence, what it saves.

Respond with STRICT JSON only, no markdown fences, matching:
{{"name": "...", "kind": "script|alias|skill", "summary": "...", "content": "full artifact text"}}"#,
    )
}

/// Build the full drafting prompt. Needs the DB (for examples); the actual
/// claude call in `run_claude` doesn't, so calls can run on worker threads.
/// Where does this user actually work? If most of their commands run inside
/// AI agents rather than a shell, a script in ~/.local/bin will never be
/// typed — a Claude Code skill is the artifact they'll actually invoke.
fn agent_native(conn: &Connection) -> bool {
    // config override: [draft] prefer = "skill" | "script" | "auto"
    if let Ok(text) = std::fs::read_to_string(crate::theme::config_path())
        && let Ok(v) = text.parse::<toml::Table>()
        && let Some(p) = v.get("draft").and_then(|d| d.get("prefer")).and_then(|p| p.as_str())
    {
        match p {
            "skill" => return true,
            "script" => return false,
            _ => {}
        }
    }
    let (agent, shell): (i64, i64) = conn
        .query_row(
            "SELECT
               SUM(CASE WHEN source IN ('claude','codex') THEN 1 ELSE 0 END),
               SUM(CASE WHEN source = 'zsh' THEN 1 ELSE 0 END)
             FROM commands",
            [],
            |r| Ok((r.get::<_, Option<i64>>(0)?.unwrap_or(0), r.get::<_, Option<i64>>(1)?.unwrap_or(0))),
        )
        .unwrap_or((0, 0));
    // shell that's mostly navigation + launching agents = agent-native user
    agent > shell / 2
}

pub fn prepare_prompt(conn: &Connection, kind: &str, templates: &[String], count: usize) -> Result<String> {
    // failure patterns key on an error signature (templates[0]), not a command;
    // gather real error snippets that match it for the draft
    if kind == "failure" {
        let mut stmt = conn.prepare(
            "SELECT error_snippet FROM commands WHERE failed = 1 AND error_snippet IS NOT NULL",
        )?;
        let snippets: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .filter_map(|s| s.ok())
            .filter(|s| crate::mine::error_signature(s).as_deref() == Some(&templates[0]))
            .take(3)
            .collect();
        let cmds = templates[1..].join("\n");
        let mut ex = format!(
            "The developer keeps hitting this error, {count} times across different commands:\n\n\"{}\"\n\nExample commands that triggered it:\n{cmds}\n",
            templates[0]
        );
        for s in &snippets {
            ex.push_str(&format!("---\n{}\n", s.chars().take(300).collect::<String>()));
        }
        return Ok(build_prompt(kind, templates, count, &ex));
    }
    let mut ex = if kind == "prompt" { String::new() } else { examples(conn, templates)? };
    if agent_native(conn) {
        ex.push_str(
            "\nIMPORTANT context about this user: they rarely type shell commands — almost all their work happens inside AI coding agents (Claude Code, Codex). A script in ~/.local/bin would never get run. Strongly prefer kind \"skill\" (a Claude Code skill they trigger with one word inside the agent), unless the workflow is inherently shell-native.\n",
        );
    }
    if kind == "fixloop" {
        // real failure output is the highest-signal context a fix skill can get
        let mut stmt = conn.prepare(
            "SELECT DISTINCT error_snippet FROM commands
             WHERE template = ?1 AND failed = 1 AND error_snippet IS NOT NULL LIMIT 3",
        )?;
        let errors: Vec<String> = stmt
            .query_map([&templates[0]], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        if !errors.is_empty() {
            ex.push_str("\nActual error output seen when it failed:\n");
            for e in errors {
                ex.push_str(&format!("---\n{e}\n"));
            }
        }
    }
    Ok(build_prompt(kind, templates, count, &ex))
}

/// The claude binary to invoke. `SISYPHUS_CLAUDE_BIN` overrides it — useful for
/// pointing at a wrapper, and for feeding tests a deterministic mock.
fn claude_bin() -> String {
    std::env::var("SISYPHUS_CLAUDE_BIN").unwrap_or_else(|_| "claude".into())
}

/// One headless claude call → parsed draft. Thread-safe; no DB access.
pub fn run_claude(prompt: &str) -> Result<Draft> {
    let out = Command::new(claude_bin())
        .args(["-p", prompt])
        .output()
        .context("failed to run `claude` — is Claude Code installed and on PATH?")?;
    if !out.status.success() {
        bail!("claude -p failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    parse_draft(&String::from_utf8_lossy(&out.stdout))
}

pub fn draft_pattern(conn: &Connection, kind: &str, templates: &[String], count: usize) -> Result<Draft> {
    run_claude(&prepare_prompt(conn, kind, templates, count)?)
}

/// Ask claude to group prompts by shared intent. Returns groups of indices.
pub fn claude_group(prompt: &str) -> Result<Vec<Vec<usize>>> {
    let out = Command::new(claude_bin())
        .args(["-p", prompt])
        .output()
        .context("failed to run `claude`")?;
    if !out.status.success() {
        bail!("claude -p failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    parse_groups(&String::from_utf8_lossy(&out.stdout))
}

/// Extract a JSON array-of-arrays of indices from claude's (possibly fenced or
/// prose-wrapped) output.
fn parse_groups(text: &str) -> Result<Vec<Vec<usize>>> {
    let start = text.find('[').context("no JSON array in claude output")?;
    let end = text.rfind(']').context("no JSON array in claude output")?;
    if end < start {
        bail!("malformed JSON array in claude output");
    }
    Ok(serde_json::from_str(&text[start..=end])?)
}

fn parse_draft(text: &str) -> Result<Draft> {
    // tolerate markdown fences or prose around the JSON object
    let start = text.find('{').context("no JSON in claude output")?;
    let end = text.rfind('}').context("no JSON in claude output")?;
    if end < start {
        bail!("no JSON object in claude output");
    }
    let draft: Draft = serde_json::from_str(&text[start..=end])
        .with_context(|| format!("unparseable draft: {}", text.chars().take(200).collect::<String>()))?;
    if !matches!(draft.kind.as_str(), "script" | "alias" | "skill") {
        bail!("unknown draft kind: {}", draft.kind);
    }
    if draft.name.contains(['/', '.']) || draft.name.is_empty() {
        bail!("bad draft name: {}", draft.name);
    }
    Ok(draft)
}

#[derive(Debug, Deserialize)]
pub struct Revision {
    pub action: String, // "revise" | "retire"
    pub reason: String,
    #[serde(default)]
    pub content: String,
}

/// The user accepted this artifact but is still doing the work manually.
/// Ask Claude to diagnose why the artifact didn't stick and either revise it
/// or recommend retiring it.
pub fn revise_artifact(
    conn: &Connection,
    artifact_path: &str,
    templates: &[String],
    uses: i64,
    manual_since: usize,
) -> Result<Revision> {
    let current = std::fs::read_to_string(artifact_path)
        .with_context(|| format!("cannot read {artifact_path}"))?;
    let prompt = format!(
        r#"A developer accepted this automation, but adoption data says it isn't working:
- artifact invoked {uses} time(s) since install
- meanwhile the manual sequence it replaces was still performed {manual_since} more time(s)

The manual sequence:
{}

Recent concrete examples of the manual steps:
{}

Current artifact ({artifact_path}):
---
{current}
---

Diagnose the most likely mismatch (wrong arguments? too rigid? name hard to remember? missing a step they actually do?) and respond with STRICT JSON only:
{{"action": "revise|retire", "reason": "one sentence", "content": "full revised artifact text (empty if retire)"}}"#,
        templates.join("\n"),
        examples(conn, templates)?,
    );
    let out = Command::new(claude_bin())
        .args(["-p", &prompt])
        .output()
        .context("failed to run `claude`")?;
    if !out.status.success() {
        bail!("claude -p failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let start = text.find('{').context("no JSON in claude output")?;
    let end = text.rfind('}').context("no JSON in claude output")?;
    let rev: Revision = serde_json::from_str(&text[start..=end]).context("unparseable revision")?;
    if !matches!(rev.action.as_str(), "revise" | "retire") {
        bail!("unknown revision action: {}", rev.action);
    }
    Ok(rev)
}

/// When `[skills] commit = true` in the config, commit a freshly installed
/// artifact to whatever git repo contains it (e.g. a dotfiles repo that tracks
/// ~/.claude/skills). Returns a short summary when a commit was actually made.
pub fn commit_if_enabled(path: &Path, name: &str, kind: &str) -> Result<Option<String>> {
    let enabled = std::fs::read_to_string(crate::theme::config_path())
        .ok()
        .and_then(|t| t.parse::<toml::Table>().ok())
        .and_then(|v| v.get("skills")?.get("commit")?.as_bool())
        .unwrap_or(false);
    if !enabled {
        return Ok(None);
    }
    let dir = path.parent().unwrap_or(Path::new("."));
    let git = |args: &[&str]| Command::new("git").args(args).output();

    let top = git(&["-C", &dir.to_string_lossy(), "rev-parse", "--show-toplevel"])?;
    if !top.status.success() {
        return Ok(None); // artifact isn't inside a git repo
    }
    let root = String::from_utf8_lossy(&top.stdout).trim().to_string();
    let scope = if kind == "skill" { "skills" } else { "bin" };
    let msg = format!("feat({scope}): add {name} (via sisyphus)");
    git(&["-C", &root, "add", &path.to_string_lossy()])?;
    let commit = git(&["-C", &root, "commit", "-m", &msg])?;
    Ok(commit
        .status
        .success()
        .then(|| format!("committed to {}", Path::new(&root).file_name().unwrap_or_default().to_string_lossy())))
}

/// Write the artifact to its real destination and return the path.
pub fn install(draft: &Draft) -> Result<PathBuf> {
    let home = dirs::home_dir().context("no home dir")?;
    match draft.kind.as_str() {
        "script" => {
            let dir = home.join(".local/bin");
            std::fs::create_dir_all(&dir)?;
            let path = dir.join(&draft.name);
            if path.exists() {
                bail!("{} already exists — rename the draft first", path.display());
            }
            let content = if draft.content.starts_with("#!") {
                draft.content.clone()
            } else {
                format!("#!/bin/zsh\n{}", draft.content)
            };
            std::fs::write(&path, content)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
            }
            Ok(path)
        }
        "alias" => {
            let dir = home.join(".config/sisyphus");
            std::fs::create_dir_all(&dir)?;
            let path = dir.join("aliases.zsh");
            let mut existing = std::fs::read_to_string(&path).unwrap_or_default();
            existing.push_str(&format!("\n# {}\n{}\n", draft.summary, draft.content.trim()));
            std::fs::write(&path, existing)?;
            Ok(path)
        }
        "skill" => {
            let dir = home.join(".claude/skills").join(&draft.name);
            if dir.exists() {
                bail!("skill {} already exists", draft.name);
            }
            std::fs::create_dir_all(&dir)?;
            let path = dir.join("SKILL.md");
            std::fs::write(&path, &draft.content)?;
            Ok(path)
        }
        other => bail!("unknown kind {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fenced_json() {
        let d = parse_draft("```json\n{\"name\":\"git-init-push\",\"kind\":\"script\",\"summary\":\"s\",\"content\":\"c\"}\n```").unwrap();
        assert_eq!(d.name, "git-init-push");
    }

    #[test]
    fn parses_group_json() {
        assert_eq!(parse_groups("[[0,4,9],[2,3]]").unwrap(), vec![vec![0, 4, 9], vec![2, 3]]);
        // fenced / prose-wrapped
        assert_eq!(parse_groups("here:\n```json\n[[1,2,3]]\n```").unwrap(), vec![vec![1, 2, 3]]);
        assert!(parse_groups("no array here").is_err());
    }

    #[test]
    fn malformed_output_errors_without_panicking() {
        // '}' before '{' must not panic on the reversed slice range
        assert!(parse_draft("} then {").is_err());
        // braces present but invalid JSON with multibyte >200 bytes: the error
        // context must not slice mid-char (chars().take, not byte index)
        assert!(parse_draft(&format!("{{ {} }}", "中".repeat(150))).is_err());
    }

    #[test]
    fn rejects_path_traversal_names() {
        assert!(parse_draft("{\"name\":\"../evil\",\"kind\":\"script\",\"summary\":\"s\",\"content\":\"c\"}").is_err());
    }

    // End-to-end coverage of the claude-calling path via a deterministic mock
    // binary — the boundary that had no tests because it shells out to claude.
    #[test]
    fn run_claude_parses_mock_output() {
        let dir = std::env::temp_dir().join(format!("sis-mock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("claude");
        // a fake `claude` that ignores its args and prints a fenced draft
        std::fs::write(
            &bin,
            "#!/bin/sh\ncat <<'EOF'\nhere you go:\n```json\n{\"name\":\"pr-triage\",\"kind\":\"skill\",\"summary\":\"triage a PR\",\"content\":\"# PR Triage\\nrun gh pr view\"}\n```\nEOF\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // SISYPHUS_CLAUDE_BIN is only read here; no other test invokes claude
        unsafe { std::env::set_var("SISYPHUS_CLAUDE_BIN", &bin) };
        let d = run_claude("draft me something").expect("mock claude should parse");
        unsafe { std::env::remove_var("SISYPHUS_CLAUDE_BIN") };
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(d.name, "pr-triage");
        assert_eq!(d.kind, "skill");
        assert!(d.content.contains("gh pr view"));
    }

    #[test]
    fn rejects_unknown_kind() {
        assert!(parse_draft("{\"name\":\"x\",\"kind\":\"daemon\",\"summary\":\"s\",\"content\":\"c\"}").is_err());
    }
}
