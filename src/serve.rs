use crate::db;
use anyhow::{Context, Result};
use serde_json::json;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

const PAGE: &str = include_str!("../assets/dashboard.html");

pub fn run(port: u16, open: bool) -> Result<()> {
    let addr = format!("127.0.0.1:{port}");
    let listener = TcpListener::bind(&addr).with_context(|| format!("cannot bind {addr}"))?;
    println!("sisyphus dashboard → http://{addr}  (ctrl-c to stop)");
    if open {
        let _ = std::process::Command::new("open").arg(format!("http://{addr}")).status();
    }
    for stream in listener.incoming().flatten() {
        std::thread::spawn(move || {
            let _ = handle(stream);
        });
    }
    Ok(())
}

fn handle(mut stream: TcpStream) -> Result<()> {
    let mut buf = [0u8; 2048];
    let n = stream.read(&mut buf)?;
    let request_line = std::str::from_utf8(&buf[..n])
        .unwrap_or_default()
        .lines()
        .next()
        .unwrap_or_default();
    let path = request_line.split_whitespace().nth(1).unwrap_or("/");

    let (status, content_type, body) = match path {
        "/" => ("200 OK", "text/html; charset=utf-8", PAGE.to_string()),
        "/api/data" => match data() {
            Ok(json) => ("200 OK", "application/json", json),
            Err(e) => ("500 Internal Server Error", "text/plain", format!("{e:#}")),
        },
        _ => ("404 Not Found", "text/plain", "not found".into()),
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\r\n{body}",
        body.len()
    )?;
    Ok(())
}

fn data() -> Result<String> {
    // per-request connection: rusqlite connections aren't Sync and requests are rare
    let conn = db::open()?;
    let count = |sql: &str| -> Result<i64> { Ok(conn.query_row(sql, [], |r| r.get(0))?) };

    let sources: Vec<_> = {
        let mut stmt = conn.prepare(
            "SELECT source, COUNT(*) FROM commands GROUP BY source ORDER BY 2 DESC",
        )?;
        
        stmt
            .query_map([], |r| Ok(json!({"name": r.get::<_, String>(0)?, "n": r.get::<_, i64>(1)?})))?
            .collect::<std::result::Result<_, _>>()?
    };

    let daily: Vec<_> = {
        let mut stmt = conn.prepare(
            "SELECT date(ts, 'unixepoch') d, COUNT(*) FROM commands
             WHERE ts IS NOT NULL GROUP BY d ORDER BY d",
        )?;
        stmt.query_map([], |r| Ok(json!({"d": r.get::<_, String>(0)?, "n": r.get::<_, i64>(1)?})))?
            .collect::<std::result::Result<_, _>>()?
    };

    let top_templates: Vec<_> = {
        let mut stmt = conn.prepare(
            "SELECT template, COUNT(*) c FROM commands
             WHERE template IS NOT NULL GROUP BY template ORDER BY c DESC LIMIT 12",
        )?;
        stmt.query_map([], |r| Ok(json!({"tpl": r.get::<_, String>(0)?, "n": r.get::<_, i64>(1)?})))?
            .collect::<std::result::Result<_, _>>()?
    };

    let patterns: Vec<_> = {
        let mut stmt = conn.prepare(
            "SELECT p.id, p.kind, p.template_seq, p.count, p.score,
                    COALESCE(d.decision, 'undecided'), d.artifact_path
             FROM patterns p LEFT JOIN decisions d ON d.pattern_id = p.id
             ORDER BY p.score DESC LIMIT 50",
        )?;
        stmt.query_map([], |r| {
            let seq: String = r.get(2)?;
            let chain = serde_json::from_str::<Vec<String>>(&seq)
                .map(|v| v.join("  →  "))
                .unwrap_or(seq);
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "kind": r.get::<_, String>(1)?,
                "chain": chain,
                "count": r.get::<_, i64>(3)?,
                "score": r.get::<_, f64>(4)?,
                "decision": r.get::<_, String>(5)?,
                "artifact": r.get::<_, Option<String>>(6)?,
            }))
        })?
        .collect::<std::result::Result<_, _>>()?
    };

    let gain: Vec<_> = {
        let mut stmt = conn.prepare(
            "SELECT d.artifact_path, p.template_seq FROM decisions d
             JOIN patterns p ON p.id = d.pattern_id
             WHERE d.decision = 'accepted' AND d.artifact_path IS NOT NULL",
        )?;
        let rows: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        let mut out = Vec::new();
        for (path, seq) in rows {
            let name = std::path::Path::new(&path)
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let steps = serde_json::from_str::<Vec<String>>(&seq).map(|v| v.len()).unwrap_or(1) as i64;
            let uses: i64 =
                conn.query_row("SELECT COUNT(*) FROM commands WHERE head = ?1", [&name], |r| r.get(0))?;
            out.push(json!({"name": name, "uses": uses, "steps": steps, "saved": uses * (steps - 1).max(0)}));
        }
        out
    };

    let body = json!({
        "totals": {
            "commands": count("SELECT COUNT(*) FROM commands WHERE source NOT LIKE '%_prompt'")?,
            "prompts": count("SELECT COUNT(*) FROM commands WHERE source LIKE '%_prompt'")?,
            "patterns": count("SELECT COUNT(*) FROM patterns")?,
            "accepted": count("SELECT COUNT(*) FROM decisions WHERE decision = 'accepted'")?,
            "steps_saved": gain.iter().map(|g| g["saved"].as_i64().unwrap_or(0)).sum::<i64>(),
        },
        "sources": sources,
        "daily": daily,
        "top_templates": top_templates,
        "patterns": patterns,
        "gain": gain,
    });
    Ok(body.to_string())
}
