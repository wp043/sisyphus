use crate::normalize;
use anyhow::Result;
use rusqlite::{Connection, params};

/// Tiny deterministic LCG so seeded data is reproducible without a rand dep.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[(self.next() as usize) % xs.len()]
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.next() % 100 < pct
    }
}

struct Writer<'a> {
    conn: &'a Connection,
    seq: i64,
    n: usize,
}

impl<'a> Writer<'a> {
    fn cmd(&mut self, source: &str, session: &str, raw: &str, ts: i64, failed: bool) -> Result<()> {
        let head = raw.split_whitespace().next().unwrap_or("");
        self.seq += 1;
        self.conn.execute(
            "INSERT INTO commands (source, raw, head, ts, session_key, seq, failed)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![source, raw, head, ts, session, self.seq, failed as i64],
        )?;
        self.n += 1;
        Ok(())
    }
}

/// Generate `days` of plausible developer history: repeated deploy/commit
/// workflows, agent fix-loops with failures, near-duplicate prompts, plus
/// background noise — everything the miner is supposed to find, with
/// timestamps so the dashboard chart and future time-based scoring light up.
pub fn run(conn: &Connection, days: i64) -> Result<usize> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let mut rng = Rng(42);
    let mut w = Writer { conn, seq: 1_000_000, n: 0 };

    let noise = [
        "ls", "git status", "clear", "cat README.md", "which node", "brew update",
        "cd ~/Documents/Tech", "nvim src/main.rs", "rg TODO", "docker ps",
    ];
    let prompts = [
        "summarize this PR and check the types",
        "please summarize the PR then check types",
        "summarize this pull request and verify the types",
        "can you summarize this PR and check its types",
        "write release notes from the last 5 commits",
        "generate release notes based on recent commits",
        "draft release notes from the latest commits",
    ];

    for day in (0..days).rev() {
        let base = now - day * 86400 + 9 * 3600; // workday starts 9:00
        let mut t = base;
        let session = format!("seed-day-{day}");

        // morning noise
        for _ in 0..(2 + rng.next() % 4) {
            t += 60 + (rng.next() % 600) as i64;
            w.cmd("zsh", "", *rng.pick(&noise), t, false)?;
        }

        // deploy workflow ~3 days out of 5
        if rng.chance(60) {
            for step in ["git pull", "npm run build", "scp -r dist/ deploy@web1:/srv/app", "curl -s https://app.example.com/health"] {
                t += 30 + (rng.next() % 240) as i64;
                w.cmd("zsh", "", step, t, false)?;
            }
        }

        // commit habit, most days
        if rng.chance(75) {
            for step in ["git add .", "git commit -m \"update\"", "git push"] {
                t += 20 + (rng.next() % 120) as i64;
                w.cmd("zsh", "", step, t, false)?;
            }
        }

        // agent session with a build fix-loop (fail, fail, pass)
        if rng.chance(50) {
            let runs = 3 + (rng.next() % 3) as i64;
            for i in 0..runs {
                t += 45 + (rng.next() % 180) as i64;
                w.cmd("claude", &session, "cargo build", t, i < runs - 1)?;
                if i < runs - 1 {
                    t += 60;
                    w.cmd("claude", &session, "cat src/lib.rs", t, false)?;
                }
            }
            t += 60;
            w.cmd("claude", &session, "cargo test", t, false)?;
        }

        // codex session doing a migration check loop
        if rng.chance(35) {
            for step in ["npx prisma migrate dev", "npx prisma db seed", "npm test"] {
                t += 60 + (rng.next() % 200) as i64;
                w.cmd("codex", &session, step, t, rng.chance(25))?;
            }
        }

        // a prompt, followed by what the agent then runs — the PR-review ask
        // always leads to the same routine, planting an intent pattern
        if rng.chance(70) {
            t += 300;
            let prompt = *rng.pick(&prompts);
            w.cmd("claude_prompt", &session, prompt, t, false)?;
            if prompt.contains("PR") || prompt.contains("pull request") {
                for step in ["gh pr view 128", "gh pr diff 128", "npx tsc --noEmit"] {
                    t += 40 + (rng.next() % 90) as i64;
                    w.cmd("claude", &session, step, t, false)?;
                }
            }
        }
    }

    normalize::run(conn)?;
    Ok(w.n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{db, mine};

    /// End-to-end: seed a fresh in-memory DB, mine it, and assert the miner
    /// finds each kind of planted pattern.
    #[test]
    fn miner_finds_planted_patterns() {
        let conn = Connection::open_in_memory().unwrap();
        db::init_schema(&conn).unwrap();
        let n = run(&conn, 21).unwrap();
        assert!(n > 100, "seeded only {n} commands");

        let cands = mine::candidates(&conn, 10, None, false).unwrap();
        let kinds: std::collections::HashSet<_> =
            cands.iter().map(|c| c.kind.clone()).collect();
        assert!(kinds.contains("sequence"), "no sequences found");
        assert!(kinds.contains("fixloop"), "no fix-loops found");
        assert!(kinds.contains("prompt"), "no prompt clusters found");

        // the planted deploy workflow must surface as one sequence
        let deploy = cands.iter().any(|c| {
            c.kind == "sequence"
                && c.templates.iter().any(|t| t.starts_with("git pull"))
                && c.templates.iter().any(|t| t.starts_with("npm run build"))
        });
        assert!(deploy, "deploy workflow not mined: {:?}",
            cands.iter().map(|c| &c.templates).collect::<Vec<_>>());

        // the cargo build fix-loop must be detected
        let fixloop = cands
            .iter()
            .any(|c| c.kind == "fixloop" && c.templates[0].starts_with("cargo build"));
        assert!(fixloop, "cargo build fix-loop not mined");

        // the PR ask + its routine must fuse into an intent
        let intent = cands.iter().find(|c| c.kind == "intent");
        let intent = intent.expect("no intent mined");
        assert!(intent.templates[0].starts_with("ask: "), "{:?}", intent.templates);
        assert!(
            intent.templates.iter().any(|t| t.starts_with("gh pr view")),
            "intent missing agent steps: {:?}",
            intent.templates
        );
        // and the bare prompt cluster it grew from must not also appear
        let ask = intent.templates[0].strip_prefix("ask: ").unwrap();
        assert!(
            !cands.iter().any(|c| c.kind == "prompt" && c.templates[0] == ask),
            "intent's prompt cluster leaked through as separate candidate"
        );
    }
}
