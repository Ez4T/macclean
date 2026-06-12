//! The enriched navigable TUI: a size-sorted list of classified Items with
//! colour-coded Safety Class badges and inline Evidence. Deletion happens only
//! on an explicit Confirm key, and `Unclassified` Items must be overridden
//! first (ADR-0001).

use crate::model::{SafetyClass, Scan};
use crate::reclaim::{self, Reclaimed};
use crate::ruleset::Ruleset;
use crate::scan::human;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

fn class_color(class: SafetyClass) -> Color {
    match class {
        SafetyClass::Regenerable => Color::Green,
        SafetyClass::Reinstallable => Color::Cyan,
        SafetyClass::Cache => Color::Blue,
        SafetyClass::RedundantCopy => Color::Magenta,
        SafetyClass::Irreplaceable => Color::Red,
        SafetyClass::Unclassified => Color::Yellow,
    }
}

struct AppState {
    scan: Scan,
    ruleset: Ruleset,
    list: ListState,
    status: String,
}

pub fn run(scan: Scan, ruleset: Ruleset) -> Result<()> {
    let mut terminal = ratatui::init();
    let mut state = AppState {
        list: {
            let mut s = ListState::default();
            if !scan.items.is_empty() {
                s.select(Some(0));
            }
            s
        },
        scan,
        ruleset,
        status: "↑/↓ move · o override Unclassified · c Confirm reclaim · q quit".into(),
    };

    let result = event_loop(&mut terminal, &mut state);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, state: &mut AppState) -> Result<()> {
    loop {
        terminal.draw(|f| draw(f, state))?;

        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Down | KeyCode::Char('j') => move_sel(state, 1),
                KeyCode::Up | KeyCode::Char('k') => move_sel(state, -1),
                KeyCode::Char('o') => toggle_override(state),
                KeyCode::Char('c') | KeyCode::Enter => confirm_reclaim(state),
                _ => {}
            }
        }
    }
}

fn move_sel(state: &mut AppState, delta: i32) {
    let len = state.scan.items.len();
    if len == 0 {
        return;
    }
    let cur = state.list.selected().unwrap_or(0) as i32;
    let next = (cur + delta).rem_euclid(len as i32) as usize;
    state.list.select(Some(next));
}

fn toggle_override(state: &mut AppState) {
    let Some(i) = state.list.selected() else { return };
    let item = &mut state.scan.items[i];
    if item.class == SafetyClass::Unclassified {
        item.override_reclaim = !item.override_reclaim;
        state.status = if item.override_reclaim {
            format!("Overridden → will Trash on Confirm: {}", item.path.display())
        } else {
            "Override cleared.".into()
        };
    } else {
        state.status = format!("{} cannot be overridden.", item.class.label());
    }
}

fn confirm_reclaim(state: &mut AppState) {
    let Some(i) = state.list.selected() else { return };
    let item = state.scan.items[i].clone();
    if !item.may_reclaim() {
        state.status = format!(
            "{} is {} — not reclaimable (press o to override).",
            item.path.display(),
            item.class.label()
        );
        return;
    }
    match reclaim::reclaim(&item, &state.ruleset) {
        Ok(done) => {
            let how = match done {
                Reclaimed::ToolClean { command } => format!("cleaned via `{command}`"),
                Reclaimed::RemovedDir => "removed".into(),
                Reclaimed::Trashed => "moved to Trash".into(),
            };
            state.status = format!("Reclaimed {} ({how}).", human(item.size_on_disk));
            state.scan.items.remove(i);
            if i >= state.scan.items.len() {
                state.list.select(state.scan.items.len().checked_sub(1));
            }
        }
        Err(e) => state.status = format!("Failed: {e}"),
    }
}

fn draw(f: &mut Frame, state: &AppState) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(3),
    ])
    .areas(f.area());

    let total = human(state.scan.reclaimable_bytes());
    f.render_widget(
        Line::from(vec![
            Span::raw(format!(" {}  ", state.scan.root.display())).bold(),
            Span::styled(
                format!("· reclaimable now: {total}"),
                Style::default().fg(Color::Green),
            ),
        ]),
        header,
    );

    let rows: Vec<ListItem> = state
        .scan
        .items
        .iter()
        .map(|it| {
            let mut badge = it.class.label().to_string();
            if it.class == SafetyClass::Unclassified && it.override_reclaim {
                badge.push_str(" (override)");
            }
            ListItem::new(Line::from(vec![
                Span::styled(format!("{:>8}  ", human(it.size_on_disk)), Style::default().bold()),
                Span::styled(
                    format!("{:<22}", badge),
                    Style::default()
                        .fg(class_color(it.class))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(it.path.display().to_string()),
                Span::styled(
                    format!("   — {}", it.evidence.summary),
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
        })
        .collect();

    let mut list_state = state.list.clone();
    f.render_stateful_widget(
        List::new(rows)
            .block(Block::default().borders(Borders::ALL).title(" Items "))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("▶ "),
        body,
        &mut list_state,
    );

    f.render_widget(
        Paragraph::new(state.status.clone())
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL)),
        footer,
    );
}
