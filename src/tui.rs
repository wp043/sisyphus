use crate::theme::Theme;
use crate::{db, draft, mine, theme};
use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Padding, Paragraph, Wrap};
use rusqlite::Connection;
use std::collections::VecDeque;
use std::sync::mpsc;

const MAX_WORKERS: usize = 3;

enum State {
    Idle,
    Queued,
    Drafting,
    Drafted(draft::Draft),
    Failed(String),
    Done(String),
}

struct Item {
    cand: mine::Candidate,
    prompt: String,
    state: State,
}

fn kind_icon(kind: &str) -> &'static str {
    match kind {
        "sequence" => "⚡",
        "fixloop" => "🔁",
        "prompt" => "💬",
        _ => "•",
    }
}

pub fn run(conn: &Connection, limit: usize) -> Result<()> {
    let cands = mine::candidates(conn, limit)?;
    if cands.is_empty() {
        println!("no undecided patterns — the boulder rests 🪨");
        return Ok(());
    }
    let mut items = Vec::new();
    for c in cands {
        let prompt = draft::prepare_prompt(conn, &c.kind, &c.templates, c.count)?;
        items.push(Item { cand: c, prompt, state: State::Idle });
    }

    let theme = theme::load();
    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let result = event_loop(conn, &mut terminal, &mut items, &theme);
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result?;

    // exit summary so decisions aren't lost to the alternate screen
    for it in &items {
        if let State::Done(msg) = &it.state {
            println!("{} {}", kind_icon(&it.cand.kind), msg);
        }
    }
    Ok(())
}

fn event_loop(
    conn: &Connection,
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    items: &mut [Item],
    theme: &Theme,
) -> Result<()> {
    let (tx, rx) = mpsc::channel::<(usize, Result<draft::Draft>)>();
    let mut queue: VecDeque<usize> = VecDeque::new();
    let mut active = 0usize;
    let mut auto_accept = false;
    let mut selected = 0usize;

    loop {
        // collect finished drafts
        while let Ok((idx, out)) = rx.try_recv() {
            active -= 1;
            items[idx].state = match out {
                Ok(d) if auto_accept => match draft::install(&d) {
                    Ok(p) => {
                        db::decide(conn, items[idx].cand.id, "accepted", Some(p.display().to_string()))?;
                        State::Done(format!("✓ {} installed → {}", d.name, p.display()))
                    }
                    Err(e) => State::Failed(format!("install failed: {e:#}")),
                },
                Ok(d) => State::Drafted(d),
                Err(e) => State::Failed(format!("draft failed: {e:#}")),
            };
        }
        // feed idle workers
        while active < MAX_WORKERS {
            let Some(idx) = queue.pop_front() else { break };
            items[idx].state = State::Drafting;
            active += 1;
            let tx = tx.clone();
            let prompt = items[idx].prompt.clone();
            std::thread::spawn(move || {
                let _ = tx.send((idx, draft::run_claude(&prompt)));
            });
        }

        terminal.draw(|f| draw(f, items, selected, active, auto_accept, theme))?;

        if !event::poll(std::time::Duration::from_millis(100))? {
            continue;
        }
        let Event::Key(key) = event::read()? else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => break,
            KeyCode::Down | KeyCode::Char('j') => {
                selected = (selected + 1).min(items.len() - 1);
            }
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Char('d') => {
                if matches!(items[selected].state, State::Idle | State::Failed(_)) {
                    items[selected].state = State::Queued;
                    queue.push_back(selected);
                }
            }
            KeyCode::Char('a') => {
                let state = std::mem::replace(&mut items[selected].state, State::Idle);
                items[selected].state = if let State::Drafted(d) = state {
                    match draft::install(&d) {
                        Ok(p) => {
                            db::decide(conn, items[selected].cand.id, "accepted", Some(p.display().to_string()))?;
                            State::Done(format!("✓ {} installed → {}", d.name, p.display()))
                        }
                        Err(e) => State::Failed(format!("install failed: {e:#}")),
                    }
                } else {
                    state
                };
            }
            KeyCode::Char('i') => {
                if matches!(items[selected].state, State::Idle | State::Drafted(_) | State::Failed(_)) {
                    db::decide(conn, items[selected].cand.id, "ignored", None)?;
                    items[selected].state = State::Done("ignored".into());
                }
            }
            KeyCode::Char('A') => {
                auto_accept = true;
                for (idx, it) in items.iter_mut().enumerate() {
                    if matches!(it.state, State::Idle | State::Failed(_)) {
                        it.state = State::Queued;
                        queue.push_back(idx);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn state_span(state: &State, t: &Theme) -> Span<'static> {
    match state {
        State::Idle => Span::styled("○", Style::default().fg(t.dim)),
        State::Queued => Span::styled("…", Style::default().fg(t.warn)),
        State::Drafting => Span::styled("◐", Style::default().fg(t.warn)),
        State::Drafted(_) => Span::styled("●", Style::default().fg(t.accent)),
        State::Failed(_) => Span::styled("✗", Style::default().fg(t.err)),
        State::Done(_) => Span::styled("✓", Style::default().fg(t.ok)),
    }
}

fn draw(f: &mut Frame, items: &[Item], selected: usize, active: usize, auto_accept: bool, t: &Theme) {
    let [main, footer] =
        Layout::vertical([Constraint::Min(5), Constraint::Length(1)]).areas(f.area());
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)]).areas(main);

    let rows: Vec<ListItem> = items
        .iter()
        .map(|it| {
            let head = it.cand.templates[0].chars().take(30).collect::<String>();
            let line = Line::from(vec![
                state_span(&it.state, t),
                Span::styled(format!(" {} ", kind_icon(&it.cand.kind)), Style::default().fg(t.kind(&it.cand.kind))),
                Span::styled(head, Style::default().fg(t.text)),
                Span::styled(
                    format!("  {}×", it.cand.count),
                    Style::default().fg(t.dim),
                ),
            ]);
            ListItem::new(line)
        })
        .collect();
    let mut list_state = ListState::default();
    list_state.select(Some(selected));
    f.render_stateful_widget(
        List::new(rows)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(t.dim))
                    .title(Span::styled(" sisyphus — boulders ", Style::default().fg(t.accent).bold())),
            )
            .highlight_style(Style::default().bg(t.highlight_bg).bold()),
        left,
        &mut list_state,
    );

    let it = &items[selected];
    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled(
                format!("{} {} ", kind_icon(&it.cand.kind), it.cand.kind),
                Style::default().fg(t.kind(&it.cand.kind)).bold(),
            ),
            Span::styled(format!("seen {}× · score {:.0}", it.cand.count, it.cand.score), Style::default().fg(t.dim)),
        ]),
        Line::raw(""),
    ];
    for (i, tpl) in it.cand.templates.iter().enumerate() {
        let arrow = if i == 0 { "  " } else { "→ " };
        lines.push(Line::from(vec![
            Span::styled(arrow, Style::default().fg(t.dim)),
            Span::styled(tpl.clone(), Style::default().fg(t.text)),
        ]));
    }
    lines.push(Line::raw(""));
    match &it.state {
        State::Idle => lines.push(Line::styled(
            "press d to draft with claude",
            Style::default().fg(t.dim),
        )),
        State::Queued | State::Drafting => lines.push(Line::styled(
            "drafting via claude -p …",
            Style::default().fg(t.warn),
        )),
        State::Failed(e) => lines.push(Line::styled(
            e.clone(),
            Style::default().fg(t.err),
        )),
        State::Done(msg) => lines.push(Line::styled(
            msg.clone(),
            Style::default().fg(t.ok),
        )),
        State::Drafted(d) => {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{} ({})", d.name, d.kind),
                    Style::default().fg(t.ok).bold(),
                ),
                Span::styled(format!(" — {}", d.summary), Style::default().fg(t.text)),
            ]));
            lines.push(Line::styled(
                "─".repeat(right.width.saturating_sub(4) as usize),
                Style::default().fg(t.dim),
            ));
            for l in d.content.lines() {
                lines.push(Line::styled(l.to_string(), Style::default().fg(t.text)));
            }
        }
    }
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(t.dim))
                    .padding(Padding::horizontal(1)),
            ),
        right,
    );

    let status = format!(
        " j/k move · d draft · a accept · i ignore · A draft+accept ALL · q quit {}{}",
        if active > 0 { format!("· {active} drafting ") } else { String::new() },
        if auto_accept { "· AUTO " } else { "" }
    );
    f.render_widget(
        Paragraph::new(status).style(Style::default().fg(t.dim)),
        footer,
    );
}
