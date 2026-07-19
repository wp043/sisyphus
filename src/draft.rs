use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use serde::Deserialize;
use std::path::PathBuf;
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
        "prompt" => format!(
            "The developer has typed essentially this same request to AI coding tools {count} separate times:\n\n\"{}\"\n\nDraft a Claude Code skill (kind \"skill\") that captures this recurring intent so it becomes a one-word command. SKILL.md format with `name:` and `description:` frontmatter.",
            templates.join("\n")
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
pub fn prepare_prompt(conn: &Connection, kind: &str, templates: &[String], count: usize) -> Result<String> {
    let mut ex = if kind == "prompt" { String::new() } else { examples(conn, templates)? };
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

/// One headless claude call → parsed draft. Thread-safe; no DB access.
pub fn run_claude(prompt: &str) -> Result<Draft> {
    let out = Command::new("claude")
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

fn parse_draft(text: &str) -> Result<Draft> {
    // tolerate markdown fences or prose around the JSON object
    let start = text.find('{').context("no JSON in claude output")?;
    let end = text.rfind('}').context("no JSON in claude output")?;
    let draft: Draft = serde_json::from_str(&text[start..=end])
        .with_context(|| format!("unparseable draft: {}", &text[..text.len().min(200)]))?;
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
    let out = Command::new("claude")
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
    fn rejects_path_traversal_names() {
        assert!(parse_draft("{\"name\":\"../evil\",\"kind\":\"script\",\"summary\":\"s\",\"content\":\"c\"}").is_err());
    }

    #[test]
    fn rejects_unknown_kind() {
        assert!(parse_draft("{\"name\":\"x\",\"kind\":\"daemon\",\"summary\":\"s\",\"content\":\"c\"}").is_err());
    }
}
