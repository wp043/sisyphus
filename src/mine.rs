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

/// Ordered streams of (template, command row id): one per agent session, plus
/// the whole zsh history as a single stream (it has no session boundaries
/// without EXTENDED_HISTORY timestamps).
fn load_streams(conn: &Connection) -> Result<Vec<Vec<(String, i64)>>> {
    let mut stmt = conn.prepare(
        "SELECT source, COALESCE(session_key, ''), template, id FROM commands
         WHERE template IS NOT NULL AND source IN ('zsh','claude','codex')
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

    let mut patterns: Vec<Pattern> = by_rotation
        .into_values()
        .filter(|(gram, _)| {
            let distinct: std::collections::HashSet<_> = gram.iter().collect();
            distinct.len() >= 2 && !gram.iter().all(|t| is_noise(t))
        })
        .map(|(templates, count)| {
            let score = count as f64 * (templates.len() as f64 - 1.0);
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
            WHERE source IN ('claude', 'codex') AND template IS NOT NULL
            GROUP BY source, session_key, template
            HAVING runs >= 3 AND fails >= 2
         ) GROUP BY template ORDER BY SUM(runs) DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(3)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (template, runs, sessions) = row?;
        if is_noise(&template) {
            continue;
        }
        out.push(Pattern {
            templates: vec![template],
            count: runs as usize,
            score: runs as f64 * sessions as f64,
        });
    }
    Ok(out)
}

fn word_set(text: &str) -> std::collections::HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3)
        .map(String::from)
        .collect()
}

/// Cluster near-duplicate prompts across all AI tools: asking the same kind of
/// thing 3+ times is a skill waiting to exist. Jaccard similarity over word
/// sets; O(n²) is fine at personal-history scale.
pub fn prompt_clusters(conn: &Connection) -> Result<Vec<Pattern>> {
    let mut stmt = conn.prepare(
        "SELECT raw FROM commands
         WHERE source LIKE '%_prompt' AND LENGTH(raw) BETWEEN 12 AND 300",
    )?;
    let prompts: Vec<String> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    let sets: Vec<_> = prompts.iter().map(|p| word_set(p)).collect();

    // union-find over similar pairs
    let mut parent: Vec<usize> = (0..prompts.len()).collect();
    fn find(parent: &mut Vec<usize>, i: usize) -> usize {
        if parent[i] != i {
            parent[i] = find(parent, parent[i]);
        }
        parent[i]
    }
    for i in 0..prompts.len() {
        for j in i + 1..prompts.len() {
            let inter = sets[i].intersection(&sets[j]).count();
            let union = sets[i].len() + sets[j].len() - inter;
            if union > 0 && inter as f64 / union as f64 >= 0.5 {
                let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                if ri != rj {
                    parent[ri] = rj;
                }
            }
        }
    }
    let mut clusters: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..prompts.len() {
        let root = find(&mut parent, i);
        clusters.entry(root).or_default().push(i);
    }
    let mut out: Vec<Pattern> = clusters
        .into_values()
        .filter(|members| members.len() >= 3)
        .map(|members| {
            let count = members.len();
            // shortest member reads as the cleanest statement of the intent
            let rep = members
                .into_iter()
                .map(|i| prompts[i].clone())
                .min_by_key(|p| p.len())
                .unwrap_or_default();
            Pattern { templates: vec![rep], count, score: count as f64 * 2.0 }
        })
        .collect();
    out.sort_by(|a, b| b.score.total_cmp(&a.score));
    Ok(out)
}

pub struct Candidate {
    pub id: i64,
    pub kind: String,
    pub templates: Vec<String>,
    pub count: usize,
    pub score: f64,
}

/// Mine everything, persist patterns, and return the undecided candidates.
pub fn candidates(conn: &Connection, limit_per_kind: usize) -> Result<Vec<Candidate>> {
    let mut out = Vec::new();
    for (kind, patterns) in [
        ("sequence", mine(conn)?),
        ("fixloop", fix_loops(conn)?),
        ("prompt", prompt_clusters(conn)?),
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
                out.push(Candidate {
                    id,
                    kind: kind.into(),
                    templates: p.templates,
                    count: p.count,
                    score: p.score,
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
             artifact_path TEXT, ts INTEGER);",
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
