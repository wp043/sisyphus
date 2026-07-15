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

fn build_prompt(templates: &[String], count: usize, examples: &str) -> String {
    format!(
        r#"You are drafting an automation for a developer who has manually repeated this command sequence {count} times (mined from their real shell/agent history):

{}

Concrete examples of each step from their history:
{examples}

Draft the best automation. Rules:
- kind "alias" for a trivial one-liner; kind "script" for a multi-step shell workflow (zsh, `set -euo pipefail`, positional args or flags for the parts that vary between runs — the <path>/<url>/<ver> placeholders); kind "skill" ONLY if the workflow inherently needs an AI agent to do judgment work.
- Scripts must be safe to re-run: no destructive commands beyond what the user's own sequence does, sensible error messages.
- name: short kebab-case.
- summary: one sentence, what it saves.

Respond with STRICT JSON only, no markdown fences, matching:
{{"name": "...", "kind": "script|alias|skill", "summary": "...", "content": "full artifact text"}}"#,
        templates.join("\n"),
    )
}

pub fn draft_pattern(conn: &Connection, templates: &[String], count: usize) -> Result<Draft> {
    let prompt = build_prompt(templates, count, &examples(conn, templates)?);
    let out = Command::new("claude")
        .args(["-p", &prompt])
        .output()
        .context("failed to run `claude` — is Claude Code installed and on PATH?")?;
    if !out.status.success() {
        bail!("claude -p failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    parse_draft(&text)
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
