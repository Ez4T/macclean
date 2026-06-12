//! The enriched navigable TUI: a size-sorted list of classified Items with
//! colour-coded Safety Class badges and inline Evidence. Deletion happens only
//! through a deliberate Confirm modal (CONTEXT.md → "Confirm": no path from Scan
//! to deletion skips it), and `Unclassified` Items must be overridden first
//! (ADR-0001).

use crate::model::{Item, SafetyClass, Scan};
use crate::reclaim::{self, Reclaimed};
use crate::ruleset::Ruleset;
use crate::scan::human;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
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

/// Which screen the TUI is showing. Reclaim is a two-step: a reclaim keypress in
/// `Browse` opens the `Confirm` modal, and only a `y` there deletes anything —
/// there is no single-key path from Scan to deletion (CONTEXT.md → "Confirm").
#[derive(Clone, Copy)]
enum Mode {
    Browse,
    Confirm { index: usize },
}

struct AppState {
    scan: Scan,
    ruleset: Ruleset,
    list: ListState,
    mode: Mode,
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
        mode: Mode::Browse,
        status: "↑/↓ move · o override Unclassified · c/Enter Confirm reclaim · q quit".into(),
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
            if handle_key(state, key.code) {
                return Ok(());
            }
        }
    }
}

/// Apply one keypress to the state. Returns `true` when the app should quit.
/// Split out from the event loop so the Confirm gate can be unit-tested without
/// a live terminal.
fn handle_key(state: &mut AppState, code: KeyCode) -> bool {
    // In the Confirm modal, only `y` reclaims; anything else cancels untouched.
    if let Mode::Confirm { index } = state.mode {
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => do_reclaim(state, index),
            _ => state.status = "Reclaim cancelled — nothing deleted.".into(),
        }
        state.mode = Mode::Browse;
        return false;
    }

    match code {
        KeyCode::Char('q') | KeyCode::Esc => return true,
        KeyCode::Down | KeyCode::Char('j') => move_sel(state, 1),
        KeyCode::Up | KeyCode::Char('k') => move_sel(state, -1),
        KeyCode::Char('o') => toggle_override(state),
        KeyCode::Char('c') | KeyCode::Enter => request_confirm(state),
        _ => {}
    }
    false
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

/// First step of a Reclaim: if the highlighted Item may be reclaimed, open the
/// Confirm modal; otherwise explain why it can't and stay in Browse. Nothing is
/// deleted here.
fn request_confirm(state: &mut AppState) {
    let Some(i) = state.list.selected() else { return };
    let item = &state.scan.items[i];
    if !item.may_reclaim() {
        state.status = format!(
            "{} is {} — not reclaimable (press o to override).",
            item.path.display(),
            item.class.label()
        );
        return;
    }
    state.mode = Mode::Confirm { index: i };
}

/// Second step of a Reclaim: actually delete the Item at `i`. Only ever reached
/// from a `y` in the Confirm modal. Re-checks `may_reclaim` as the last guardrail.
fn do_reclaim(state: &mut AppState, i: usize) {
    let Some(item) = state.scan.items.get(i).cloned() else { return };
    if !item.may_reclaim() {
        state.status = format!(
            "{} is {} — not reclaimable.",
            item.path.display(),
            item.class.label()
        );
        return;
    }
    match reclaim::reclaim(&item, &state.ruleset) {
        Ok(done) => {
            let how = match done {
                Reclaimed::ToolClean { command } => format!("cleaned via `{command}`"),
                Reclaimed::Removed => "removed".into(),
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
    let [header, body, detail, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
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

    // Recovery Method for the highlighted Item, always visible (issue #4).
    if let Some(it) = state.list.selected().and_then(|i| state.scan.items.get(i)) {
        f.render_widget(
            Line::from(vec![
                Span::raw(" Recovery: ").bold(),
                Span::styled(it.recovery_line(), Style::default().fg(class_color(it.class))),
            ]),
            detail,
        );
    }

    f.render_widget(
        Paragraph::new(state.status.clone())
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL)),
        footer,
    );

    // The Confirm modal overlays everything while pending (CONTEXT.md → "Confirm").
    if let Mode::Confirm { index } = state.mode {
        if let Some(item) = state.scan.items.get(index) {
            draw_confirm(f, item);
        }
    }
}

/// The deliberate Confirm prompt: quotes the Item's path, size, class, and how it
/// will be recovered, then waits for a `y`. This is the only gate to deletion.
fn draw_confirm(f: &mut Frame, item: &Item) {
    let area = centered_rect(64, 9, f.area());
    f.render_widget(Clear, area);

    let mut class_label = item.class.label().to_string();
    if item.class == SafetyClass::Unclassified && item.override_reclaim {
        class_label.push_str(" (override)");
    }

    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::raw(item.path.display().to_string()).bold(),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(human(item.size_on_disk), Style::default().bold()),
            Span::raw("   "),
            Span::styled(
                class_label,
                Style::default()
                    .fg(class_color(item.class))
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::raw("  Recovery: "),
            Span::styled(item.recovery_line(), Style::default().fg(Color::Gray)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("[y]", Style::default().fg(Color::Green).bold()),
            Span::raw(" Reclaim    "),
            Span::styled("[N]", Style::default().fg(Color::Red).bold()),
            Span::raw(" cancel"),
        ]),
    ];

    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Confirm Reclaim ")
                .border_style(Style::default().fg(Color::Yellow)),
        ),
        area,
    );
}

/// A horizontally-centered rectangle `percent_x` wide and `height` tall, clamped
/// to `area`. Used to float the Confirm modal over the list. The width math is
/// done in `u32` so a wide terminal (`area.width * percent_x > u16::MAX`, i.e.
/// past ~1023 columns) can't overflow and panic on resize.
fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let width = (area.width as u32 * percent_x as u32 / 100).min(area.width as u32) as u16;
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect { x, y, width, height: height.min(area.height) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Evidence, RecoveryMethod};
    use std::fs;
    use std::path::PathBuf;

    fn state_with(items: Vec<Item>) -> AppState {
        let mut list = ListState::default();
        if !items.is_empty() {
            list.select(Some(0));
        }
        AppState {
            scan: Scan { root: PathBuf::from("/tmp"), items },
            ruleset: Ruleset::defaults(),
            list,
            mode: Mode::Browse,
            status: String::new(),
        }
    }

    fn node_modules_item(path: PathBuf) -> Item {
        Item {
            path,
            size_on_disk: 4096,
            class: SafetyClass::Reinstallable,
            recovery: RecoveryMethod::Reinstall { command: "npm install".into() },
            evidence: Evidence { summary: "node_modules".into() },
            override_reclaim: false,
        }
    }

    /// Acceptance for issue #4 / CONTEXT.md "Confirm": a single reclaim keypress
    /// must NOT delete — it only opens the Confirm modal. No path skips Confirm.
    #[test]
    fn reclaim_key_opens_confirm_and_deletes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let nm = tmp.path().join("node_modules");
        fs::create_dir(&nm).unwrap();
        fs::write(nm.join("index.js"), b"x").unwrap();

        let mut state = state_with(vec![node_modules_item(nm.clone())]);
        let quit = handle_key(&mut state, KeyCode::Char('c'));

        assert!(!quit);
        assert!(matches!(state.mode, Mode::Confirm { index: 0 }));
        assert!(nm.exists(), "nothing is deleted before the y confirm");
        assert_eq!(state.scan.items.len(), 1);
    }

    /// Cancelling the Confirm (any key that isn't `y`) returns to Browse and
    /// deletes nothing.
    #[test]
    fn confirm_cancel_leaves_item_intact() {
        let tmp = tempfile::tempdir().unwrap();
        let nm = tmp.path().join("node_modules");
        fs::create_dir(&nm).unwrap();

        let mut state = state_with(vec![node_modules_item(nm.clone())]);
        handle_key(&mut state, KeyCode::Char('c'));
        handle_key(&mut state, KeyCode::Char('n'));

        assert!(matches!(state.mode, Mode::Browse));
        assert!(nm.exists());
        assert_eq!(state.scan.items.len(), 1);
    }

    /// Only the deliberate `y` in the Confirm modal actually Reclaims.
    #[test]
    fn confirm_y_reclaims_the_item() {
        let tmp = tempfile::tempdir().unwrap();
        let nm = tmp.path().join("node_modules");
        fs::create_dir(&nm).unwrap();
        fs::write(nm.join("index.js"), b"x").unwrap();

        let mut state = state_with(vec![node_modules_item(nm.clone())]);
        handle_key(&mut state, KeyCode::Char('c'));
        handle_key(&mut state, KeyCode::Char('y'));

        assert!(matches!(state.mode, Mode::Browse));
        assert!(!nm.exists(), "the y confirm reclaims the Item");
        assert!(state.scan.items.is_empty());
    }

    /// A Protected (Irreplaceable) Item never opens the Confirm modal — the
    /// guardrail holds before deletion is ever offered.
    #[test]
    fn protected_item_never_reaches_confirm() {
        let item = Item {
            path: PathBuf::from("/tmp/orbstack.img.raw"),
            size_on_disk: 4096,
            class: SafetyClass::Irreplaceable,
            recovery: RecoveryMethod::None,
            evidence: Evidence { summary: "VM image".into() },
            override_reclaim: false,
        };
        let mut state = state_with(vec![item]);
        handle_key(&mut state, KeyCode::Char('c'));
        assert!(matches!(state.mode, Mode::Browse));
    }

    /// Empty-list edge case: navigation, override, and confirm keypresses must be
    /// no-ops that never panic and never select anything (issue #5).
    #[test]
    fn empty_list_keys_are_safe_noops() {
        let mut state = state_with(vec![]);
        assert_eq!(state.list.selected(), None);

        for code in [
            KeyCode::Down,
            KeyCode::Up,
            KeyCode::Char('o'),
            KeyCode::Char('c'),
            KeyCode::Enter,
        ] {
            assert!(!handle_key(&mut state, code));
            assert!(matches!(state.mode, Mode::Browse));
            assert_eq!(state.list.selected(), None, "nothing to select in an empty list");
        }
    }

    /// Last-item-removed edge case: reclaiming the only Item empties the list and
    /// clears the selection rather than leaving a dangling index (issue #5).
    #[test]
    fn reclaiming_last_item_clears_selection() {
        let tmp = tempfile::tempdir().unwrap();
        let nm = tmp.path().join("node_modules");
        fs::create_dir(&nm).unwrap();

        let mut state = state_with(vec![node_modules_item(nm)]);
        do_reclaim(&mut state, 0);

        assert!(state.scan.items.is_empty());
        assert_eq!(state.list.selected(), None);
    }

    /// Removing the highlighted last-of-many Item walks the selection back onto a
    /// still-valid index instead of pointing past the end (issue #5).
    #[test]
    fn reclaiming_tail_item_keeps_selection_in_range() {
        let tmp = tempfile::tempdir().unwrap();
        let items: Vec<Item> = (0..3)
            .map(|n| {
                let p = tmp.path().join(format!("nm{n}"));
                fs::create_dir(&p).unwrap();
                node_modules_item(p)
            })
            .collect();

        let mut state = state_with(items);
        state.list.select(Some(2)); // highlight the tail
        do_reclaim(&mut state, 2);

        assert_eq!(state.scan.items.len(), 2);
        let sel = state.list.selected().expect("selection survives");
        assert!(sel < state.scan.items.len(), "selection stays in range");
    }

    /// Navigation wraps both ways and stays in range (issue #5).
    #[test]
    fn move_sel_wraps_around() {
        let tmp = tempfile::tempdir().unwrap();
        let items: Vec<Item> = (0..3)
            .map(|n| node_modules_item(tmp.path().join(format!("nm{n}"))))
            .collect();
        let mut state = state_with(items);

        move_sel(&mut state, -1); // wrap from 0 back to the tail
        assert_eq!(state.list.selected(), Some(2));
        move_sel(&mut state, 1); // wrap from tail back to head
        assert_eq!(state.list.selected(), Some(0));
    }

    /// Resize edge case: the Confirm modal geometry must not overflow on a very
    /// wide terminal and must stay clamped inside the area (issue #5).
    #[test]
    fn centered_rect_survives_wide_terminal() {
        let area = Rect { x: 0, y: 0, width: 4000, height: 50 };
        let r = centered_rect(64, 9, area);
        assert!(r.width <= area.width);
        assert!(r.x + r.width <= area.x + area.width);
        assert!(r.y + r.height <= area.y + area.height);
    }

    /// A modal taller than the terminal is clamped to the available height instead
    /// of drawing off-screen (issue #5).
    #[test]
    fn centered_rect_clamps_to_short_terminal() {
        let area = Rect { x: 0, y: 0, width: 80, height: 4 };
        let r = centered_rect(64, 9, area);
        assert!(r.height <= area.height);
        assert!(r.y + r.height <= area.y + area.height);
    }

    /// The recovery line shown for the highlighted Item reflects both the Recovery
    /// Method and the Trash destination of an overridden Unclassified (ADR-0004).
    #[test]
    fn recovery_line_reflects_method_and_destination() {
        let reinstall = node_modules_item(PathBuf::from("/tmp/node_modules"));
        assert_eq!(reinstall.recovery_line(), "reinstall via `npm install`");

        let unknown = Item {
            path: PathBuf::from("/tmp/mystery"),
            size_on_disk: 4096,
            class: SafetyClass::Unclassified,
            recovery: RecoveryMethod::None,
            evidence: Evidence { summary: "unknown".into() },
            override_reclaim: true,
        };
        assert!(unknown.recovery_line().contains("Trash"));
    }
}
