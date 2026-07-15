# sisyphus

**Finds the boulders you keep pushing.**

sisyphus mines your real work history — zsh, Claude Code, Codex, Gemini CLI — for multi-step command sequences you keep repeating by hand, then uses Claude to draft the automation (a script, alias, or Claude Code skill) for you to accept, edit, or ignore.

Everything stays local: history is ingested into a SQLite DB on your machine, and the only network call is one `claude -p` invocation when *you* ask for a draft.

## Usage

```sh
sisyphus ingest      # incrementally pull new history from all sources
sisyphus stats       # what's been ingested, top command templates
sisyphus report      # mine patterns, then interactively draft/accept/ignore
sisyphus draft <id>  # draft one pattern non-interactively (no install)
sisyphus gain        # are accepted automations actually being used?
sisyphus evolve      # act on adoption feedback: revise, retire, resurface
sisyphus watch --install  # hourly background scan; notifies on new patterns
```

Example:

```
⚡ #1 (pattern 2) — seen 6× · score 12
     git remote add origin <path>
   → git branch -M main
   → git push -u origin main

pattern #1 (6×): [d]raft with claude / [i]gnore forever / [s]kip / [q]uit? d
── git-publish (script) — connects a repo to a remote and pushes main in one command
[a]ccept / [e]dit / [i]gnore forever / [s]kip? a
  ✓ installed: ~/.local/bin/git-publish
```

## Sources

| Source | Location | What's extracted |
|---|---|---|
| zsh | `~/.zsh_history` | commands (+timestamps with `EXTENDED_HISTORY`) |
| Claude Code | `~/.claude/projects/*/*.jsonl` | Bash tool calls, prompts |
| Codex | `~/.codex/sessions/**/rollout-*.jsonl` | exec/shell calls, prompts |
| Gemini CLI | `~/.gemini/tmp/*/logs.json` | prompts |

Ingestion is incremental (per-source cursors); re-running is cheap.

## What it finds

Three kinds of boulder, each drafted differently:

- **⚡ Repeated workflows** — frequent multi-step command sequences → drafted as a script or alias.
- **🔁 Fix-loops** — the same command re-run after failures inside agent sessions (an execute→fail→fix→retry cycle done by hand) → drafted as a Claude Code skill that runs the whole loop.
- **💬 Repeated prompts** — near-duplicate requests you keep typing to Claude/Codex/Gemini (Jaccard-clustered) → drafted as a skill that makes the intent a one-word command.

## How mining works

Commands are normalized into templates (`git checkout a1b2c3d` → `git checkout <hash>`), then frequent contiguous sequences are mined per session stream. Three filters keep the output honest: quasi-periodic grams collapse to their repeating unit, rotations of the same cycle are deduplicated, and patterns shadowed by a longer pattern with the same support are dropped. Failures are tracked from transcript tool results (`is_error` in Claude, output heuristics in Codex) to power fix-loop detection. No LLM is involved until you ask for a draft.

Accepted artifacts install to `~/.local/bin/` (scripts), `~/.config/sisyphus/aliases.zsh` (aliases), or `~/.claude/skills/` (skills).

## The loop closes

Every decision snapshots where your history stood, so sisyphus can see what happened *after* — and act on it, not just report it:

- **Accepted but not adopted** — you installed `git-publish` yet did the manual dance 4 more times, never invoking it. `sisyphus evolve` feeds the artifact plus the post-install evidence back to Claude to diagnose the mismatch (wrong args? unmemorable name? missing step?) and revises it in place — or retires it and reopens the pattern.
- **Ignored but still growing** — a pattern you dismissed that kept happening gets resurfaced with evidence.
- The hourly `watch` scan notifies about both new patterns and automations that aren't sticking.

observe → propose → install → measure → **revise** → repeat.

## Build

```sh
cargo build --release   # requires claude CLI on PATH for drafting
```
