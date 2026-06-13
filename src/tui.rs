//! The enriched navigable TUI: a Safety-Class-first grouped view with drill-down
//! into size-sorted classified Items. Deletion happens only through a deliberate
//! Confirm modal (CONTEXT.md → "Confirm": no path from Scan to deletion skips
//! it), and `Unclassified` Items must be overridden first (ADR-0001).

use crate::model::{Item, SafetyClass, Scan};
use crate::reclaim::{self, Reclaimed};
use crate::ruleset::Ruleset;
use crate::scan::human;
use crate::{classify, dedup};
use anyhow::{anyhow, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{
    mpsc::{self, TryRecvError},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};

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

const CLASS_ORDER: [SafetyClass; 6] = [
    SafetyClass::Regenerable,
    SafetyClass::Reinstallable,
    SafetyClass::Cache,
    SafetyClass::RedundantCopy,
    SafetyClass::Unclassified,
    SafetyClass::Irreplaceable,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    Groups,
    Items { class: SafetyClass },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfirmTarget {
    Item { index: usize },
    Group { class: SafetyClass },
}

/// Which screen the TUI is showing. Reclaim is a two-step: a reclaim keypress in
/// `Browse` opens the `Confirm` modal, and only a `y` there deletes anything —
/// there is no single-key path from Scan to deletion (CONTEXT.md → "Confirm").
#[derive(Clone, Copy)]
enum Mode {
    Browse,
    Confirm { target: ConfirmTarget },
    Reclaiming,
}

struct ReclaimJob {
    rx: mpsc::Receiver<ReclaimMessage>,
    stop_after_current: Arc<AtomicBool>,
    progress: ReclaimProgress,
    successful_indices: Vec<usize>,
    last_success: Option<String>,
}

struct ReclaimProgress {
    title: String,
    target_label: String,
    total: usize,
    current_ordinal: usize,
    completed: usize,
    reclaimed_count: usize,
    reclaimed_bytes: u64,
    failed_count: usize,
    first_error: Option<String>,
    current_path: Option<PathBuf>,
    current_action: String,
    stop_requested: bool,
    stopped: bool,
}

enum ReclaimMessage {
    ItemStarted {
        ordinal: usize,
        path: PathBuf,
        action: String,
    },
    ItemFinished {
        index: usize,
        bytes: u64,
        result: std::result::Result<String, String>,
    },
    Finished {
        stopped: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GroupSummary {
    class: SafetyClass,
    count: usize,
    total_bytes: u64,
    reclaimable_count: usize,
    reclaimable_bytes: u64,
}

struct AppState {
    scan: Scan,
    ruleset: Ruleset,
    view: View,
    group_list: ListState,
    item_list: ListState,
    mode: Mode,
    reclaim_job: Option<ReclaimJob>,
    status: String,
    /// Whether the proportional on-disk-usage overview pane is shown to the left
    /// of the action list (issue #7). Off by default; toggled with `t`.
    show_overview: bool,
}

impl AppState {
    fn new(scan: Scan, ruleset: Ruleset) -> Self {
        let mut group_list = ListState::default();
        select_default_group(&scan, &mut group_list);
        Self {
            scan,
            ruleset,
            view: View::Groups,
            group_list,
            item_list: ListState::default(),
            mode: Mode::Browse,
            reclaim_job: None,
            status: "↑/↓ move · Enter open group · c Confirm group · t overview · q quit".into(),
            show_overview: false,
        }
    }
}

fn group_summaries(scan: &Scan) -> Vec<GroupSummary> {
    CLASS_ORDER
        .iter()
        .filter_map(|&class| {
            let mut summary = GroupSummary {
                class,
                count: 0,
                total_bytes: 0,
                reclaimable_count: 0,
                reclaimable_bytes: 0,
            };

            for item in scan.items.iter().filter(|item| item.class == class) {
                summary.count += 1;
                summary.total_bytes += item.size_on_disk;
                if item.may_reclaim() {
                    summary.reclaimable_count += 1;
                    summary.reclaimable_bytes += item.size_on_disk;
                }
            }

            (summary.count > 0).then_some(summary)
        })
        .collect()
}

fn select_default_group(scan: &Scan, list: &mut ListState) {
    let groups = group_summaries(scan);
    let selected = groups
        .iter()
        .enumerate()
        .filter(|(_, group)| group.reclaimable_bytes > 0)
        .max_by_key(|(_, group)| group.reclaimable_bytes)
        .map(|(i, _)| i)
        .or_else(|| (!groups.is_empty()).then_some(0));
    list.select(selected);
}

fn selected_group_summary(state: &AppState) -> Option<GroupSummary> {
    let groups = group_summaries(&state.scan);
    state
        .group_list
        .selected()
        .and_then(|i| groups.get(i).copied())
}

fn item_indices_for_class(scan: &Scan, class: SafetyClass) -> Vec<usize> {
    scan.items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| (item.class == class).then_some(i))
        .collect()
}

fn selected_item_index(state: &AppState) -> Option<usize> {
    let View::Items { class } = state.view else {
        return None;
    };
    let indices = item_indices_for_class(&state.scan, class);
    state
        .item_list
        .selected()
        .and_then(|i| indices.get(i).copied())
}

fn clamp_list_selection(list: &mut ListState, len: usize) {
    match (len, list.selected()) {
        (0, _) => list.select(None),
        (_, None) => list.select(Some(0)),
        (_, Some(i)) if i >= len => list.select(Some(len - 1)),
        _ => {}
    }
}

fn sync_selection_after_items_changed(state: &mut AppState) {
    if let View::Items { class } = state.view {
        let item_count = item_indices_for_class(&state.scan, class).len();
        clamp_list_selection(&mut state.item_list, item_count);
        if item_count == 0 {
            state.view = View::Groups;
            state.item_list.select(None);
        }
    }

    let group_count = group_summaries(&state.scan).len();
    clamp_list_selection(&mut state.group_list, group_count);
}

fn item_word(count: usize) -> &'static str {
    if count == 1 {
        "Item"
    } else {
        "Items"
    }
}

fn class_recovery_hint(class: SafetyClass) -> &'static str {
    match class {
        SafetyClass::Regenerable => "Recovery: rebuild after Reclaim",
        SafetyClass::Reinstallable => "Recovery: reinstall dependencies after Reclaim",
        SafetyClass::Cache => "Recovery: cache refills automatically",
        SafetyClass::RedundantCopy => "Recovery: the surviving copy remains",
        SafetyClass::Unclassified => "Open group and override individual Items before Reclaim",
        SafetyClass::Irreplaceable => "Protected: not offered for Reclaim",
    }
}

pub fn run_scanning(root: PathBuf, ruleset: Ruleset, min_unclassified: u64) -> Result<()> {
    let mut terminal = ratatui::init();
    let (tx, rx) = mpsc::channel();
    let scan_root = root.clone();
    let scan_ruleset = ruleset.clone();

    thread::spawn(move || {
        let mut scan = classify::run(&scan_root, &scan_ruleset, min_unclassified);
        dedup::analyze(&mut scan);
        let _ = tx.send(scan);
    });

    let result = loading_loop(&mut terminal, root, ruleset, rx);
    ratatui::restore();
    result
}

fn loading_loop(
    terminal: &mut ratatui::DefaultTerminal,
    root: PathBuf,
    ruleset: Ruleset,
    rx: mpsc::Receiver<Scan>,
) -> Result<()> {
    let started = Instant::now();
    let mut tick = 0usize;

    loop {
        match rx.try_recv() {
            Ok(scan) => {
                let mut state = AppState::new(scan, ruleset);
                return event_loop(terminal, &mut state);
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return Err(anyhow!("scan worker stopped")),
        }

        terminal.draw(|f| draw_loading(f, &root, started.elapsed(), tick))?;
        tick = tick.wrapping_add(1);

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press
                    && matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                {
                    return Ok(());
                }
            }
        }
    }
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, state: &mut AppState) -> Result<()> {
    let mut tick = 0usize;
    loop {
        drain_reclaim_messages(state);
        terminal.draw(|f| draw(f, state, tick))?;
        tick = tick.wrapping_add(1);

        if matches!(state.mode, Mode::Reclaiming) {
            if event::poll(Duration::from_millis(100))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        handle_key(state, key.code);
                    }
                }
            }
            continue;
        }

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
    if matches!(state.mode, Mode::Reclaiming) {
        match code {
            KeyCode::Char('s') | KeyCode::Char('S') | KeyCode::Esc => {
                request_stop_after_current(state)
            }
            _ => {}
        }
        return false;
    }

    // In the Confirm modal, only `y` reclaims; anything else cancels untouched.
    if let Mode::Confirm { target } = state.mode {
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => start_reclaim_job(state, target),
            _ => state.status = "Reclaim cancelled. Nothing deleted.".into(),
        }
        if !matches!(state.mode, Mode::Reclaiming) {
            state.mode = Mode::Browse;
        }
        return false;
    }

    match code {
        KeyCode::Char('q') => return true,
        KeyCode::Esc => {
            if state.view == View::Groups {
                return true;
            }
            back_to_groups(state);
        }
        KeyCode::Down | KeyCode::Char('j') => move_sel(state, 1),
        KeyCode::Up | KeyCode::Char('k') => move_sel(state, -1),
        KeyCode::Left | KeyCode::Backspace | KeyCode::Char('b') => back_to_groups(state),
        KeyCode::Right => open_selected_group(state),
        KeyCode::Char('o') => toggle_override(state),
        KeyCode::Char('t') => toggle_overview(state),
        KeyCode::Char('c') => request_confirm(state),
        KeyCode::Enter => match state.view {
            View::Groups => open_selected_group(state),
            View::Items { .. } => request_confirm(state),
        },
        _ => {}
    }
    false
}

fn draw_loading(f: &mut Frame, root: &Path, elapsed: Duration, tick: usize) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(3),
    ])
    .areas(f.area());
    let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let spinner = frames[tick % frames.len()];

    f.render_widget(
        Line::from(vec![
            Span::raw(format!(" {}  ", root.display())).bold(),
            Span::styled("· scanning filesystem", Style::default().fg(Color::Yellow)),
        ]),
        header,
    );

    let seconds = elapsed.as_secs();
    let panel = Paragraph::new(vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(spinner, Style::default().fg(Color::Yellow).bold()),
            Span::raw(format!(" Scan running for {seconds}s")),
        ]),
        Line::from(""),
        Line::from("  Large dependency trees and VM/data directories can take a while to size."),
    ])
    .wrap(Wrap { trim: true })
    .block(Block::default().borders(Borders::ALL).title(" Scan "));
    f.render_widget(panel, body);

    f.render_widget(
        Paragraph::new(" q/Esc quit ")
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL)),
        footer,
    );
}

fn move_sel(state: &mut AppState, delta: i32) {
    let len = match state.view {
        View::Groups => group_summaries(&state.scan).len(),
        View::Items { class } => item_indices_for_class(&state.scan, class).len(),
    };
    if len == 0 {
        return;
    }
    let list = match state.view {
        View::Groups => &mut state.group_list,
        View::Items { .. } => &mut state.item_list,
    };
    let cur = list.selected().unwrap_or(0) as i32;
    let next = (cur + delta).rem_euclid(len as i32) as usize;
    list.select(Some(next));
}

fn open_selected_group(state: &mut AppState) {
    if state.view != View::Groups {
        return;
    }
    let Some(group) = selected_group_summary(state) else {
        return;
    };
    state.view = View::Items { class: group.class };
    state.item_list.select(Some(0));
    state.status = format!(
        "{} group open · {} {} · {}",
        group.class.label(),
        group.count,
        item_word(group.count),
        class_recovery_hint(group.class)
    );
}

fn back_to_groups(state: &mut AppState) {
    if state.view == View::Groups {
        return;
    }
    state.view = View::Groups;
    state.item_list.select(None);
    state.status = "Group view · Enter open group · c Confirm group · q quit.".into();
}

fn toggle_override(state: &mut AppState) {
    let Some(i) = selected_item_index(state) else {
        state.status = "Open Unclassified to override individual Items.".into();
        return;
    };
    let item = &mut state.scan.items[i];
    if item.class == SafetyClass::Unclassified {
        item.override_reclaim = !item.override_reclaim;
        state.status = if item.override_reclaim {
            format!(
                "Overridden → will Trash on Confirm: {}",
                item.path.display()
            )
        } else {
            "Override cleared.".into()
        };
        sync_selection_after_items_changed(state);
    } else {
        state.status = format!("{} cannot be overridden.", item.class.label());
    }
}

fn toggle_overview(state: &mut AppState) {
    state.show_overview = !state.show_overview;
    state.status = if state.show_overview {
        "Overview pane on · bars scaled by on-disk size (t to hide).".into()
    } else {
        "Overview pane off.".into()
    };
}

/// First step of a Reclaim: if the highlighted group or Item may be reclaimed,
/// open the Confirm modal; otherwise explain why it can't and stay in Browse.
/// Nothing is deleted here.
fn request_confirm(state: &mut AppState) {
    match state.view {
        View::Groups => request_group_confirm(state),
        View::Items { .. } => request_item_confirm(state),
    }
}

fn request_group_confirm(state: &mut AppState) {
    let Some(group) = selected_group_summary(state) else {
        return;
    };
    if group.reclaimable_count == 0 {
        state.status = match group.class {
            SafetyClass::Unclassified => {
                "Unclassified is not offered. Open the group and press o on Items to override."
                    .into()
            }
            SafetyClass::Irreplaceable => "Irreplaceable is Protected. Not reclaimable.".into(),
            _ => format!("{} group has nothing reclaimable.", group.class.label()),
        };
        return;
    }
    state.mode = Mode::Confirm {
        target: ConfirmTarget::Group { class: group.class },
    };
}

fn request_item_confirm(state: &mut AppState) {
    let Some(i) = selected_item_index(state) else {
        return;
    };
    let item = &state.scan.items[i];
    if !item.may_reclaim() {
        state.status = if item.class == SafetyClass::Unclassified {
            format!(
                "{} is Unclassified. Press o to override before Reclaim.",
                item.path.display()
            )
        } else {
            format!(
                "{} is {}. Not reclaimable.",
                item.path.display(),
                item.class.label()
            )
        };
        return;
    }
    state.mode = Mode::Confirm {
        target: ConfirmTarget::Item { index: i },
    };
}

fn start_reclaim_job(state: &mut AppState, target: ConfirmTarget) {
    let (items, title, target_label) = match target {
        ConfirmTarget::Item { index } => {
            let Some(item) = state.scan.items.get(index).cloned() else {
                state.status = "Item is no longer available.".into();
                return;
            };
            if !item.may_reclaim() {
                state.status = format!(
                    "{} is {}. Not reclaimable.",
                    item.path.display(),
                    item.class.label()
                );
                return;
            }
            (vec![(index, item)], "Reclaiming Item".into(), "Item".into())
        }
        ConfirmTarget::Group { class } => {
            let items: Vec<(usize, Item)> = state
                .scan
                .items
                .iter()
                .cloned()
                .enumerate()
                .filter(|(_, item)| item.class == class && item.may_reclaim())
                .collect();

            if items.is_empty() {
                state.status = format!("{} group has nothing reclaimable.", class.label());
                return;
            }

            (
                items,
                format!("Reclaiming {} group", class.label()),
                format!("{} Items", class.label()),
            )
        }
    };

    let total = items.len();
    let ruleset = state.ruleset.clone();
    let (tx, rx) = mpsc::channel();
    let stop_after_current = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop_after_current);

    thread::spawn(move || run_reclaim_worker(items, ruleset, worker_stop, tx));

    state.reclaim_job = Some(ReclaimJob {
        rx,
        stop_after_current,
        progress: ReclaimProgress {
            title,
            target_label,
            total,
            current_ordinal: 0,
            completed: 0,
            reclaimed_count: 0,
            reclaimed_bytes: 0,
            failed_count: 0,
            first_error: None,
            current_path: None,
            current_action: "Preparing Reclaim".into(),
            stop_requested: false,
            stopped: false,
        },
        successful_indices: Vec::new(),
        last_success: None,
    });
    state.mode = Mode::Reclaiming;
    state.status = "Reclaim running. Only stop-after-current is available.".into();
}

fn run_reclaim_worker(
    items: Vec<(usize, Item)>,
    ruleset: Ruleset,
    stop_after_current: Arc<AtomicBool>,
    tx: mpsc::Sender<ReclaimMessage>,
) {
    let mut stopped = false;

    for (pos, (index, item)) in items.into_iter().enumerate() {
        if pos > 0 && stop_after_current.load(Ordering::Relaxed) {
            stopped = true;
            break;
        }

        let action = reclaim::planned_action(&item, &ruleset);
        if tx
            .send(ReclaimMessage::ItemStarted {
                ordinal: pos + 1,
                path: item.path.clone(),
                action,
            })
            .is_err()
        {
            return;
        }

        let bytes = item.size_on_disk;
        let result = reclaim::reclaim(&item, &ruleset)
            .map(reclaimed_label)
            .map_err(|e| format!("{}: {e}", item.path.display()));

        if tx
            .send(ReclaimMessage::ItemFinished {
                index,
                bytes,
                result,
            })
            .is_err()
        {
            return;
        }
    }

    let _ = tx.send(ReclaimMessage::Finished { stopped });
}

fn reclaimed_label(done: Reclaimed) -> String {
    match done {
        Reclaimed::ToolClean { command } => format!("cleaned via `{command}`"),
        Reclaimed::Removed => "removed".into(),
        Reclaimed::Trashed => "moved to Trash".into(),
    }
}

fn request_stop_after_current(state: &mut AppState) {
    let Some(job) = state.reclaim_job.as_mut() else {
        return;
    };

    if job.progress.total <= 1 {
        state.status = "Current Item is already running; wait for completion.".into();
        return;
    }

    job.stop_after_current.store(true, Ordering::Relaxed);
    job.progress.stop_requested = true;
    state.status =
        "Stop requested. Current Item will finish; remaining Items will not start.".into();
}

fn drain_reclaim_messages(state: &mut AppState) {
    let mut finished = false;

    if let Some(job) = state.reclaim_job.as_mut() {
        loop {
            match job.rx.try_recv() {
                Ok(ReclaimMessage::ItemStarted {
                    ordinal,
                    path,
                    action,
                }) => {
                    job.progress.current_ordinal = ordinal;
                    job.progress.current_path = Some(path);
                    job.progress.current_action = action;
                }
                Ok(ReclaimMessage::ItemFinished {
                    index,
                    bytes,
                    result,
                }) => {
                    job.progress.completed += 1;
                    match result {
                        Ok(how) => {
                            job.progress.reclaimed_count += 1;
                            job.progress.reclaimed_bytes += bytes;
                            job.successful_indices.push(index);
                            job.last_success = Some(how);
                        }
                        Err(error) => {
                            job.progress.failed_count += 1;
                            job.progress.first_error.get_or_insert(error);
                        }
                    }
                }
                Ok(ReclaimMessage::Finished { stopped }) => {
                    job.progress.stopped = stopped;
                    job.progress.current_path = None;
                    job.progress.current_action = "Done".into();
                    finished = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    job.progress
                        .first_error
                        .get_or_insert_with(|| "reclaim worker stopped".into());
                    job.progress.failed_count += 1;
                    finished = true;
                    break;
                }
            }
        }
    }

    if finished {
        finish_reclaim_job(state);
    }
}

fn finish_reclaim_job(state: &mut AppState) {
    let Some(mut job) = state.reclaim_job.take() else {
        return;
    };

    job.successful_indices.sort_unstable();
    job.successful_indices.dedup();
    for i in job.successful_indices.iter().rev() {
        if *i < state.scan.items.len() {
            state.scan.items.remove(*i);
        }
    }

    state.status = reclaim_finished_status(&job.progress, job.last_success.as_deref());
    state.mode = Mode::Browse;
    sync_selection_after_items_changed(state);
}

fn reclaim_finished_status(progress: &ReclaimProgress, last_success: Option<&str>) -> String {
    let reclaimed = human(progress.reclaimed_bytes);

    if progress.stopped {
        let untouched = progress.total.saturating_sub(progress.completed);
        let mut status = format!(
            "Stopped after current Item. Reclaimed {}/{} {}, {}. {} not touched.",
            progress.reclaimed_count, progress.total, progress.target_label, reclaimed, untouched
        );
        if progress.failed_count > 0 {
            if let Some(error) = &progress.first_error {
                status.push_str(&format!(" First failure: {error}"));
            }
        }
        return status;
    }

    if progress.failed_count > 0 {
        let error = progress
            .first_error
            .as_deref()
            .unwrap_or("unknown reclaim failure");
        return format!(
            "Reclaimed {}/{} {}, {}. First failure: {}",
            progress.reclaimed_count, progress.total, progress.target_label, reclaimed, error
        );
    }

    if progress.total == 1 {
        let how = last_success.unwrap_or("completed");
        return format!("Reclaimed {reclaimed} ({how}).");
    }

    format!(
        "Reclaimed {} {}, {}.",
        progress.reclaimed_count, progress.target_label, reclaimed
    )
}

/// Direct single-Item Reclaim path used by selection edge-case tests. The live
/// TUI starts a worker with [`start_reclaim_job`] after Confirm.
#[cfg(test)]
fn do_reclaim_item(state: &mut AppState, i: usize) {
    let Some(item) = state.scan.items.get(i).cloned() else {
        return;
    };
    if !item.may_reclaim() {
        state.status = format!(
            "{} is {}. Not reclaimable.",
            item.path.display(),
            item.class.label()
        );
        return;
    }
    match reclaim::reclaim(&item, &state.ruleset) {
        Ok(done) => {
            let how = reclaimed_label(done);
            state.status = format!("Reclaimed {} ({how}).", human(item.size_on_disk));
            state.scan.items.remove(i);
            sync_selection_after_items_changed(state);
        }
        Err(e) => state.status = format!("Failed: {e}"),
    }
}

fn draw(f: &mut Frame, state: &AppState, tick: usize) {
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

    // Optional overview pane (issue #7): split the body so a proportional
    // on-disk-usage bar chart sits to the left of the action list. The list keeps
    // the remaining width, so the action surface is never crowded out.
    let list_area = if state.show_overview {
        let [overview, list] =
            Layout::horizontal([Constraint::Percentage(34), Constraint::Min(24)]).areas(body);
        draw_overview(f, state, overview);
        list
    } else {
        body
    };

    match state.view {
        View::Groups => draw_group_list(f, state, list_area),
        View::Items { class } => draw_item_list(f, state, class, list_area),
    }

    draw_detail(f, state, detail);

    f.render_widget(
        Paragraph::new(state.status.clone())
            .wrap(Wrap { trim: true })
            .block(Block::default().borders(Borders::ALL)),
        footer,
    );

    // The Confirm modal overlays everything while pending (CONTEXT.md → "Confirm").
    if let Mode::Confirm { target } = state.mode {
        draw_confirm(f, state, target);
    }

    if matches!(state.mode, Mode::Reclaiming) {
        draw_reclaiming(f, state, tick);
    }
}

fn draw_group_list(f: &mut Frame, state: &AppState, area: Rect) {
    let rows: Vec<ListItem> = group_summaries(&state.scan)
        .into_iter()
        .map(|group| {
            let action = if group.reclaimable_count > 0 {
                Span::styled(
                    format!("{:>8} reclaimable", human(group.reclaimable_bytes)),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )
            } else if group.class == SafetyClass::Irreplaceable {
                Span::styled(
                    "Protected".to_string(),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )
            } else if group.class == SafetyClass::Unclassified {
                Span::styled(
                    "needs override".to_string(),
                    Style::default().fg(Color::Yellow),
                )
            } else {
                Span::styled(
                    "nothing reclaimable".to_string(),
                    Style::default().fg(Color::Gray),
                )
            };

            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:<20}", group.class.label()),
                    Style::default()
                        .fg(class_color(group.class))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{:>8} total  ", human(group.total_bytes)),
                    Style::default().bold(),
                ),
                Span::raw(format!("{:>3} {}  ", group.count, item_word(group.count))),
                action,
            ]))
        })
        .collect();

    let mut list_state = state.group_list.clone();
    f.render_stateful_widget(
        List::new(rows)
            .block(Block::default().borders(Borders::ALL).title(" Groups "))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("▶ "),
        area,
        &mut list_state,
    );
}

fn draw_item_list(f: &mut Frame, state: &AppState, class: SafetyClass, area: Rect) {
    let rows: Vec<ListItem> = item_indices_for_class(&state.scan, class)
        .into_iter()
        .filter_map(|i| state.scan.items.get(i))
        .map(|it| {
            let badge = if it.class == SafetyClass::Unclassified && it.override_reclaim {
                "override".to_string()
            } else {
                it.class.label().to_string()
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:>8}  ", human(it.size_on_disk)),
                    Style::default().bold(),
                ),
                Span::styled(
                    format!("{:<16}", badge),
                    Style::default()
                        .fg(class_color(it.class))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(it.path.display().to_string()),
                Span::styled(
                    format!("   · {}", it.evidence.summary),
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
        })
        .collect();

    let mut list_state = state.item_list.clone();
    f.render_stateful_widget(
        List::new(rows)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} Items ", class.label())),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("▶ "),
        area,
        &mut list_state,
    );
}

fn draw_detail(f: &mut Frame, state: &AppState, area: Rect) {
    let line = match state.view {
        View::Groups => selected_group_summary(state)
            .map(|group| {
                Line::from(vec![
                    Span::raw(" Group: ").bold(),
                    Span::styled(
                        group.class.label(),
                        Style::default().fg(class_color(group.class)).bold(),
                    ),
                    Span::raw(format!(
                        " · {} {} · {} reclaimable · {}",
                        group.count,
                        item_word(group.count),
                        human(group.reclaimable_bytes),
                        class_recovery_hint(group.class)
                    )),
                ])
            })
            .unwrap_or_else(|| Line::from(" No Items surfaced.")),
        View::Items { .. } => selected_item_index(state)
            .and_then(|i| state.scan.items.get(i))
            .map(|it| {
                Line::from(vec![
                    Span::raw(" Recovery: ").bold(),
                    Span::styled(
                        it.recovery_line(),
                        Style::default().fg(class_color(it.class)),
                    ),
                ])
            })
            .unwrap_or_else(|| Line::from(" No Items in this group.")),
    };

    f.render_widget(line, area);
}

/// The overview pane (issue #7): one proportional bar per Item, scaled so the
/// largest on-disk Item fills the pane and the rest are relative to it, coloured
/// by Safety Class. A 1-D "block treemap" — the spike found a true 2-D squarified
/// treemap illegible in a narrow terminal column, so this sorted-bar form is what
/// stays readable. The highlighted Item is marked to tie the two panes together.
fn draw_overview(f: &mut Frame, state: &AppState, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(match state.view {
            View::Groups => " Overview · group size ",
            View::Items { .. } => " Overview · item size ",
        });
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Reserve space for the "▶ " marker (2) and the right-aligned size label (7).
    let bar_max = inner.width.saturating_sub(9);
    let lines: Vec<Line> = match state.view {
        View::Groups => {
            let groups = group_summaries(&state.scan);
            let max_size = groups.iter().map(|g| g.total_bytes).max().unwrap_or(0);
            let selected = state.group_list.selected();
            groups
                .into_iter()
                .enumerate()
                .take(inner.height as usize)
                .map(|(i, group)| {
                    let cells = bar_cells(group.total_bytes, max_size, bar_max);
                    let marker = if selected == Some(i) { "▶ " } else { "  " };
                    Line::from(vec![
                        Span::raw(marker),
                        Span::styled(
                            format!("{:>6} ", human(group.total_bytes)),
                            Style::default().bold(),
                        ),
                        Span::styled(
                            "█".repeat(cells as usize),
                            Style::default().fg(class_color(group.class)),
                        ),
                    ])
                })
                .collect()
        }
        View::Items { class } => {
            let indices = item_indices_for_class(&state.scan, class);
            let max_size = indices
                .iter()
                .filter_map(|&i| state.scan.items.get(i))
                .map(|item| item.size_on_disk)
                .max()
                .unwrap_or(0);
            let selected = state.item_list.selected();
            indices
                .into_iter()
                .enumerate()
                .take(inner.height as usize)
                .filter_map(|(row, i)| state.scan.items.get(i).map(|item| (row, item)))
                .map(|(row, item)| {
                    let cells = bar_cells(item.size_on_disk, max_size, bar_max);
                    let marker = if selected == Some(row) { "▶ " } else { "  " };
                    Line::from(vec![
                        Span::raw(marker),
                        Span::styled(
                            format!("{:>6} ", human(item.size_on_disk)),
                            Style::default().bold(),
                        ),
                        Span::styled(
                            "█".repeat(cells as usize),
                            Style::default().fg(class_color(item.class)),
                        ),
                    ])
                })
                .collect()
        }
    };

    f.render_widget(Paragraph::new(lines), inner);
}

/// Filled block cells for an Item's overview bar, scaled so the largest Item
/// (`max_size`) fills `max_width` and the rest are proportional to it. Any
/// non-empty Item gets at least one cell so it never visually disappears; a
/// zero-size Item or a zero scale/width yields zero (issue #7).
fn bar_cells(size: u64, max_size: u64, max_width: u16) -> u16 {
    if size == 0 || max_size == 0 || max_width == 0 {
        return 0;
    }
    let frac = size as f64 / max_size as f64;
    let cells = (frac * max_width as f64).round() as u16;
    cells.clamp(1, max_width)
}

fn draw_confirm(f: &mut Frame, state: &AppState, target: ConfirmTarget) {
    match target {
        ConfirmTarget::Item { index } => {
            if let Some(item) = state.scan.items.get(index) {
                draw_item_confirm(f, item);
            }
        }
        ConfirmTarget::Group { class } => {
            let group = group_summaries(&state.scan)
                .into_iter()
                .find(|group| group.class == class);
            if let Some(group) = group {
                draw_group_confirm(f, group);
            }
        }
    }
}

/// The deliberate Confirm prompt: quotes the Item's path, size, class, and how it
/// will be recovered, then waits for a `y`. This is the only gate to deletion.
fn draw_item_confirm(f: &mut Frame, item: &Item) {
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

/// Group-level Confirm keeps the batch action explicit: it quotes the Safety
/// Class, how many currently reclaimable Items will be affected, and the total
/// bytes. Protected and un-overridden Unclassified Items never reach this modal.
fn draw_group_confirm(f: &mut Frame, group: GroupSummary) {
    let area = centered_rect(64, 12, f.area());
    f.render_widget(Clear, area);

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("{} group", group.class.label()),
                Style::default()
                    .fg(class_color(group.class))
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::raw("  Reclaim "),
            Span::styled(human(group.reclaimable_bytes), Style::default().bold()),
            Span::raw(format!(
                " from {} {}",
                group.reclaimable_count,
                item_word(group.reclaimable_count)
            )),
        ]),
        Line::from(vec![
            Span::raw("  Group total: "),
            Span::styled(human(group.total_bytes), Style::default().fg(Color::Gray)),
            Span::raw(format!(
                " across {} {}",
                group.count,
                item_word(group.count)
            )),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                class_recovery_hint(group.class),
                Style::default().fg(Color::Gray),
            ),
        ]),
        Line::from("  After Confirm, reclaim runs one Item at a time."),
        Line::from("  You can stop before the next Item while it runs."),
    ];

    if group.class == SafetyClass::Unclassified {
        lines.push(Line::from("  Only overridden Items move to Trash."));
    }

    lines.extend([
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("[y]", Style::default().fg(Color::Green).bold()),
            Span::raw(" Reclaim group    "),
            Span::styled("[N]", Style::default().fg(Color::Red).bold()),
            Span::raw(" cancel"),
        ]),
    ]);

    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Confirm Group Reclaim ")
                .border_style(Style::default().fg(Color::Yellow)),
        ),
        area,
    );
}

fn draw_reclaiming(f: &mut Frame, state: &AppState, tick: usize) {
    let Some(job) = &state.reclaim_job else {
        return;
    };
    let progress = &job.progress;
    let area = centered_rect(72, 8, f.area());
    f.render_widget(Clear, area);

    let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let spinner = frames[tick % frames.len()];
    let current_path = progress
        .current_path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "Preparing next Item".into());

    let lines = if progress.total > 1 {
        let ordinal = progress.current_ordinal.max(progress.completed);
        vec![
            Line::from(vec![
                Span::raw("  "),
                Span::styled(spinner, Style::default().fg(Color::Yellow).bold()),
                Span::raw(" "),
                Span::raw(progress.title.clone()).bold(),
            ]),
            Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("{ordinal}/{} Items", progress.total),
                    Style::default().bold(),
                ),
                Span::raw(format!(" · {} reclaimed", human(progress.reclaimed_bytes))),
            ]),
            Line::from(vec![Span::raw("  Now: "), Span::raw(current_path)]),
            Line::from(if progress.stop_requested {
                vec![
                    Span::raw("  "),
                    Span::styled("Stop requested", Style::default().fg(Color::Yellow).bold()),
                    Span::raw(" · current Item will finish"),
                ]
            } else {
                vec![
                    Span::raw("  "),
                    Span::styled("[s/Esc]", Style::default().fg(Color::Yellow).bold()),
                    Span::raw(" stop after current Item"),
                ]
            }),
        ]
    } else {
        vec![
            Line::from(vec![
                Span::raw("  "),
                Span::styled(spinner, Style::default().fg(Color::Yellow).bold()),
                Span::raw(" "),
                Span::raw(progress.title.clone()).bold(),
            ]),
            Line::from(vec![Span::raw("  "), Span::raw(current_path)]),
            Line::from(vec![
                Span::raw("  "),
                Span::raw(progress.current_action.clone()),
            ]),
            Line::from("  This may take a while"),
        ]
    };

    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: true }).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Reclaiming ")
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
    Rect {
        x,
        y,
        width,
        height: height.min(area.height),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Evidence, RecoveryMethod};
    use crate::ruleset::{Match, Rule};
    use std::fs;
    use std::path::PathBuf;

    fn state_with(items: Vec<Item>) -> AppState {
        let mut state = AppState::new(
            Scan {
                root: PathBuf::from("/tmp"),
                items,
            },
            Ruleset::defaults(),
        );
        state.status.clear();
        state
    }

    fn node_modules_item(path: PathBuf) -> Item {
        Item {
            path,
            size_on_disk: 4096,
            class: SafetyClass::Reinstallable,
            recovery: RecoveryMethod::Reinstall {
                command: "npm install".into(),
            },
            evidence: Evidence {
                summary: "node_modules".into(),
            },
            override_reclaim: false,
        }
    }

    fn cache_item(path: PathBuf, size_on_disk: u64) -> Item {
        Item {
            path,
            size_on_disk,
            class: SafetyClass::Cache,
            recovery: RecoveryMethod::AutoRefill,
            evidence: Evidence {
                summary: "cache".into(),
            },
            override_reclaim: false,
        }
    }

    fn protected_item(path: PathBuf, size_on_disk: u64) -> Item {
        Item {
            path,
            size_on_disk,
            class: SafetyClass::Irreplaceable,
            recovery: RecoveryMethod::None,
            evidence: Evidence {
                summary: "VM image".into(),
            },
            override_reclaim: false,
        }
    }

    fn unclassified_item(path: PathBuf, override_reclaim: bool) -> Item {
        Item {
            path,
            size_on_disk: 4096,
            class: SafetyClass::Unclassified,
            recovery: RecoveryMethod::None,
            evidence: Evidence {
                summary: "unknown".into(),
            },
            override_reclaim,
        }
    }

    fn slow_regenerable_item(path: PathBuf) -> Item {
        Item {
            path,
            size_on_disk: 4096,
            class: SafetyClass::Regenerable,
            recovery: RecoveryMethod::Rebuild {
                command: "make".into(),
            },
            evidence: Evidence {
                summary: "fixture build output".into(),
            },
            override_reclaim: false,
        }
    }

    fn slow_ruleset() -> Ruleset {
        Ruleset {
            rules: vec![Rule {
                name: "slow-buildcache".into(),
                matches: Match::DirNamed {
                    dir: "buildcache".into(),
                },
                class: SafetyClass::Regenerable,
                clean_command: Some("sleep 0.2".into()),
                recover_command: Some("make".into()),
                evidence: "fixture build output".into(),
            }],
        }
    }

    fn select_group(state: &mut AppState, class: SafetyClass) {
        let idx = group_summaries(&state.scan)
            .iter()
            .position(|group| group.class == class)
            .expect("group exists");
        state.group_list.select(Some(idx));
    }

    fn wait_for_reclaim(state: &mut AppState) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while matches!(state.mode, Mode::Reclaiming) && Instant::now() < deadline {
            drain_reclaim_messages(state);
            std::thread::sleep(Duration::from_millis(10));
        }
        drain_reclaim_messages(state);
        assert!(
            !matches!(state.mode, Mode::Reclaiming),
            "reclaim worker did not finish"
        );
    }

    fn wait_until_reclaim_current_ordinal(state: &mut AppState, ordinal: usize) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            drain_reclaim_messages(state);
            if state
                .reclaim_job
                .as_ref()
                .is_some_and(|job| job.progress.current_ordinal == ordinal)
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("reclaim worker did not reach item {ordinal}");
    }

    #[test]
    fn default_selection_is_largest_reclaimable_group() {
        let state = state_with(vec![
            node_modules_item(PathBuf::from("/tmp/node_modules")),
            cache_item(PathBuf::from("/tmp/.npm"), 16 * 1024),
            protected_item(PathBuf::from("/tmp/disk.img.raw"), 64 * 1024),
        ]);

        let selected = selected_group_summary(&state).expect("a group is selected");
        assert_eq!(selected.class, SafetyClass::Cache);
        assert_eq!(selected.reclaimable_bytes, 16 * 1024);
    }

    #[test]
    fn enter_opens_selected_group() {
        let mut state = state_with(vec![node_modules_item(PathBuf::from("/tmp/node_modules"))]);

        handle_key(&mut state, KeyCode::Enter);

        assert_eq!(
            state.view,
            View::Items {
                class: SafetyClass::Reinstallable
            }
        );
        assert_eq!(state.item_list.selected(), Some(0));
    }

    #[test]
    fn item_reclaim_inside_group_still_opens_confirm_first() {
        let tmp = tempfile::tempdir().unwrap();
        let nm = tmp.path().join("node_modules");
        fs::create_dir(&nm).unwrap();

        let mut state = state_with(vec![node_modules_item(nm.clone())]);
        handle_key(&mut state, KeyCode::Enter);
        handle_key(&mut state, KeyCode::Char('c'));

        assert!(matches!(
            state.mode,
            Mode::Confirm {
                target: ConfirmTarget::Item { index: 0 }
            }
        ));
        assert!(nm.exists(), "nothing is deleted before item Confirm");
    }

    #[test]
    fn unclassified_group_requires_item_override_before_confirm() {
        let mut state = state_with(vec![unclassified_item(
            PathBuf::from("/tmp/mystery"),
            false,
        )]);

        handle_key(&mut state, KeyCode::Char('c'));
        assert!(matches!(state.mode, Mode::Browse));

        handle_key(&mut state, KeyCode::Enter);
        handle_key(&mut state, KeyCode::Char('o'));
        assert!(state.scan.items[0].override_reclaim);

        handle_key(&mut state, KeyCode::Esc);
        handle_key(&mut state, KeyCode::Char('c'));

        assert!(matches!(
            state.mode,
            Mode::Confirm {
                target: ConfirmTarget::Group {
                    class: SafetyClass::Unclassified
                }
            }
        ));
    }

    /// Acceptance for grouped UX plus CONTEXT.md "Confirm": a single group
    /// reclaim keypress must NOT delete — it only opens the Confirm modal.
    #[test]
    fn group_reclaim_key_opens_confirm_and_deletes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let nm = tmp.path().join("node_modules");
        fs::create_dir(&nm).unwrap();
        fs::write(nm.join("index.js"), b"x").unwrap();

        let mut state = state_with(vec![node_modules_item(nm.clone())]);
        let quit = handle_key(&mut state, KeyCode::Char('c'));

        assert!(!quit);
        assert!(matches!(
            state.mode,
            Mode::Confirm {
                target: ConfirmTarget::Group {
                    class: SafetyClass::Reinstallable
                }
            }
        ));
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

    /// Only the deliberate `y` in the group Confirm modal actually Reclaims.
    #[test]
    fn confirm_y_reclaims_the_group() {
        let tmp = tempfile::tempdir().unwrap();
        let nm1 = tmp.path().join("node_modules");
        let nm2 = tmp.path().join("app").join("node_modules");
        let cache = tmp.path().join(".npm");
        fs::create_dir_all(&nm1).unwrap();
        fs::create_dir_all(&nm2).unwrap();
        fs::create_dir_all(&cache).unwrap();
        fs::write(nm1.join("index.js"), b"x").unwrap();
        fs::write(nm2.join("index.js"), b"x").unwrap();
        fs::write(cache.join("blob"), b"x").unwrap();

        let mut state = state_with(vec![
            node_modules_item(nm1.clone()),
            node_modules_item(nm2.clone()),
            cache_item(cache.clone(), 1024),
        ]);
        select_group(&mut state, SafetyClass::Reinstallable);
        handle_key(&mut state, KeyCode::Char('c'));
        handle_key(&mut state, KeyCode::Char('y'));

        assert!(matches!(state.mode, Mode::Reclaiming));
        wait_for_reclaim(&mut state);

        assert!(matches!(state.mode, Mode::Browse));
        assert!(!nm1.exists(), "the y confirm reclaims the first group Item");
        assert!(
            !nm2.exists(),
            "the y confirm reclaims the second group Item"
        );
        assert!(cache.exists(), "other groups are left alone");
        assert_eq!(state.scan.items.len(), 1);
    }

    /// A running group Reclaim is locked, but the user can request that no next
    /// Item starts. The current Item is allowed to finish because mid-delete
    /// cancellation cannot promise a rollback.
    #[test]
    fn stop_after_current_leaves_remaining_group_items_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("one").join("buildcache");
        let second = tmp.path().join("two").join("buildcache");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();

        let mut state = state_with(vec![
            slow_regenerable_item(first),
            slow_regenerable_item(second),
        ]);
        state.ruleset = slow_ruleset();

        select_group(&mut state, SafetyClass::Regenerable);
        handle_key(&mut state, KeyCode::Char('c'));
        handle_key(&mut state, KeyCode::Char('y'));
        wait_until_reclaim_current_ordinal(&mut state, 1);

        handle_key(&mut state, KeyCode::Char('s'));
        wait_for_reclaim(&mut state);

        assert!(matches!(state.mode, Mode::Browse));
        assert_eq!(state.scan.items.len(), 1, "one Item was not touched");
        assert!(state.status.contains("Stopped after current Item"));
        assert!(state.status.contains("1 not touched"));
    }

    /// A Protected (Irreplaceable) Item never opens the Confirm modal — the
    /// guardrail holds before deletion is ever offered.
    #[test]
    fn protected_item_never_reaches_confirm() {
        let mut state = state_with(vec![protected_item(
            PathBuf::from("/tmp/orbstack.img.raw"),
            4096,
        )]);
        handle_key(&mut state, KeyCode::Char('c'));
        assert!(matches!(state.mode, Mode::Browse));
    }

    /// Empty-list edge case: navigation, override, and confirm keypresses must be
    /// no-ops that never panic and never select anything (issue #5).
    #[test]
    fn empty_list_keys_are_safe_noops() {
        let mut state = state_with(vec![]);
        assert_eq!(state.group_list.selected(), None);

        for code in [
            KeyCode::Down,
            KeyCode::Up,
            KeyCode::Char('o'),
            KeyCode::Char('c'),
            KeyCode::Enter,
        ] {
            assert!(!handle_key(&mut state, code));
            assert!(matches!(state.mode, Mode::Browse));
            assert_eq!(
                state.group_list.selected(),
                None,
                "nothing to select in an empty group list"
            );
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
        state.view = View::Items {
            class: SafetyClass::Reinstallable,
        };
        state.item_list.select(Some(0));
        do_reclaim_item(&mut state, 0);

        assert!(state.scan.items.is_empty());
        assert_eq!(state.item_list.selected(), None);
        assert_eq!(state.group_list.selected(), None);
        assert_eq!(state.view, View::Groups);
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
        state.view = View::Items {
            class: SafetyClass::Reinstallable,
        };
        state.item_list.select(Some(2)); // highlight the tail
        do_reclaim_item(&mut state, 2);

        assert_eq!(state.scan.items.len(), 2);
        let sel = state.item_list.selected().expect("selection survives");
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
        state.view = View::Items {
            class: SafetyClass::Reinstallable,
        };
        state.item_list.select(Some(0));

        move_sel(&mut state, -1); // wrap from 0 back to the tail
        assert_eq!(state.item_list.selected(), Some(2));
        move_sel(&mut state, 1); // wrap from tail back to head
        assert_eq!(state.item_list.selected(), Some(0));
    }

    /// The overview pane toggles on and off with `t` and starts hidden (issue #7).
    #[test]
    fn overview_toggles_with_t() {
        let mut state = state_with(vec![node_modules_item(PathBuf::from("/tmp/node_modules"))]);
        assert!(!state.show_overview, "overview starts hidden");
        handle_key(&mut state, KeyCode::Char('t'));
        assert!(state.show_overview, "t shows the overview");
        handle_key(&mut state, KeyCode::Char('t'));
        assert!(!state.show_overview, "t hides it again");
    }

    /// The overview bar scale (issue #7): the largest Item fills the pane, others
    /// are proportional, any non-empty Item keeps at least one cell, and a zero
    /// scale/size/width can never divide-by-zero or overflow the width.
    #[test]
    fn bar_cells_scales_proportionally_and_safely() {
        assert_eq!(bar_cells(100, 100, 20), 20, "largest fills the pane");
        assert_eq!(bar_cells(50, 100, 20), 10, "half size → half width");
        assert_eq!(bar_cells(1, 1_000_000, 20), 1, "tiny Item keeps one cell");
        assert_eq!(bar_cells(0, 100, 20), 0, "empty Item draws nothing");
        assert_eq!(bar_cells(50, 0, 20), 0, "zero scale never divides by zero");
        assert_eq!(bar_cells(50, 100, 0), 0, "zero width draws nothing");
        // Monotonic in size and never wider than the pane.
        assert!(bar_cells(30, 100, 20) <= bar_cells(60, 100, 20));
        assert!(bar_cells(u64::MAX, 100, 20) <= 20);
    }

    /// Resize edge case: the Confirm modal geometry must not overflow on a very
    /// wide terminal and must stay clamped inside the area (issue #5).
    #[test]
    fn centered_rect_survives_wide_terminal() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 4000,
            height: 50,
        };
        let r = centered_rect(64, 9, area);
        assert!(r.width <= area.width);
        assert!(r.x + r.width <= area.x + area.width);
        assert!(r.y + r.height <= area.y + area.height);
    }

    /// A modal taller than the terminal is clamped to the available height instead
    /// of drawing off-screen (issue #5).
    #[test]
    fn centered_rect_clamps_to_short_terminal() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 4,
        };
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
            evidence: Evidence {
                summary: "unknown".into(),
            },
            override_reclaim: true,
        };
        assert!(unknown.recovery_line().contains("Trash"));
    }
}
