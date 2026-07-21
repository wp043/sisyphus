use anyhow::Result;
use rusqlite::{Connection, params};
use std::collections::HashMap;

const MIN_LEN: usize = 2;
const MAX_LEN: usize = 7;
const MIN_COUNT: usize = 3;

/// Templates that carry no automation signal on their own; a pattern made
/// entirely of these is navigation noise, not a workflow.
fn is_noise(tpl: &str) -> bool {
    let head = tpl.split_whitespace().next().unwrap_or("");
    matches!(head, "ls" | "ll" | "la" | "pwd" | "clear" | "exit" | "history" | "cd" | "source" | "which" | "echo" | "cat" if !tpl.contains("&&"))
        || matches!(tpl, "git status" | "git diff" | "git log")
}

/// True if the gram is (a prefix of) repetitions of a shorter unit — e.g.
/// [A,B,A,B], [A,B,A], [B,A,B] all reduce to the [A,B] cycle.
fn is_quasi_periodic(gram: &[String]) -> bool {
    (1..gram.len()).any(|p| (p..gram.len()).all(|i| gram[i] == gram[i % p]))
}

/// Lexicographically smallest rotation, used to group cyclic duplicates.
fn canonical_rotation(gram: &[String]) -> Vec<String> {
    (0..gram.len())
        .map(|r| {
            let mut v = gram.to_vec();
            v.rotate_left(r);
            v
        })
        .min()
        .unwrap_or_default()
}

#[derive(Debug)]
pub struct Pattern {
    pub templates: Vec<String>,
    pub count: usize,
    pub score: f64,
}

/// Estimated wall-clock seconds a step costs: its recorded command duration
/// when known, otherwise the gap until the next action in the same session
/// (capped at 5 min so idle time doesn't skew it). Returns per-template medians
/// plus a global fallback for templates without timing data — this is what
/// turns "seen 14×" into "~84 min wasted".
fn template_seconds(conn: &Connection) -> Result<(HashMap<String, f64>, f64)> {
    let mut stmt = conn.prepare(
        "SELECT COALESCE(session_key, ''), template, ts, duration_ms FROM commands
         WHERE template IS NOT NULL
           AND COALESCE(session_key, '') NOT IN (SELECT session_key FROM superseded)
         ORDER BY source, session_key, seq",
    )?;
    let rows: Vec<(String, String, Option<i64>, Option<i64>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<std::result::Result<_, _>>()?;

    let mut per: HashMap<String, Vec<f64>> = HashMap::new();
    for (i, (sess, tpl, ts, dur)) in rows.iter().enumerate() {
        let secs = if let Some(ms) = dur {
            (*ms as f64 / 1000.0).clamp(0.0, 300.0)
        } else if let (Some(t0), Some(next)) = (ts, rows.get(i + 1)) {
            // gap to the next action, but only within the same session
            match (next.0 == *sess, next.2) {
                (true, Some(t1)) => ((t1 - t0).max(0) as f64).min(300.0),
                _ => continue,
            }
        } else {
            continue;
        };
        per.entry(tpl.clone()).or_default().push(secs);
    }

    let median = |v: &mut Vec<f64>| {
        v.sort_by(|a, b| a.total_cmp(b));
        v[v.len() / 2]
    };
    let mut medians = HashMap::new();
    let mut all = Vec::new();
    for (t, mut v) in per {
        let m = median(&mut v);
        all.push(m);
        medians.insert(t, m);
    }
    // a sensible default when a step has never been timed
    let fallback = if all.is_empty() { 12.0 } else { median(&mut all) };
    Ok((medians, fallback))
}

/// Detect transcript overlap and record it. Agent tools re-log a continued
/// conversation into a fresh session file that replays all prior events, so a
/// continuation file's event stream is a prefix of (or identical to) the more
/// complete file. Any session whose full event fingerprint is a prefix of
/// another's is marked superseded, so mining counts each real event once.
/// Returns how many sessions were collapsed.
pub fn dedupe_sessions(conn: &Connection) -> Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT session_key, COALESCE(template, raw) FROM commands
         WHERE session_key IS NOT NULL AND session_key != ''
         ORDER BY session_key, seq",
    )?;
    let mut by_session: HashMap<String, Vec<String>> = HashMap::new();
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
        let (k, ev) = row?;
        by_session.entry(k).or_default().push(ev);
    }

    // longest first (ties by key) so a replayed prefix is always compared
    // against the more complete session that supersedes it
    let mut sessions: Vec<(String, Vec<String>)> = by_session.into_iter().collect();
    sessions.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));

    let mut kept: Vec<&[String]> = Vec::new();
    let mut superseded: Vec<&str> = Vec::new();
    for (key, fp) in &sessions {
        // require a non-trivial stream to avoid dropping a short standalone
        // session that coincidentally starts like a longer unrelated one
        let is_replay =
            fp.len() >= 3 && kept.iter().any(|k| k.len() >= fp.len() && k[..fp.len()] == fp[..]);
        if is_replay {
            superseded.push(key);
        } else {
            kept.push(fp.as_slice());
        }
    }

    conn.execute("DELETE FROM superseded", [])?;
    let mut ins = conn.prepare("INSERT OR IGNORE INTO superseded (session_key) VALUES (?1)")?;
    for k in &superseded {
        ins.execute([k])?;
    }
    Ok(superseded.len())
}

/// Ordered streams of (template, command row id): one per agent session, plus
/// the whole zsh history as a single stream (it has no session boundaries
/// without EXTENDED_HISTORY timestamps).
fn load_streams(conn: &Connection) -> Result<Vec<Vec<(String, i64)>>> {
    let mut stmt = conn.prepare(
        "SELECT source, COALESCE(session_key, ''), template, id FROM commands
         WHERE template IS NOT NULL AND source IN ('zsh','claude','codex')
           AND COALESCE(session_key, '') NOT IN (SELECT session_key FROM superseded)
         ORDER BY source, session_key, seq",
    )?;
    let mut streams: Vec<Vec<(String, i64)>> = Vec::new();
    let mut current_key = None::<(String, String)>;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;
    for row in rows {
        let (source, session, tpl, id) = row?;
        let key = (source, session);
        if current_key.as_ref() != Some(&key) {
            current_key = Some(key);
            streams.push(Vec::new());
        }
        streams.last_mut().unwrap().push((tpl, id));
    }
    Ok(streams)
}

/// Collapse immediate repeats (`pnpm run dev` × 14 in a row is one attempt
/// loop, not 14 workflow steps).
fn dedup_runs(stream: &[(String, i64)]) -> Vec<(String, i64)> {
    let mut out: Vec<(String, i64)> = Vec::new();
    for (tpl, id) in stream {
        if out.last().map(|(t, _)| t) != Some(tpl) {
            out.push((tpl.clone(), *id));
        }
    }
    out
}

/// How many times this exact sequence occurred with its first command newer
/// than `min_id` — i.e. the manual repetitions since a decision was made.
pub fn occurrences_since(conn: &Connection, templates: &[String], min_id: i64) -> Result<usize> {
    if templates.is_empty() {
        return Ok(0);
    }
    let mut count = 0;
    for stream in load_streams(conn)?.iter().map(|s| dedup_runs(s)) {
        let mut i = 0;
        while i + templates.len() <= stream.len() {
            let window = &stream[i..i + templates.len()];
            if window.iter().zip(templates).all(|((t, _), want)| t == want) {
                if window[0].1 > min_id {
                    count += 1;
                }
                i += templates.len();
            } else {
                i += 1;
            }
        }
    }
    Ok(count)
}

pub fn mine(conn: &Connection) -> Result<Vec<Pattern>> {
    let streams: Vec<Vec<(String, i64)>> =
        load_streams(conn)?.iter().map(|s| dedup_runs(s)).collect();

    // gram -> list of (stream index, position)
    let mut occurrences: HashMap<Vec<String>, Vec<(usize, usize)>> = HashMap::new();
    for (si, stream) in streams.iter().enumerate() {
        for start in 0..stream.len() {
            for len in MIN_LEN..=MAX_LEN.min(stream.len() - start) {
                let gram: Vec<String> =
                    stream[start..start + len].iter().map(|(t, _)| t.clone()).collect();
                occurrences.entry(gram).or_default().push((si, start));
            }
        }
    }

    // count non-overlapping occurrences per gram (greedy left-to-right)
    let mut counted: Vec<(Vec<String>, usize)> = occurrences
        .into_iter()
        .filter_map(|(gram, mut occ)| {
            occ.sort_unstable();
            let len = gram.len();
            let mut count = 0usize;
            let mut last: Option<(usize, usize)> = None;
            for (si, pos) in occ {
                if let Some((lsi, lpos)) = last
                    && lsi == si && pos < lpos + len {
                        continue;
                    }
                count += 1;
                last = Some((si, pos));
            }
            (count >= MIN_COUNT).then_some((gram, count))
        })
        .collect();

    // a gram that just repeats a shorter unit ([A,B,A,B,A] = period 2) is an
    // iteration loop, not a longer workflow — the unit itself carries the signal
    counted.retain(|(gram, _)| !is_quasi_periodic(gram));

    // a template showing up 3+ times inside one gram is a retry loop wearing a
    // workflow costume — that signal belongs to fix-loop mining
    counted.retain(|(gram, _)| {
        let mut freq: HashMap<&String, usize> = HashMap::new();
        for t in gram {
            *freq.entry(t).or_default() += 1;
        }
        freq.values().all(|&n| n < 3)
    });

    // closed/overlap filter: drop a gram when a kept gram (longer or shifted)
    // shares all but one of its steps with similar support — shifted windows of
    // one true workflow shouldn't surface as separate patterns
    counted.sort_by_key(|(g, c)| (std::cmp::Reverse(g.len()), std::cmp::Reverse(*c)));
    let mut kept: Vec<(Vec<String>, usize)> = Vec::new();
    'outer: for (gram, count) in counted {
        let w = (gram.len() - 1).max(2).min(gram.len());
        for (other, ocount) in &kept {
            if *ocount as f64 >= count as f64 * 0.6
                && gram
                    .windows(w)
                    .any(|gw| other.windows(w.min(other.len())).any(|ow| ow == gw))
            {
                continue 'outer;
            }
        }
        kept.push((gram, count));
    }

    // rotations of a cycle ([A,B] vs [B,A]) describe the same loop; keep the
    // most frequent representative per rotation class
    let mut by_rotation: HashMap<Vec<String>, (Vec<String>, usize)> = HashMap::new();
    for (gram, count) in kept {
        let key = canonical_rotation(&gram);
        let entry = by_rotation.entry(key).or_insert_with(|| (gram.clone(), count));
        if count > entry.1 {
            *entry = (gram, count);
        }
    }

    let (secs, fallback) = template_seconds(conn)?;
    let mut patterns: Vec<Pattern> = by_rotation
        .into_values()
        .filter(|(gram, _)| {
            let distinct: std::collections::HashSet<_> = gram.iter().collect();
            distinct.len() >= 2 && !gram.iter().all(|t| is_noise(t))
        })
        .map(|(templates, count)| {
            // score = total estimated seconds spent doing this by hand
            let per_run: f64 = templates.iter().map(|t| secs.get(t).copied().unwrap_or(fallback)).sum();
            let score = count as f64 * per_run;
            Pattern { templates, count, score }
        })
        .collect();
    patterns.sort_by(|a, b| b.score.total_cmp(&a.score));
    Ok(patterns)
}

/// A repeated same-command retry cycle inside agent sessions: the command was
/// re-run after failures, meaning an agent (or the user) sat in an
/// execute→fix→retry loop.
pub fn fix_loops(conn: &Connection) -> Result<Vec<Pattern>> {
    let mut stmt = conn.prepare(
        "SELECT template, SUM(runs), SUM(fails), COUNT(*) FROM (
            SELECT template, COUNT(*) runs, SUM(COALESCE(failed, 0)) fails
            FROM commands
            WHERE source IN ('claude', 'codex', 'zsh') AND template IS NOT NULL
              AND COALESCE(session_key, '') NOT IN (SELECT session_key FROM superseded)
            -- zsh has no sessions; day-bucket it (only hook rows carry ts +
            -- exit codes, so plain history can't produce false fails)
            GROUP BY source, COALESCE(session_key, date(ts, 'unixepoch')), template
            HAVING runs >= 3 AND fails >= 2
         ) GROUP BY template ORDER BY SUM(runs) DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(3)?))
    })?;
    let (secs, fallback) = template_seconds(conn)?;
    let mut out = Vec::new();
    for row in rows {
        let (template, runs, sessions) = row?;
        if is_noise(&template) {
            continue;
        }
        // a failing retry loop wastes more than nominal runtime (debugging
        // between tries); weight by how many sessions it plagued
        let per_run = secs.get(&template).copied().unwrap_or(fallback);
        let score = runs as f64 * per_run * (1.0 + sessions as f64 * 0.25);
        out.push(Pattern { templates: vec![template], count: runs as usize, score });
    }
    Ok(out)
}

/// Words that don't distinguish one intent from another: politeness, glue,
/// and the interchangeable "produce something" verbs. Stripping them lets
/// "write release notes" and "generate release notes" land in one cluster.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "this", "that", "then", "please", "can", "you", "your",
    "its", "with", "from", "into", "onto", "using", "about", "also", "just",
    "write", "generate", "draft", "create", "make", "give", "help", "based",
];

fn word_set(text: &str) -> std::collections::HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3 && !STOPWORDS.contains(w))
        .map(String::from)
        .collect()
}

struct PromptRow {
    source: String, // 'claude_prompt' | 'codex_prompt' | 'gemini_prompt'
    session: String,
    seq: i64,
    raw: String,
}

/// Minimum distinct content words a prompt must have to be clusterable. Below
/// this, near-empty word sets cluster on a single shared token and inflate
/// counts ("add certificate" pooling with every other 1-word ask).
const MIN_CONTENT_WORDS: usize = 3;

/// Skill directives and tool wrappers that ride in the prompt stream but aren't
/// natural-language asks (e.g. "$autonomous-skill …", slash commands).
fn is_directive(raw: &str) -> bool {
    let t = raw.trim_start();
    t.starts_with('$') || t.starts_with('/') || t.starts_with('!')
}

fn load_prompts(conn: &Connection) -> Result<Vec<PromptRow>> {
    let mut stmt = conn.prepare(
        "SELECT source, COALESCE(session_key, ''), seq, raw FROM commands
         WHERE source LIKE '%_prompt' AND LENGTH(raw) BETWEEN 12 AND 300
           AND COALESCE(session_key, '') NOT IN (SELECT session_key FROM superseded)",
    )?;
    let rows: Vec<PromptRow> = stmt
        .query_map([], |r| {
            Ok(PromptRow { source: r.get(0)?, session: r.get(1)?, seq: r.get(2)?, raw: r.get(3)? })
        })?
        .collect::<std::result::Result<_, _>>()?;

    // Deduplicate identical prompt text. Agent transcripts overlap heavily:
    // when a long conversation continues into a new session file, the whole
    // prior exchange is replayed with fresh timestamps, so one real utterance
    // is stored many times. A user doesn't type the same sentence verbatim
    // across genuinely separate sessions — exact-identical text is an overlap
    // artifact. Near-duplicate clustering still catches real paraphrase repeats.
    let mut seen = std::collections::HashSet::new();
    Ok(rows
        .into_iter()
        .filter(|p| !is_directive(&p.raw) && word_set(&p.raw).len() >= MIN_CONTENT_WORDS)
        .filter(|p| seen.insert(p.raw.clone()))
        .collect())
}

/// Union-find clustering of near-duplicate prompts (Jaccard over word sets);
/// returns member-index groups of size >= 3. O(n²) is fine at personal scale.
fn cluster_prompts(rows: &[PromptRow]) -> Vec<Vec<usize>> {
    let sets: Vec<_> = rows.iter().map(|p| word_set(&p.raw)).collect();
    let mut parent: Vec<usize> = (0..rows.len()).collect();
    fn find(parent: &mut Vec<usize>, i: usize) -> usize {
        if parent[i] != i {
            parent[i] = find(parent, parent[i]);
        }
        parent[i]
    }
    for i in 0..rows.len() {
        for j in i + 1..rows.len() {
            let inter = sets[i].intersection(&sets[j]).count();
            let union = sets[i].len() + sets[j].len() - inter;
            // require real overlap (≥2 shared words) AND a high ratio, so a
            // single coincidental token can't fuse two unrelated asks
            if inter >= 2 && union > 0 && inter as f64 / union as f64 >= 0.4 {
                let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                if ri != rj {
                    parent[ri] = rj;
                }
            }
        }
    }
    let mut clusters: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..rows.len() {
        let root = find(&mut parent, i);
        clusters.entry(root).or_default().push(i);
    }
    clusters.into_values().filter(|m| m.len() >= 3).collect()
}

fn representative(rows: &[PromptRow], members: &[usize]) -> String {
    members
        .iter()
        .map(|&i| rows[i].raw.clone())
        .min_by_key(|p| p.len())
        .unwrap_or_default()
}

/// Cluster near-duplicate prompts across all AI tools: asking the same kind of
/// thing 3+ times is a skill waiting to exist.
pub fn prompt_clusters(conn: &Connection) -> Result<Vec<Pattern>> {
    let rows = load_prompts(conn)?;
    let mut out: Vec<Pattern> = cluster_prompts(&rows)
        .into_iter()
        .map(|members| {
            let count = members.len();
            let rep = representative(&rows, &members);
            Pattern { templates: vec![rep], count, score: count as f64 * 2.0 }
        })
        .collect();
    out.sort_by(|a, b| b.score.total_cmp(&a.score));
    Ok(out)
}

/// The deepest AI-side signal: a recurring ask *paired with* what the agent
/// then actually runs. For each prompt cluster, look at the commands between
/// each member prompt and the next prompt in its session; templates common to
/// most instances are the intent's known steps. Drafted as a skill, those
/// steps stop being rediscovered from scratch every time.
pub fn intents(conn: &Connection) -> Result<Vec<Pattern>> {
    let rows = load_prompts(conn)?;
    let (secs, fallback) = template_seconds(conn)?;
    let mut cmd_stmt = conn.prepare(
        "SELECT template FROM commands
         WHERE source = ?1 AND session_key = ?2 AND seq > ?3 AND seq < ?4
           AND template IS NOT NULL
         ORDER BY seq LIMIT 15",
    )?;

    let mut out = Vec::new();
    for members in cluster_prompts(&rows) {
        let mut instances: Vec<Vec<String>> = Vec::new();
        for &i in &members {
            let p = &rows[i];
            let cmd_source = p.source.trim_end_matches("_prompt");
            // commands attributed to this prompt end where the next prompt starts
            let next_seq = rows
                .iter()
                .filter(|o| o.source == p.source && o.session == p.session && o.seq > p.seq)
                .map(|o| o.seq)
                .min()
                .unwrap_or(i64::MAX);
            let mut seen = std::collections::HashSet::new();
            let templates: Vec<String> = cmd_stmt
                .query_map(params![cmd_source, p.session, p.seq, next_seq], |r| r.get(0))?
                .filter_map(|t| t.ok())
                .filter(|t: &String| seen.insert(t.clone()))
                .collect();
            if !templates.is_empty() {
                instances.push(templates);
            }
        }
        if instances.len() < 2 {
            continue;
        }
        // a step counts as "the known routine" if most instances include it
        let threshold = ((instances.len() as f64 * 0.6).ceil() as usize).max(2);
        let mut freq: HashMap<&String, (usize, usize)> = HashMap::new(); // count, position sum
        for inst in &instances {
            for (pos, t) in inst.iter().enumerate() {
                let e = freq.entry(t).or_default();
                e.0 += 1;
                e.1 += pos;
            }
        }
        let mut common: Vec<(&String, usize)> = freq
            .iter()
            .filter(|(t, (n, _))| *n >= threshold && !is_noise(t))
            .map(|(t, (n, pos_sum))| (*t, pos_sum / n))
            .collect();
        common.sort_by_key(|(_, avg_pos)| *avg_pos);
        if common.len() < 2 {
            continue;
        }
        let steps: Vec<String> = common.into_iter().map(|(t, _)| t.clone()).collect();
        // time saved per invocation = the routine the agent no longer rediscovers,
        // weighted up because the ask itself is recurring
        let per_run: f64 = steps.iter().map(|t| secs.get(t).copied().unwrap_or(fallback)).sum();
        let count = members.len();
        let score = count as f64 * per_run * 1.5;
        let mut templates = vec![format!("ask: {}", representative(&rows, &members))];
        templates.extend(steps);
        out.push(Pattern { templates, count, score });
    }
    out.sort_by(|a, b| b.score.total_cmp(&a.score));
    Ok(out)
}

pub struct Candidate {
    pub id: i64,
    pub kind: String,
    pub templates: Vec<String>,
    pub count: usize,
    pub score: f64,
    pub project: Option<String>,
}

/// The repo a pattern predominantly lives in: the basename of the most common
/// working directory among commands matching its most distinctive step. Lets
/// the report say "this deploy dance is in fitlens".
pub fn pattern_project(conn: &Connection, templates: &[String]) -> Result<Option<String>> {
    // the "ask:" head isn't a command; pick the first real step to match on
    let Some(step) = templates.iter().find(|t| !t.starts_with("ask: ")) else {
        return Ok(None);
    };
    let cwd: Option<String> = conn
        .query_row(
            "SELECT cwd FROM commands
             WHERE template = ?1 AND cwd IS NOT NULL AND cwd != ''
             GROUP BY cwd ORDER BY COUNT(*) DESC LIMIT 1",
            params![step],
            |r| r.get(0),
        )
        .ok();
    Ok(cwd.and_then(|p| {
        p.trim_end_matches('/')
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .map(String::from)
    }))
}

/// How many times an ask (a prompt cluster's representative) recurred after a
/// decision — the prompt-side analogue of `occurrences_since`, used by evolve
/// to judge intent/prompt adoption (their patterns aren't command sequences).
pub fn ask_recurrence_since(conn: &Connection, ask: &str, min_id: i64) -> Result<usize> {
    let target = word_set(ask);
    if target.is_empty() {
        return Ok(0);
    }
    let mut stmt = conn.prepare(
        "SELECT raw FROM commands WHERE source LIKE '%_prompt' AND id > ?1",
    )?;
    let mut n = 0;
    let rows = stmt.query_map(params![min_id], |r| r.get::<_, String>(0))?;
    for raw in rows {
        let set = word_set(&raw?);
        let inter = target.intersection(&set).count();
        let union = target.len() + set.len() - inter;
        if inter >= 2 && union > 0 && inter as f64 / union as f64 >= 0.4 {
            n += 1;
        }
    }
    Ok(n)
}

impl Candidate {
    /// Human label for the pattern's cost. Command-based kinds carry an
    /// estimated total time spent doing this by hand across history; prompt
    /// clusters have no command timing, so they fall back to a raw score.
    pub fn cost_label(&self) -> String {
        if self.kind == "prompt" {
            return format!("score {:.0}", self.score);
        }
        let mins = self.score / 60.0;
        if mins >= 1.0 {
            format!("~{mins:.0} min by hand")
        } else {
            format!("~{:.0}s by hand", self.score)
        }
    }
}

/// Mine everything, persist patterns, and return the undecided candidates.
pub fn candidates(conn: &Connection, limit_per_kind: usize) -> Result<Vec<Candidate>> {
    let intent_patterns = intents(conn)?;
    // an intent subsumes the bare prompt cluster it grew from
    let intent_asks: std::collections::HashSet<String> = intent_patterns
        .iter()
        .filter_map(|p| p.templates[0].strip_prefix("ask: ").map(String::from))
        .collect();
    let prompt_patterns: Vec<Pattern> = prompt_clusters(conn)?
        .into_iter()
        .filter(|p| !intent_asks.contains(&p.templates[0]))
        .collect();

    let mut out = Vec::new();
    for (kind, patterns) in [
        ("sequence", mine(conn)?),
        ("fixloop", fix_loops(conn)?),
        ("intent", intent_patterns),
        ("prompt", prompt_patterns),
    ] {
        let mut kept = 0;
        for p in patterns {
            let key = serde_json::to_string(&p.templates)?;
            conn.execute(
                "INSERT INTO patterns (kind, template_seq, count, score) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(template_seq) DO UPDATE SET count = ?3, score = ?4",
                params![kind, key, p.count as i64, p.score],
            )?;
            let id: i64 = conn.query_row(
                "SELECT id FROM patterns WHERE template_seq = ?1",
                params![key],
                |r| r.get(0),
            )?;
            let decided: bool = conn
                .query_row("SELECT 1 FROM decisions WHERE pattern_id = ?1", params![id], |_| Ok(true))
                .unwrap_or(false);
            if !decided && kept < limit_per_kind {
                kept += 1;
                let project = pattern_project(conn, &p.templates)?;
                out.push(Candidate {
                    id,
                    kind: kind.into(),
                    templates: p.templates,
                    count: p.count,
                    score: p.score,
                    project,
                });
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn_with(cmds: &[(&str, &str, i64)]) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE commands (id INTEGER PRIMARY KEY, source TEXT, raw TEXT, template TEXT,
             head TEXT, ts INTEGER, duration_ms INTEGER, cwd TEXT, project TEXT,
             session_key TEXT, seq INTEGER, failed INTEGER);
             CREATE TABLE patterns (id INTEGER PRIMARY KEY, kind TEXT, template_seq TEXT UNIQUE,
             count INTEGER, score REAL, first_ts INTEGER, last_ts INTEGER);
             CREATE TABLE decisions (pattern_id INTEGER PRIMARY KEY, decision TEXT,
             artifact_path TEXT, ts INTEGER);
             CREATE TABLE superseded (session_key TEXT PRIMARY KEY);",
        )
        .unwrap();
        for (src, tpl, seq) in cmds {
            conn.execute(
                "INSERT INTO commands (source, raw, template, session_key, seq) VALUES (?1, ?2, ?2, '', ?3)",
                params![src, tpl, seq],
            )
            .unwrap();
        }
        conn
    }

    #[test]
    fn finds_repeated_sequence() {
        let mut rows = Vec::new();
        let mut seq = 0;
        for _ in 0..4 {
            for t in ["git add .", "git commit -m <msg>", "git push", "vim"] {
                rows.push(("zsh", t, seq));
                seq += 1;
            }
        }
        let conn = conn_with(&rows);
        let patterns = mine(&conn).unwrap();
        assert!(!patterns.is_empty());
        let top = &patterns[0];
        assert!(top.templates.contains(&"git push".to_string()), "{:?}", top.templates);
        assert!(top.count >= 3);
    }

    #[test]
    fn drops_noise_only_patterns() {
        let mut rows = Vec::new();
        for i in 0..10 {
            rows.push(("zsh", if i % 2 == 0 { "ls" } else { "cd <path>" }, i));
        }
        let conn = conn_with(&rows);
        let patterns = mine(&conn).unwrap();
        assert!(patterns.is_empty(), "{:?}", patterns.iter().map(|p| &p.templates).collect::<Vec<_>>());
    }

    #[test]
    fn counts_occurrences_after_a_boundary() {
        let mut rows = Vec::new();
        let mut seq = 0;
        for _ in 0..4 {
            for t in ["git add .", "git push"] {
                rows.push(("zsh", t, seq));
                seq += 1;
            }
        }
        let conn = conn_with(&rows);
        let boundary: i64 = conn
            .query_row("SELECT MAX(id) FROM commands", [], |r| r.get(0))
            .unwrap();
        let tpls = vec!["git add .".to_string(), "git push".to_string()];
        assert_eq!(occurrences_since(&conn, &tpls, 0).unwrap(), 4);
        assert_eq!(occurrences_since(&conn, &tpls, boundary).unwrap(), 0);
        // two more manual repetitions after the boundary
        for t in ["git add .", "git push", "git add .", "git push"] {
            conn.execute(
                "INSERT INTO commands (source, raw, template, session_key, seq) VALUES ('zsh', ?1, ?1, '', ?2)",
                params![t, seq],
            )
            .unwrap();
            seq += 1;
        }
        assert_eq!(occurrences_since(&conn, &tpls, boundary).unwrap(), 2);
    }

    #[test]
    fn detects_fix_loop() {
        let conn = conn_with(&[]);
        for i in 0..4 {
            conn.execute(
                "INSERT INTO commands (source, raw, template, session_key, seq, failed) VALUES ('claude', 'cargo build', 'cargo build', 's1', ?1, ?2)",
                params![i, (i < 2) as i64],
            )
            .unwrap();
        }
        let loops = fix_loops(&conn).unwrap();
        assert_eq!(loops.len(), 1);
        assert_eq!(loops[0].templates, vec!["cargo build".to_string()]);
        assert_eq!(loops[0].count, 4);
    }

    #[test]
    fn clusters_similar_prompts() {
        let conn = conn_with(&[]);
        let prompts = [
            "summarize this PR and check the types please",
            "summarize the PR then check types",
            "please summarize this PR and check its types",
            "write a haiku about rust",
        ];
        for (i, p) in prompts.iter().enumerate() {
            conn.execute(
                "INSERT INTO commands (source, raw, session_key, seq) VALUES ('claude_prompt', ?1, 's1', ?2)",
                params![p, i as i64],
            )
            .unwrap();
        }
        let clusters = prompt_clusters(&conn).unwrap();
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].count, 3);
        assert!(clusters[0].templates[0].contains("summarize"));
    }

    #[test]
    fn dedupes_replayed_sessions() {
        let conn = conn_with(&[]);
        let insert = |sess: &str, seq: i64, tpl: &str| {
            conn.execute(
                "INSERT INTO commands (source, raw, template, session_key, seq) VALUES ('codex', ?1, ?1, ?2, ?3)",
                params![tpl, sess, seq],
            )
            .unwrap();
        };
        // session B replays A's three commands then adds one more (a continuation)
        for (i, t) in ["npm ci", "npm run build", "npm test"].iter().enumerate() {
            insert("A", i as i64, t);
        }
        for (i, t) in ["npm ci", "npm run build", "npm test", "npm run deploy"].iter().enumerate() {
            insert("B", i as i64, t);
        }
        let collapsed = dedupe_sessions(&conn).unwrap();
        assert_eq!(collapsed, 1); // A is a prefix of B → superseded
        let gone: bool = conn
            .query_row("SELECT 1 FROM superseded WHERE session_key='A'", [], |_| Ok(true))
            .unwrap_or(false);
        assert!(gone);
        // the replayed sequence must now be counted once, not twice
        let seqs = mine(&conn).unwrap();
        let build = seqs.iter().find(|p| p.templates.iter().any(|t| t == "npm run build"));
        assert!(build.is_none() || build.unwrap().count == 1);
    }

    #[test]
    fn ask_recurrence_counts_matches_after_boundary() {
        let conn = conn_with(&[]);
        let add = |seq: i64, raw: &str| {
            conn.execute(
                "INSERT INTO commands (source, raw, session_key, seq) VALUES ('claude_prompt', ?1, 's', ?2)",
                params![raw, seq],
            )
            .unwrap();
        };
        add(0, "summarize this PR and check the types");
        let boundary: i64 = conn.query_row("SELECT MAX(id) FROM commands", [], |r| r.get(0)).unwrap();
        add(1, "please summarize the PR then check its types"); // recurrence after
        add(2, "write a poem about the ocean"); // unrelated
        let n = ask_recurrence_since(&conn, "summarize this PR and check the types", boundary).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn short_oneoffs_do_not_pool() {
        let conn = conn_with(&[]);
        // distinct short asks sharing at most one incidental word, plus a
        // skill directive — none should cluster or inflate a count
        let prompts = [
            "add certificate",
            "add license file",
            "fix the build",
            "$autonomous-skill build recommended features autonomously",
            "yes change to wendy pan",
        ];
        for (i, p) in prompts.iter().enumerate() {
            conn.execute(
                "INSERT INTO commands (source, raw, session_key, seq) VALUES ('claude_prompt', ?1, 's1', ?2)",
                params![p, i as i64],
            )
            .unwrap();
        }
        assert!(prompt_clusters(&conn).unwrap().is_empty());
    }

    #[test]
    fn collapses_immediate_repeats() {
        let mut rows = Vec::new();
        for i in 0..20 {
            rows.push(("zsh", "pnpm run dev", i));
        }
        let conn = conn_with(&rows);
        let patterns = mine(&conn).unwrap();
        assert!(patterns.is_empty());
    }
}
