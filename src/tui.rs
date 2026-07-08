//! Interactive, live-updating terminal UI built on `ratatui` + `crossterm`.
//!
//! The scan runs on a background thread and streams [`ScanEvent`]s back over a
//! channel; the UI thread drains them every tick, so dependency directories
//! appear as soon as they are discovered and their sizes fill in as the
//! parallel sizing completes. The user can scroll, re-sort, group by ecosystem,
//! filter, mark directories for deletion, and quit; on exit the final
//! largest-first results (minus anything deleted) are returned so the caller can
//! leave a static summary in the terminal scrollback.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Paragraph, Row as TableRow, Table, TableState, Wrap,
};
use ratatui::DefaultTerminal;

use crate::report::{group_thousands, human_size, megabytes_size, relative_age};
use crate::scanner::{self, DependencyDir, ScanEvent, ScanOptions};
use crate::state::{self, AgeFilter, IconStyle, SizeFilter, SizeUnit, SortKey, ViewState};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Redraw cadence while something is animating (the scan or a deletion is
/// running): fast enough for a smooth spinner and live-streaming rows.
const TICK: Duration = Duration::from_millis(100);
/// Once the scan is finished and nothing is animating the screen is static, so
/// the event loop blocks for up to this long waiting for input instead of
/// busy-redrawing every `TICK` — keeping idle CPU usage near zero. A resize or
/// key press wakes it sooner.
const IDLE_TICK: Duration = Duration::from_secs(3600);

/// Foreground/background accents for directories marked for deletion.
const SELECT_FG: Color = Color::Rgb(255, 120, 120);
const SELECT_BG: Color = Color::Rgb(60, 28, 34);

/// Run the interactive TUI for a scan of `root`, returning the final list of
/// dependency directories (sorted largest-first and filtered by `min_size` /
/// `top`) once the user quits.
pub fn run(
    root: &Path,
    opts: ScanOptions,
    top: usize,
    min_size: u64,
    icons: Option<IconStyle>,
) -> Result<Vec<DependencyDir>> {
    let (tx, rx) = mpsc::channel();
    let root_buf = root.to_path_buf();
    let scan_thread = thread::spawn(move || {
        scanner::scan_streaming(&root_buf, &opts, tx);
    });

    // Restore the view preferences last used for this directory. A `--icons`
    // flag overrides the remembered icon style just for this run.
    let mut view = state::load(root);
    if let Some(style) = icons {
        view.icon_style = style;
    }

    let mut terminal = ratatui::init();
    let mut app = App::new(root.to_path_buf(), top, min_size, view);
    let result = app.event_loop(&mut terminal, rx);
    ratatui::restore();

    // The scan thread finishes once its `tx` is dropped at the end of
    // `scan_streaming`; joining keeps us from leaking it.
    let _ = scan_thread.join();

    // Remember the current view for next time (best-effort; only on clean exit).
    if result.is_ok() {
        state::save(root, &app.view_state());
    }

    result.map(|()| app.final_results())
}

/// One discovered directory: either still being sized, or done.
enum Row {
    Pending {
        path: PathBuf,
        ecosystem: &'static str,
    },
    Done(DependencyDir),
}

impl Row {
    fn path(&self) -> &Path {
        match self {
            Row::Pending { path, .. } => path,
            Row::Done(d) => &d.path,
        }
    }

    fn ecosystem(&self) -> &'static str {
        match self {
            Row::Pending { ecosystem, .. } => ecosystem,
            Row::Done(d) => d.ecosystem,
        }
    }
}

/// A modal overlay drawn on top of the table; only one can be open at a time.
enum Overlay {
    None,
    Help,
    Ecosystems {
        cursor: usize,
    },
    /// Confirmation gate before deleting the marked directories. `permanent`
    /// toggles between moving to the OS trash (default) and an irreversible
    /// `remove_dir_all`.
    ConfirmDelete {
        permanent: bool,
    },
}

/// The result of trying to remove one directory, sent back from the worker
/// thread as it goes so the UI can update without blocking.
enum DeleteOutcome {
    Removed { path: PathBuf, size: u64 },
    Failed { path: PathBuf, error: String },
}

/// Live state of an in-flight deletion, driving the progress modal.
struct DeleteProgress {
    done: usize,
    total: usize,
    reclaimed: u64,
    failures: usize,
    permanent: bool,
}

/// One rendered line of the table: an ecosystem group header (only when
/// grouping is on) or an actual directory row.
enum DisplayItem<'a> {
    Header {
        ecosystem: &'static str,
        total: u64,
        count: usize,
        /// How many of the group's directories are marked for deletion.
        selected: usize,
        collapsed: bool,
    },
    Data(&'a Row),
}

struct App {
    root: PathBuf,
    rows: Vec<Row>,
    /// Path → position in `rows`, so a `Sized` event can update the matching
    /// discovered row in O(1) instead of scanning the whole list. Sizing
    /// completions arrive in bursts, and on a large tree an O(n) search per
    /// completion adds up to O(n²) work that stalls the UI thread. Rebuilt
    /// whenever rows are removed (see [`App::remove_rows`]).
    index: HashMap<PathBuf, usize>,
    warnings: usize,
    last_warning: Option<String>,
    scanning: bool,
    sort: SortKey,
    group: bool,
    size_filter: SizeFilter,
    age_filter: AgeFilter,
    /// How the size column renders byte counts (whole MB vs adaptive humansize).
    size_unit: SizeUnit,
    /// Which icon (if any) precedes the ecosystem label.
    icon_style: IconStyle,
    /// Ecosystems the user has toggled *off* in the ecosystem panel. Empty means
    /// every ecosystem is shown. Stored by label so it survives across restarts.
    disabled_ecos: HashSet<String>,
    /// Ecosystems whose group is collapsed (header shown, rows hidden). Only has
    /// an effect while grouping is on.
    collapsed_ecos: HashSet<String>,
    /// Directories marked for deletion, keyed by path. In-memory only — a
    /// deletion set is deliberately never persisted across runs.
    selected: HashSet<PathBuf>,
    /// Transient one-line status shown in the footer after an action (e.g. the
    /// result of a deletion). Cleared the next time the selection changes.
    action: Option<String>,
    /// While a deletion is running it happens on a worker thread; outcomes stream
    /// back over this channel so the UI stays responsive. `Some` iff a deletion
    /// is in flight, paired with `delete_progress`.
    delete_rx: Option<mpsc::Receiver<DeleteOutcome>>,
    delete_progress: Option<DeleteProgress>,
    overlay: Overlay,
    spinner: usize,
    top: usize,
    min_size: u64,
    state: TableState,
}

impl App {
    fn new(root: PathBuf, top: usize, min_size: u64, view: ViewState) -> Self {
        App {
            root,
            rows: Vec::new(),
            index: HashMap::new(),
            warnings: 0,
            last_warning: None,
            scanning: true,
            sort: view.sort,
            group: view.group,
            size_filter: view.size_filter,
            age_filter: view.age_filter,
            size_unit: view.size_unit,
            icon_style: view.icon_style,
            disabled_ecos: view.disabled_ecos,
            collapsed_ecos: view.collapsed_ecos,
            selected: HashSet::new(),
            action: None,
            delete_rx: None,
            delete_progress: None,
            overlay: Overlay::None,
            spinner: 0,
            top,
            min_size,
            state: TableState::default(),
        }
    }

    /// Snapshot the current view preferences for persistence.
    fn view_state(&self) -> ViewState {
        ViewState {
            sort: self.sort,
            group: self.group,
            size_filter: self.size_filter,
            age_filter: self.age_filter,
            size_unit: self.size_unit,
            icon_style: self.icon_style,
            disabled_ecos: self.disabled_ecos.clone(),
            collapsed_ecos: self.collapsed_ecos.clone(),
        }
    }

    fn event_loop(
        &mut self,
        terminal: &mut DefaultTerminal,
        rx: mpsc::Receiver<ScanEvent>,
    ) -> Result<()> {
        // Draw once up front so there's a UI before we block on the first poll.
        self.drain(&rx);
        self.drain_deletion();
        terminal.draw(|frame| self.draw(frame))?;

        loop {
            // The spinner only animates while the scan or a deletion is running;
            // the rest of the time the screen is static. When idle we block on
            // input (up to `IDLE_TICK`) and redraw only in response to an event,
            // instead of re-rendering at `TICK` forever — which otherwise burns
            // CPU long after the scan has finished.
            let animating = self.scanning || self.delete_progress.is_some();
            let timeout = if animating { TICK } else { IDLE_TICK };

            // An event (key press, resize, …) means the view may have changed.
            let mut dirty = false;
            if event::poll(timeout)? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press && self.handle_key(key.code, key.modifiers) {
                        return Ok(());
                    }
                }
                dirty = true;
            }

            // Scan results and deletion outcomes can arrive between polls.
            self.drain(&rx);
            self.drain_deletion();

            if animating {
                self.spinner = (self.spinner + 1) % SPINNER.len();
                terminal.draw(|frame| self.draw(frame))?;
            } else if dirty {
                terminal.draw(|frame| self.draw(frame))?;
            }
        }
    }

    /// Pull every currently-available scan event without blocking. A
    /// disconnected channel means the scan thread has finished.
    fn drain(&mut self, rx: &mpsc::Receiver<ScanEvent>) {
        loop {
            match rx.try_recv() {
                Ok(event) => self.apply(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.scanning = false;
                    break;
                }
            }
        }
    }

    fn apply(&mut self, event: ScanEvent) {
        match event {
            ScanEvent::Discovered {
                path, ecosystem, ..
            } => {
                self.index.insert(path.clone(), self.rows.len());
                self.rows.push(Row::Pending { path, ecosystem });
            }
            ScanEvent::Sized(dir) => {
                if let Some(&i) = self.index.get(&dir.path) {
                    self.rows[i] = Row::Done(dir);
                } else {
                    self.index.insert(dir.path.clone(), self.rows.len());
                    self.rows.push(Row::Done(dir));
                }
            }
            ScanEvent::Warning(message) => {
                self.warnings += 1;
                self.last_warning = Some(message);
            }
        }
    }

    /// Returns `true` when the app should quit.
    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        // Ctrl-C always quits, regardless of any open overlay or in-flight work.
        if mods.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
            return true;
        }

        // While a deletion is running the UI is read-only: swallow every other
        // key so nothing races the worker thread (or quits mid-delete).
        if self.delete_progress.is_some() {
            return false;
        }

        match self.overlay {
            Overlay::Help => {
                // Any key dismisses the help dialog.
                self.overlay = Overlay::None;
                false
            }
            Overlay::Ecosystems { .. } => {
                self.handle_eco_key(code);
                false
            }
            Overlay::ConfirmDelete { .. } => {
                self.handle_confirm_key(code);
                false
            }
            Overlay::None => self.handle_main_key(code),
        }
    }

    fn handle_main_key(&mut self, code: KeyCode) -> bool {
        let len = self.visible_len();
        match code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::Home => self.select_first(),
            KeyCode::End => {
                if len > 0 {
                    self.move_selection(len as isize);
                }
            }
            // Enter folds the selected group; Space/x marks for deletion. Both
            // used to fold, but selection needs a binding and a checkbox-style
            // Space is the more intuitive of the two.
            KeyCode::Enter => self.toggle_collapse(),
            KeyCode::Char(' ') | KeyCode::Char('x') => self.toggle_selection(),
            KeyCode::Char('A') => self.toggle_select_all_visible(),
            KeyCode::Char('c') => self.clear_selection(),
            KeyCode::Char('d') | KeyCode::Delete => self.open_confirm_delete(),
            KeyCode::Char('S') => self.sort = self.sort.next(),
            KeyCode::Char('u') => self.size_unit = self.size_unit.toggle(),
            KeyCode::Char('i') => self.icon_style = self.icon_style.cycle(),
            KeyCode::Char('g') => self.group = !self.group,
            KeyCode::Char('s') => self.size_filter = self.size_filter.next(),
            KeyCode::Char('a') => self.age_filter = self.age_filter.next(),
            KeyCode::Char('e') => self.overlay = Overlay::Ecosystems { cursor: 0 },
            KeyCode::Char('r') => self.reset_filters(),
            KeyCode::Char('?') => self.overlay = Overlay::Help,
            _ => {}
        }
        false
    }

    fn handle_confirm_key(&mut self, code: KeyCode) {
        let permanent = match self.overlay {
            Overlay::ConfirmDelete { permanent } => permanent,
            _ => return,
        };
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                self.overlay = Overlay::None;
                self.start_deletion(permanent);
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => {
                self.overlay = Overlay::None;
            }
            KeyCode::Char('t') | KeyCode::Char('T') => {
                self.overlay = Overlay::ConfirmDelete {
                    permanent: !permanent,
                };
            }
            _ => {}
        }
    }

    fn handle_eco_key(&mut self, code: KeyCode) {
        let ecos = self.ecosystems_present();
        let Overlay::Ecosystems { cursor } = &mut self.overlay else {
            return;
        };
        match code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('e') => {
                self.overlay = Overlay::None;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !ecos.is_empty() {
                    *cursor = (*cursor + 1).min(ecos.len() - 1);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                *cursor = cursor.saturating_sub(1);
            }
            KeyCode::Char(' ') | KeyCode::Enter => {
                if let Some(&eco) = ecos.get(*cursor) {
                    if !self.disabled_ecos.remove(eco) {
                        self.disabled_ecos.insert(eco.to_string());
                    }
                }
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                // Toggle all: if anything is disabled, enable everything;
                // otherwise disable everything.
                if self.disabled_ecos.is_empty() {
                    self.disabled_ecos
                        .extend(ecos.iter().map(|e| e.to_string()));
                } else {
                    self.disabled_ecos.clear();
                }
            }
            _ => {}
        }
    }

    fn reset_filters(&mut self) {
        self.group = false;
        self.size_filter = SizeFilter::All;
        self.age_filter = AgeFilter::Any;
        self.disabled_ecos.clear();
        self.collapsed_ecos.clear();
    }

    fn select_first(&mut self) {
        let select = if self.visible_len() > 0 {
            Some(0)
        } else {
            None
        };
        self.state.select(select);
    }

    /// Move the selection by `delta` rows. Every displayed item — including
    /// group headers, so they can be folded — is selectable.
    fn move_selection(&mut self, delta: isize) {
        let len = self.visible_len();
        if len == 0 {
            self.state.select(None);
            return;
        }
        let current = self.state.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, len as isize - 1) as usize;
        self.state.select(Some(next));
    }

    /// Fold or unfold the group whose header is selected (no-op otherwise).
    fn toggle_collapse(&mut self) {
        let Some(sel) = self.state.selected() else {
            return;
        };
        let eco = match self.display_items(now()).get(sel) {
            Some(DisplayItem::Header { ecosystem, .. }) => *ecosystem,
            _ => return,
        };
        if !self.collapsed_ecos.remove(eco) {
            self.collapsed_ecos.insert(eco.to_string());
        }
    }

    /// Mark or unmark the highlighted item for deletion. On a data row this
    /// toggles that single directory; on a group header it toggles every
    /// (currently visible) directory in the ecosystem at once. Pending rows,
    /// whose size isn't known yet, can't be selected.
    fn toggle_selection(&mut self) {
        self.action = None;
        let Some(sel) = self.state.selected() else {
            return;
        };
        // Resolve the selection to an owned target first so the immutable borrow
        // from `display_items` is released before we mutate `self.selected`.
        enum Target {
            Row(PathBuf),
            Group(&'static str),
        }
        let target = match self.display_items(now()).get(sel) {
            Some(DisplayItem::Data(Row::Done(d))) => Some(Target::Row(d.path.clone())),
            Some(DisplayItem::Header { ecosystem, .. }) => Some(Target::Group(ecosystem)),
            _ => None,
        };
        match target {
            Some(Target::Row(path)) => {
                if !self.selected.remove(&path) {
                    self.selected.insert(path);
                }
            }
            Some(Target::Group(eco)) => self.toggle_group_selection(eco),
            None => {}
        }
    }

    /// Toggle selection for every visible directory in `eco`: if they are all
    /// already marked, clear them; otherwise mark them all.
    fn toggle_group_selection(&mut self, eco: &str) {
        let now = now();
        let paths: Vec<PathBuf> = self
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Done(d) if d.ecosystem == eco && self.passes(d, now) => Some(d.path.clone()),
                _ => None,
            })
            .collect();
        let all_marked = !paths.is_empty() && paths.iter().all(|p| self.selected.contains(p));
        if all_marked {
            for p in &paths {
                self.selected.remove(p);
            }
        } else {
            self.selected.extend(paths);
        }
    }

    /// Toggle selection for every directory currently shown (respecting active
    /// filters and the `--top` cap): mark them all, or clear them if they were.
    fn toggle_select_all_visible(&mut self) {
        self.action = None;
        let paths: Vec<PathBuf> = self
            .display_items(now())
            .iter()
            .filter_map(|it| match it {
                DisplayItem::Data(Row::Done(d)) => Some(d.path.clone()),
                _ => None,
            })
            .collect();
        let all_marked = !paths.is_empty() && paths.iter().all(|p| self.selected.contains(p));
        if all_marked {
            for p in &paths {
                self.selected.remove(p);
            }
        } else {
            self.selected.extend(paths);
        }
    }

    fn clear_selection(&mut self) {
        self.selected.clear();
        self.action = None;
    }

    /// Open the delete-confirmation overlay, defaulting to the recoverable
    /// move-to-trash mode. A no-op (with a nudge) when nothing is marked.
    fn open_confirm_delete(&mut self) {
        if self.selected_total().0 == 0 {
            self.action = Some("Nothing marked — press space to mark directories.".to_string());
            return;
        }
        self.overlay = Overlay::ConfirmDelete { permanent: false };
    }

    /// Count and total size of the directories currently marked for deletion.
    fn selected_total(&self) -> (usize, u64) {
        self.rows
            .iter()
            .filter_map(|r| match r {
                Row::Done(d) if self.selected.contains(&d.path) => Some(d.size_bytes),
                _ => None,
            })
            .fold((0, 0), |(n, bytes), size| (n + 1, bytes + size))
    }

    /// The marked directories as `(path, size)` pairs, largest first — used to
    /// render the confirmation list.
    fn selected_dirs(&self) -> Vec<(PathBuf, u64)> {
        let mut dirs: Vec<(PathBuf, u64)> = self
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Done(d) if self.selected.contains(&d.path) => {
                    Some((d.path.clone(), d.size_bytes))
                }
                _ => None,
            })
            .collect();
        dirs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        dirs
    }

    /// Begin deleting every marked directory, either to the OS trash or
    /// permanently. The removal runs on a worker thread — moving a large tree to
    /// the trash (especially across volumes, where it becomes a recursive copy)
    /// can take a while, and doing it inline would freeze the UI — while
    /// `drain_deletion` applies the streamed results each tick.
    fn start_deletion(&mut self, permanent: bool) {
        let targets = self.selected_dirs();
        if targets.is_empty() {
            return;
        }
        self.action = None;
        self.delete_progress = Some(DeleteProgress {
            done: 0,
            total: targets.len(),
            reclaimed: 0,
            failures: 0,
            permanent,
        });

        let (tx, rx) = mpsc::channel();
        self.delete_rx = Some(rx);
        thread::spawn(move || {
            for (path, size) in targets {
                let outcome = match delete_path(&path, permanent) {
                    Ok(()) => DeleteOutcome::Removed { path, size },
                    Err(error) => DeleteOutcome::Failed { path, error },
                };
                // The UI hung up (quit): stop early, nothing left to report to.
                if tx.send(outcome).is_err() {
                    break;
                }
            }
        });
    }

    /// Apply any deletion outcomes that have arrived from the worker thread,
    /// pruning removed rows live and finishing up once the channel closes.
    fn drain_deletion(&mut self) {
        loop {
            let Some(rx) = &self.delete_rx else {
                return;
            };
            match rx.try_recv() {
                Ok(DeleteOutcome::Removed { path, size }) => {
                    self.remove_rows(std::slice::from_ref(&path));
                    self.selected.remove(&path);
                    if let Some(p) = &mut self.delete_progress {
                        p.done += 1;
                        p.reclaimed += size;
                    }
                }
                Ok(DeleteOutcome::Failed { path, error }) => {
                    self.warnings += 1;
                    self.last_warning =
                        Some(format!("failed to delete {}: {error}", path.display()));
                    self.selected.remove(&path);
                    if let Some(p) = &mut self.delete_progress {
                        p.done += 1;
                        p.failures += 1;
                    }
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.finish_deletion();
                    return;
                }
            }
        }
    }

    /// Tear down the in-flight deletion state and leave a one-line summary in the
    /// footer.
    fn finish_deletion(&mut self) {
        self.delete_rx = None;
        let Some(p) = self.delete_progress.take() else {
            return;
        };
        let verb = if p.permanent { "Deleted" } else { "Trashed" };
        let ok = p.done - p.failures;
        self.action = Some(if p.failures == 0 {
            format!(
                "{verb} {ok} director{} · reclaimed {}",
                if ok == 1 { "y" } else { "ies" },
                self.fmt_size(p.reclaimed)
            )
        } else {
            format!(
                "{verb} {ok}, {} failed · reclaimed {} · see ⚠",
                p.failures,
                self.fmt_size(p.reclaimed)
            )
        });
    }

    /// Drop the given paths from the row list (after they were deleted on disk).
    fn remove_rows(&mut self, paths: &[PathBuf]) {
        if paths.is_empty() {
            return;
        }
        self.rows.retain(|r| !paths.iter().any(|p| p == r.path()));
        // Retaining shifts positions, so the path→row index is now stale.
        // Removals are user-initiated and rare, so a full rebuild is fine.
        self.rebuild_index();
    }

    /// Rebuild the path→row index from scratch to match the current `rows`.
    fn rebuild_index(&mut self) {
        self.index.clear();
        for (i, row) in self.rows.iter().enumerate() {
            self.index.insert(row.path().to_path_buf(), i);
        }
    }

    /// Whether a completed directory passes the active filters.
    fn passes(&self, d: &DependencyDir, now: SystemTime) -> bool {
        d.size_bytes >= self.min_size
            && d.size_bytes >= self.size_filter.min_bytes()
            && !self.disabled_ecos.contains(d.ecosystem)
            && self.age_passes(d.modified, now)
    }

    fn age_passes(&self, modified: Option<SystemTime>, now: SystemTime) -> bool {
        match self.age_filter.min_age_secs() {
            None => true,
            Some(min) => match modified {
                Some(m) => now
                    .duration_since(m)
                    .map(|d| d.as_secs() >= min)
                    .unwrap_or(false),
                None => false,
            },
        }
    }

    /// Build the ordered list of items to render this frame. When grouping is
    /// off this is just the filtered, sorted, top-capped directory rows followed
    /// by any still-pending rows. When grouping is on, rows are bucketed by
    /// ecosystem (groups ordered by total size) with a subtotal header each.
    fn display_items(&self, now: SystemTime) -> Vec<DisplayItem<'_>> {
        let done: Vec<&Row> = self
            .rows
            .iter()
            .filter(|r| matches!(r, Row::Done(d) if self.passes(d, now)))
            .collect();

        // Pending rows are only shown while scanning, and only filtered by the
        // ecosystem toggle (their size/age aren't known yet).
        let pending: Vec<&Row> = if self.scanning {
            self.rows
                .iter()
                .filter(|r| {
                    matches!(r, Row::Pending { .. }) && !self.disabled_ecos.contains(r.ecosystem())
                })
                .collect()
        } else {
            Vec::new()
        };

        if !self.group {
            let mut done = done;
            done.sort_by(|a, b| match (a, b) {
                (Row::Done(a), Row::Done(b)) => self.compare(a, b),
                _ => std::cmp::Ordering::Equal,
            });
            if self.top > 0 && done.len() > self.top {
                done.truncate(self.top);
            }
            let mut items: Vec<DisplayItem> = done.into_iter().map(DisplayItem::Data).collect();
            items.extend(pending.into_iter().map(DisplayItem::Data));
            return items;
        }

        // Grouped: bucket every row by its ecosystem.
        let mut groups: HashMap<&'static str, Vec<&Row>> = HashMap::new();
        for r in done.into_iter().chain(pending) {
            groups.entry(r.ecosystem()).or_default().push(r);
        }

        // Order the groups by their done-size subtotal, largest first.
        let mut ordered: Vec<(&'static str, Vec<&Row>)> = groups.into_iter().collect();
        for (_, rows) in &mut ordered {
            rows.sort_by(|a, b| match (a, b) {
                (Row::Done(a), Row::Done(b)) => self.compare(a, b),
                (Row::Done(_), Row::Pending { .. }) => std::cmp::Ordering::Less,
                (Row::Pending { .. }, Row::Done(_)) => std::cmp::Ordering::Greater,
                _ => std::cmp::Ordering::Equal,
            });
        }
        ordered.sort_by(|a, b| {
            group_total(&b.1)
                .cmp(&group_total(&a.1))
                .then_with(|| a.0.cmp(b.0))
        });

        let mut items = Vec::new();
        for (eco, rows) in ordered {
            let total = group_total(&rows);
            let count = rows.iter().filter(|r| matches!(r, Row::Done(_))).count();
            let selected = rows
                .iter()
                .filter(|r| matches!(r, Row::Done(d) if self.selected.contains(&d.path)))
                .count();
            let collapsed = self.collapsed_ecos.contains(eco);
            items.push(DisplayItem::Header {
                ecosystem: eco,
                total,
                count,
                selected,
                collapsed,
            });
            if !collapsed {
                items.extend(rows.into_iter().map(DisplayItem::Data));
            }
        }
        items
    }

    fn compare(&self, a: &DependencyDir, b: &DependencyDir) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        let ord = match self.sort {
            SortKey::Size => b.size_bytes.cmp(&a.size_bytes),
            SortKey::Files => b.file_count.cmp(&a.file_count),
            SortKey::Name => a.name.cmp(b.name),
            // Most-recently modified first; unknown timestamps sort last.
            SortKey::Modified => match (b.modified, a.modified) {
                (Some(x), Some(y)) => x.cmp(&y),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            },
        };
        ord.then_with(|| a.path.cmp(&b.path))
    }

    /// Number of selectable rows in the current view (data rows plus, when
    /// grouping, the group headers).
    fn visible_len(&self) -> usize {
        self.display_items(now()).len()
    }

    /// The completed, filtered, sorted-by-size results to hand back to the
    /// caller when the UI exits. The interactive sort/group/filters are
    /// view-only and deliberately do not affect this list.
    fn final_results(&self) -> Vec<DependencyDir> {
        let mut done: Vec<DependencyDir> = self
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Done(d) if d.size_bytes >= self.min_size => Some(d.clone()),
                _ => None,
            })
            .collect();
        scanner::sort_by_size(&mut done);
        if self.top > 0 && done.len() > self.top {
            done.truncate(self.top);
        }
        done
    }

    /// Ecosystems present among the discovered rows, sorted and deduplicated.
    fn ecosystems_present(&self) -> Vec<&'static str> {
        let mut ecos: Vec<&'static str> = self.rows.iter().map(|r| r.ecosystem()).collect();
        ecos.sort_unstable();
        ecos.dedup();
        ecos
    }

    fn draw(&mut self, frame: &mut Frame) {
        let chunks = Layout::vertical([
            Constraint::Length(4),
            Constraint::Min(1),
            Constraint::Length(5),
        ])
        .split(frame.area());

        self.draw_header(frame, chunks[0]);
        // `draw_table` builds the view model (filter + sort + group) once and
        // reports how many data rows it showed, so `draw_footer` doesn't have to
        // recompute it. On a large tree this alone cuts the per-frame sort/group
        // work by two thirds.
        let shown = self.draw_table(frame, chunks[1]);
        self.draw_footer(frame, chunks[2], shown);

        // An in-flight deletion takes over the screen, superseding any overlay.
        if self.delete_progress.is_some() {
            self.draw_deleting(frame);
            return;
        }

        match self.overlay {
            Overlay::Help => self.draw_help(frame),
            Overlay::Ecosystems { cursor } => self.draw_eco_panel(frame, cursor),
            Overlay::ConfirmDelete { permanent } => self.draw_confirm_delete(frame, permanent),
            Overlay::None => {}
        }
    }

    /// A spinner-and-progress modal shown while the worker thread removes the
    /// marked directories, so a slow trash operation reads as "working", not
    /// "frozen".
    fn draw_deleting(&self, frame: &mut Frame) {
        let Some(p) = &self.delete_progress else {
            return;
        };
        let verb = if p.permanent {
            "Deleting"
        } else {
            "Moving to trash"
        };
        let lines = vec![
            Line::from(vec![
                Span::styled(
                    format!(" {} ", SPINNER[self.spinner]),
                    Style::new().fg(Color::Yellow).bold(),
                ),
                Span::styled(
                    format!("{verb} {} of {}…", (p.done + 1).min(p.total), p.total),
                    Style::new().fg(Color::White).bold(),
                ),
            ]),
            Line::raw(""),
            Line::from(vec![
                Span::styled("Reclaimed so far: ", Style::new().fg(Color::Gray)),
                Span::styled(
                    self.fmt_size(p.reclaimed),
                    Style::new().fg(Color::Green).bold(),
                ),
            ]),
            Line::raw(""),
            Line::from(Span::styled(
                "Please wait — Ctrl-C to abort.",
                Style::new().fg(Color::DarkGray).italic(),
            )),
        ];
        let area = centered_rect(frame.area(), 50, lines.len() as u16 + 2);
        frame.render_widget(Clear, area);
        let para = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::new().fg(Color::Yellow))
                .title(Span::styled(
                    " Working ",
                    Style::new().fg(Color::Yellow).bold(),
                ))
                .style(Style::new().bg(Color::Rgb(24, 22, 16))),
        );
        frame.render_widget(para, area);
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let status = if self.scanning {
            format!(" {} scanning", SPINNER[self.spinner])
        } else {
            " ✓ done".to_string()
        };
        let title = Line::from(vec![
            Span::styled(
                " dda ",
                Style::new().bg(Color::Cyan).fg(Color::Black).bold(),
            ),
            Span::raw("  "),
            Span::styled(
                self.root.display().to_string(),
                Style::new().fg(Color::Gray),
            ),
            Span::raw("  "),
            Span::styled(
                status,
                Style::new().fg(if self.scanning {
                    Color::Yellow
                } else {
                    Color::Green
                }),
            ),
        ]);
        let text = Text::from(vec![title, self.filter_line()]);
        let para = Paragraph::new(text).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::new().fg(Color::DarkGray))
                .title(" dependency-dir-analyzer "),
        );
        frame.render_widget(para, area);
    }

    /// The filter bar shown as the second header line: each row filter labelled
    /// with its keyboard shortcut, so `f`/`a`/`e` (and `r` to reset) are visible
    /// without opening help. Active filters are highlighted; the reset hint lights
    /// up only when there is something to reset.
    fn filter_line(&self) -> Line<'static> {
        let eco_active = !self.disabled_ecos.is_empty();
        let eco_value = if eco_active {
            format!("{} hidden", self.disabled_ecos.len())
        } else {
            "all".to_string()
        };

        let mut spans = vec![Span::styled(
            "Filters:   ",
            Style::new().fg(Color::Gray).bold(),
        )];
        spans.extend(top_filter_chip(
            "s",
            "size",
            self.size_filter.label(),
            self.size_filter != SizeFilter::All,
        ));
        spans.push(Span::raw("  "));
        spans.extend(top_filter_chip(
            "a",
            "age",
            self.age_filter.label(),
            self.age_filter != AgeFilter::Any,
        ));
        spans.push(Span::raw("  "));
        spans.extend(top_filter_chip("e", "eco", &eco_value, eco_active));
        spans.push(Span::raw("   "));
        spans.push(Span::styled(
            "[r]",
            Style::new()
                .fg(if self.has_filters() {
                    Color::Cyan
                } else {
                    Color::DarkGray
                })
                .bold(),
        ));
        spans.push(Span::styled(
            " reset",
            Style::new().fg(if self.has_filters() {
                Color::Gray
            } else {
                Color::DarkGray
            }),
        ));
        Line::from(spans)
    }

    /// Ensure the current selection points at a valid row, snapping it back into
    /// range when the view has shrunk (e.g. after a filter change or a fold).
    /// Takes the already-computed row count so the caller doesn't rebuild the
    /// view model just to clamp.
    fn clamp_selection_to(&mut self, len: usize) {
        match self.state.selected() {
            _ if len == 0 => self.state.select(None),
            Some(i) if i >= len => self.state.select(Some(len - 1)),
            None => self.state.select(Some(0)),
            _ => {}
        }
    }

    /// Render the results table and return how many data rows were shown (used
    /// by the footer so it doesn't have to rebuild the view model).
    fn draw_table(&mut self, frame: &mut Frame, area: Rect) -> usize {
        // Build the view model once per frame; everything below reads from it.
        let items = self.display_items(now());
        let len = items.len();

        if items.is_empty() {
            self.state.select(None);
            let msg = if self.scanning {
                "Scanning…".to_string()
            } else if self.has_filters() {
                "No directories match the active filters. Press r to reset.".to_string()
            } else {
                format!(
                    "No dependency directories found under {}.",
                    self.root.display()
                )
            };
            let para = Paragraph::new(msg)
                .style(Style::new().fg(Color::Gray))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::new().fg(Color::DarkGray)),
                )
                .wrap(Wrap { trim: true });
            frame.render_widget(para, area);
            return 0;
        }

        let max_size = items
            .iter()
            .filter_map(|it| match it {
                DisplayItem::Data(Row::Done(d)) => Some(d.size_bytes),
                _ => None,
            })
            .max()
            .unwrap_or(0);

        // Bar column width derives from the available area (the leading
        // checkbox column and its spacing take a few more cells than before).
        let bar_width = (area.width as usize).saturating_sub(74).clamp(6, 24);

        // Collect into an owned Vec so the immutable borrow of `self`/`items`
        // ends before the table is rendered with `&mut self.state` below.
        let rows: Vec<TableRow> = items
            .iter()
            .map(|it| self.render_item(it, max_size, bar_width))
            .collect();
        let shown = items
            .iter()
            .filter(|it| matches!(it, DisplayItem::Data(_)))
            .count();
        // `items` is no longer used past here, so its borrow of `self` ends and
        // we can take `&mut self.state` for clamping and rendering.
        self.clamp_selection_to(len);

        let header = TableRow::new(vec![
            Cell::from(""),
            Cell::from(Text::from("Size").right_aligned()),
            Cell::from(""),
            Cell::from(Text::from("Files").right_aligned()),
            Cell::from("Modified"),
            Cell::from("Ecosystem"),
            Cell::from("Path"),
        ])
        .style(Style::new().fg(Color::Black).bg(Color::Cyan).bold());

        let widths = [
            Constraint::Length(1),
            Constraint::Length(12),
            Constraint::Length(bar_width as u16),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Length(19),
            Constraint::Min(20),
        ];

        let table = Table::new(rows, widths)
            .header(header)
            .column_spacing(2)
            .row_highlight_style(Style::new().bg(Color::Rgb(40, 40, 55)).bold())
            .highlight_symbol("▶ ")
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::new().fg(Color::DarkGray)),
            );

        frame.render_stateful_widget(table, area, &mut self.state);
        shown
    }

    fn render_item(
        &self,
        item: &DisplayItem,
        max_size: u64,
        bar_width: usize,
    ) -> TableRow<'static> {
        match item {
            DisplayItem::Header {
                ecosystem,
                total,
                count,
                selected,
                collapsed,
            } => {
                let glyph = if *collapsed { "▸" } else { "▾" };
                // Header checkbox reflects the group: full when every directory
                // is marked, partial (◐) when only some are.
                let (mark, mark_style) = if *count > 0 && *selected == *count {
                    ("✓", Style::new().fg(SELECT_FG).bold())
                } else if *selected > 0 {
                    ("◐", Style::new().fg(SELECT_FG).bold())
                } else {
                    (" ", Style::new())
                };
                TableRow::new(vec![
                    Cell::from(mark).style(mark_style),
                    Cell::from(Text::from(self.fmt_size(*total)).right_aligned())
                        .style(Style::new().fg(Color::White).bold()),
                    Cell::from(""),
                    Cell::from(
                        Text::from(format!("{count} dir{}", if *count == 1 { "" } else { "s" }))
                            .right_aligned(),
                    )
                    .style(Style::new().fg(Color::Gray)),
                    Cell::from(""),
                    Cell::from(format!("{glyph} {}", self.eco_label(ecosystem)))
                        .style(Style::new().fg(Color::Cyan).bold()),
                    Cell::from(""),
                ])
                .style(Style::new().bg(Color::Rgb(28, 30, 42)))
            }
            DisplayItem::Data(Row::Done(d)) => {
                let frac = if max_size > 0 {
                    d.size_bytes as f64 / max_size as f64
                } else {
                    0.0
                };
                let color = size_color(frac);
                let indent = if self.group { "  " } else { "" };
                let marked = self.selected.contains(&d.path);
                let mark = if marked { "✓" } else { " " };
                let row = TableRow::new(vec![
                    Cell::from(mark).style(Style::new().fg(SELECT_FG).bold()),
                    Cell::from(Text::from(self.fmt_size(d.size_bytes)).right_aligned())
                        .style(Style::new().fg(color).bold()),
                    Cell::from(build_bar(frac, bar_width)).style(Style::new().fg(color)),
                    Cell::from(Text::from(group_thousands(d.file_count)).right_aligned())
                        .style(Style::new().fg(Color::Gray)),
                    Cell::from(relative_age(d.modified))
                        .style(Style::new().fg(age_color(d.modified))),
                    Cell::from(self.eco_label(d.ecosystem)).style(Style::new().fg(Color::Cyan)),
                    Cell::from(format!("{indent}{}", self.display_path(&d.path))),
                ]);
                // Tint marked rows so the deletion set stands out even when the
                // cursor is elsewhere.
                if marked {
                    row.style(Style::new().bg(SELECT_BG))
                } else {
                    row
                }
            }
            DisplayItem::Data(Row::Pending { path, ecosystem }) => TableRow::new(vec![
                Cell::from(" "),
                Cell::from(format!("{} sizing…", SPINNER[self.spinner]))
                    .style(Style::new().fg(Color::DarkGray)),
                Cell::from(build_bar(0.0, bar_width)),
                Cell::from(Text::from("—").right_aligned()).style(Style::new().fg(Color::DarkGray)),
                Cell::from("—").style(Style::new().fg(Color::DarkGray)),
                Cell::from(self.eco_label(ecosystem)).style(Style::new().fg(Color::DarkGray)),
                Cell::from(self.display_path(path)).style(Style::new().fg(Color::DarkGray)),
            ]),
        }
    }

    fn has_filters(&self) -> bool {
        self.size_filter != SizeFilter::All
            || self.age_filter != AgeFilter::Any
            || !self.disabled_ecos.is_empty()
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect, shown: usize) {
        let now = now();
        let sized = self
            .rows
            .iter()
            .filter(|r| matches!(r, Row::Done(_)))
            .count();
        // Total reflects what is currently shown (filtered), so the header chip
        // tracks the reclaimable space for the active view.
        let total: u64 = self
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Done(d) if self.passes(d, now) => Some(d.size_bytes),
                _ => None,
            })
            .sum();

        // The deletion set gets its own prominent, attention-grabbing chip right
        // after the reclaimable-space total.
        let (sel_count, sel_bytes) = self.selected_total();
        let mut chips = vec![Span::styled(
            format!(" {} ", self.fmt_size(total)),
            Style::new().bg(Color::Green).fg(Color::Black).bold(),
        )];
        if sel_count > 0 {
            chips.push(Span::raw(" "));
            chips.push(Span::styled(
                format!(" ✗ {sel_count} marked · {} ", self.fmt_size(sel_bytes)),
                Style::new().bg(Color::Red).fg(Color::White).bold(),
            ));
        }
        chips.extend([
            Span::raw("  "),
            Span::styled(
                format!("{shown} shown · {sized} sized · {} found", self.rows.len()),
                Style::new().fg(Color::Gray),
            ),
            Span::raw("   "),
            Span::styled(
                format!("sort:{}", self.sort.label()),
                Style::new().fg(Color::Magenta),
            ),
            Span::raw("  "),
            Span::styled(
                format!("group:{}", if self.group { "eco" } else { "off" }),
                Style::new().fg(if self.group {
                    Color::Cyan
                } else {
                    Color::DarkGray
                }),
            ),
            Span::raw("  "),
            Span::styled(
                format!("unit:{}", self.size_unit.label()),
                Style::new().fg(Color::Blue),
            ),
            Span::raw("  "),
            Span::styled(
                format!("icons:{}", self.icon_style.label()),
                Style::new().fg(Color::Blue),
            ),
        ]);
        if self.warnings > 0 {
            chips.push(Span::raw("  "));
            chips.push(Span::styled(
                format!("⚠ {}", self.warnings),
                Style::new().fg(Color::Yellow),
            ));
        }
        // The transient result of the last action (e.g. a deletion) trails the
        // chips until the selection changes again.
        if let Some(msg) = &self.action {
            chips.push(Span::raw("   "));
            chips.push(Span::styled(
                msg.clone(),
                Style::new().fg(Color::Green).italic(),
            ));
        }

        // Two hint rows: view controls on top, the selection/deletion verbs
        // (which the footer chip already advertises with "press d to delete")
        // below.
        let hints_view = Line::from(vec![Span::styled(
            "↑/↓ move   S sort   g group   u unit   i icons   ↵ fold",
            Style::new().fg(Color::DarkGray),
        )]);
        let delete_hint = if sel_count > 0 {
            Span::styled("d delete", Style::new().fg(SELECT_FG).bold())
        } else {
            Span::styled("d delete", Style::new().fg(Color::DarkGray))
        };
        let hints_action = Line::from(vec![
            Span::styled(
                "space/x mark   A all   c clear   ",
                Style::new().fg(Color::DarkGray),
            ),
            delete_hint,
            Span::styled("   ? help   q quit", Style::new().fg(Color::DarkGray)),
        ]);

        let text = Text::from(vec![Line::from(chips), hints_view, hints_action]);
        let para = Paragraph::new(text).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::new().fg(Color::DarkGray)),
        );
        frame.render_widget(para, area);
    }

    fn draw_help(&self, frame: &mut Frame) {
        let lines = vec![
            help_line("↑ / ↓ , j / k", "move selection"),
            help_line("PgUp / PgDn", "jump 10 rows"),
            help_line("Home / End", "first / last row"),
            Line::raw(""),
            help_line("S", "cycle sort (size · modified · name · files)"),
            help_line("u", "toggle size units (MB · auto)"),
            help_line("i", "cycle ecosystem icons (emoji · nerd · off)"),
            help_line("g", "toggle grouping by ecosystem"),
            help_line("Enter / Space", "fold/unfold selected group"),
            help_line("s", "cycle minimum-size filter"),
            help_line("a", "cycle \"modified older than\" filter"),
            help_line("e", "open ecosystem filter (multi-select)"),
            help_line("r", "reset all filters & grouping"),
            Line::raw(""),
            help_line("Space / x", "mark/unmark for deletion (group on a header)"),
            help_line("A", "mark/unmark all visible"),
            help_line("c", "clear the selection"),
            help_line("d / Del", "delete marked dirs (asks to confirm)"),
            Line::raw(""),
            help_line("?", "toggle this help"),
            help_line("q / Esc", "quit"),
            Line::raw(""),
            Line::from(Span::styled(
                "Modified colour: green = recent · red = old",
                Style::new().fg(Color::DarkGray).italic(),
            )),
        ];

        let area = centered_rect(frame.area(), 60, lines.len() as u16 + 2);
        frame.render_widget(Clear, area);
        let para = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::new().fg(Color::Cyan))
                .title(Span::styled(
                    " Keyboard shortcuts ",
                    Style::new().fg(Color::Cyan).bold(),
                ))
                .style(Style::new().bg(Color::Rgb(20, 22, 32))),
        );
        frame.render_widget(para, area);
    }

    fn draw_eco_panel(&self, frame: &mut Frame, cursor: usize) {
        let ecos = self.ecosystems_present();

        let lines: Vec<Line> = if ecos.is_empty() {
            vec![Line::from(Span::styled(
                "No ecosystems discovered yet…",
                Style::new().fg(Color::Gray),
            ))]
        } else {
            ecos.iter()
                .enumerate()
                .map(|(i, eco)| {
                    let enabled = !self.disabled_ecos.contains(*eco);
                    let marker = if i == cursor { "▶ " } else { "  " };
                    let check = if enabled { "[x]" } else { "[ ]" };
                    let row_style = if i == cursor {
                        Style::new().bg(Color::Rgb(40, 40, 55)).bold()
                    } else {
                        Style::new()
                    };
                    Line::from(vec![
                        Span::styled(marker, Style::new().fg(Color::Cyan)),
                        Span::styled(
                            format!("{check} "),
                            Style::new().fg(if enabled {
                                Color::Green
                            } else {
                                Color::DarkGray
                            }),
                        ),
                        Span::styled(
                            self.eco_label(eco),
                            Style::new().fg(if enabled {
                                Color::White
                            } else {
                                Color::DarkGray
                            }),
                        ),
                    ])
                    .style(row_style)
                })
                .collect()
        };

        let mut content = lines;
        content.push(Line::raw(""));
        content.push(Line::from(Span::styled(
            "space toggle · a all/none · Esc close",
            Style::new().fg(Color::DarkGray).italic(),
        )));

        let area = centered_rect(frame.area(), 46, content.len() as u16 + 2);
        frame.render_widget(Clear, area);
        let para = Paragraph::new(content).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::new().fg(Color::Cyan))
                .title(Span::styled(
                    " Filter by ecosystem ",
                    Style::new().fg(Color::Cyan).bold(),
                ))
                .style(Style::new().bg(Color::Rgb(20, 22, 32))),
        );
        frame.render_widget(para, area);
    }

    /// The deletion confirmation modal: lists the marked directories, the total
    /// space to be reclaimed, the current mode (trash vs permanent) and the
    /// keys. The accent colour and wording shift to red when the irreversible
    /// permanent mode is selected.
    fn draw_confirm_delete(&self, frame: &mut Frame, permanent: bool) {
        let dirs = self.selected_dirs();
        let (count, bytes) = self.selected_total();

        // Show a bounded list of paths so a huge selection can't overflow the
        // screen; the rest is summarised on a trailing line.
        const MAX_LISTED: usize = 10;
        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(Span::styled(
            format!(
                "About to remove {count} director{}:",
                if count == 1 { "y" } else { "ies" }
            ),
            Style::new().fg(Color::White).bold(),
        )));
        lines.push(Line::raw(""));
        for (path, size) in dirs.iter().take(MAX_LISTED) {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {:>9}  ", self.fmt_size(*size)),
                    Style::new().fg(Color::Gray),
                ),
                Span::styled(self.display_path(path), Style::new().fg(Color::White)),
            ]));
        }
        if dirs.len() > MAX_LISTED {
            lines.push(Line::from(Span::styled(
                format!("  … and {} more", dirs.len() - MAX_LISTED),
                Style::new().fg(Color::DarkGray).italic(),
            )));
        }

        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled("Reclaims ", Style::new().fg(Color::Gray)),
            Span::styled(self.fmt_size(bytes), Style::new().fg(Color::Green).bold()),
            Span::styled(" of disk space.", Style::new().fg(Color::Gray)),
        ]));
        lines.push(Line::raw(""));

        let (accent, mode_line) = if permanent {
            (
                Color::Red,
                Line::from(vec![
                    Span::styled("Mode: ", Style::new().fg(Color::Gray)),
                    Span::styled(
                        " PERMANENT DELETE ",
                        Style::new().bg(Color::Red).fg(Color::White).bold(),
                    ),
                    Span::styled(
                        "  irreversible — files are not recoverable",
                        Style::new().fg(Color::Rgb(255, 150, 150)),
                    ),
                ]),
            )
        } else {
            (
                Color::Yellow,
                Line::from(vec![
                    Span::styled("Mode: ", Style::new().fg(Color::Gray)),
                    Span::styled(
                        " Move to Trash ",
                        Style::new().bg(Color::Yellow).fg(Color::Black).bold(),
                    ),
                    Span::styled(
                        "  recoverable from your system trash",
                        Style::new().fg(Color::Rgb(210, 210, 160)),
                    ),
                ]),
            )
        };
        lines.push(mode_line);
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled("  y/↵ ", Style::new().fg(accent).bold()),
            Span::styled("confirm    ", Style::new().fg(Color::Gray)),
            Span::styled("t ", Style::new().fg(Color::Cyan).bold()),
            Span::styled("toggle mode    ", Style::new().fg(Color::Gray)),
            Span::styled("n/Esc ", Style::new().fg(Color::Cyan).bold()),
            Span::styled("cancel", Style::new().fg(Color::Gray)),
        ]));

        let area = centered_rect(frame.area(), 70, lines.len() as u16 + 2);
        frame.render_widget(Clear, area);
        let title = if permanent {
            " ⚠ Permanently delete? "
        } else {
            " Move to trash? "
        };
        let para = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::new().fg(accent))
                .title(Span::styled(title, Style::new().fg(accent).bold()))
                .style(Style::new().bg(Color::Rgb(24, 18, 22))),
        );
        frame.render_widget(para, area);
    }

    /// The ecosystem label, prefixed with its icon unless icons are off.
    fn eco_label(&self, ecosystem: &str) -> String {
        let icon = self.icon_style.icon(ecosystem);
        if icon.is_empty() {
            ecosystem.to_string()
        } else {
            format!("{icon} {ecosystem}")
        }
    }

    /// Render a byte count for the size column, honouring the current unit mode.
    fn fmt_size(&self, bytes: u64) -> String {
        match self.size_unit {
            SizeUnit::Megabytes => megabytes_size(bytes),
            SizeUnit::Human => human_size(bytes),
        }
    }

    /// Display a path relative to the scan root when possible, so the table
    /// stays readable for deep trees.
    fn display_path(&self, path: &Path) -> String {
        match path.strip_prefix(&self.root) {
            Ok(rel) if rel.as_os_str().is_empty() => ".".to_string(),
            Ok(rel) => rel.display().to_string(),
            Err(_) => path.display().to_string(),
        }
    }
}

fn now() -> SystemTime {
    SystemTime::now()
}

/// Remove a directory tree, either by moving it to the OS trash (recoverable)
/// or deleting it permanently. Errors are reported as a string so the caller can
/// surface them in the UI without depending on the `trash` error type.
fn delete_path(path: &Path, permanent: bool) -> std::result::Result<(), String> {
    if permanent {
        std::fs::remove_dir_all(path).map_err(|e| e.to_string())
    } else {
        trash::delete(path).map_err(|e| e.to_string())
    }
}

/// Sum of the done-row sizes in a group (pending rows contribute nothing).
fn group_total(rows: &[&Row]) -> u64 {
    rows.iter()
        .filter_map(|r| match r {
            Row::Done(d) => Some(d.size_bytes),
            _ => None,
        })
        .sum()
}

/// One filter entry for the top header bar: the bracketed keyboard shortcut, the
/// filter's name, and its current value as a chip (highlighted when the filter is
/// doing something).
fn top_filter_chip(key: &str, label: &str, value: &str, active: bool) -> Vec<Span<'static>> {
    let value_style = if active {
        Style::new().fg(Color::Black).bg(Color::Yellow).bold()
    } else {
        Style::new().fg(Color::DarkGray)
    };
    vec![
        Span::styled(format!("[{key}] "), Style::new().fg(Color::Cyan).bold()),
        Span::styled(format!("{label} "), Style::new().fg(Color::Gray)),
        Span::styled(format!(" {value} "), value_style),
    ]
}

fn help_line(keys: &'static str, desc: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {keys:<16}"), Style::new().fg(Color::Cyan).bold()),
        Span::styled(desc, Style::new().fg(Color::Gray)),
    ])
}

/// A rectangle of the given width-percentage and fixed `height`, centred within
/// `area` (clamped so it always fits).
fn centered_rect(area: Rect, percent_x: u16, height: u16) -> Rect {
    let width = (area.width * percent_x / 100).clamp(20, area.width);
    let height = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

/// A horizontal bar `width` cells wide filled to `frac` (0..=1) using Unicode
/// eighth-blocks for sub-cell precision.
fn build_bar(frac: f64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    const PARTIAL: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let frac = frac.clamp(0.0, 1.0);
    let eighths = (frac * width as f64 * 8.0).round() as usize;
    let full = eighths / 8;
    let rem = eighths % 8;

    let mut bar = String::with_capacity(width);
    for _ in 0..full.min(width) {
        bar.push('█');
    }
    if rem > 0 && full < width {
        bar.push(PARTIAL[rem]);
    }
    while bar.chars().count() < width {
        bar.push(' ');
    }
    bar
}

/// Interpolate a colour from green (small) through yellow to red (large).
fn size_color(frac: f64) -> Color {
    let f = frac.clamp(0.0, 1.0);
    let (r, g) = if f < 0.5 {
        ((f * 2.0 * 255.0) as u8, 220)
    } else {
        (235, ((1.0 - f) * 2.0 * 220.0) as u8)
    };
    Color::Rgb(r, g, 70)
}

/// Colour the age column on a green→red scale: recently-modified directories are
/// green and old ones are red.
fn age_color(modified: Option<SystemTime>) -> Color {
    let Some(modified) = modified else {
        return Color::DarkGray;
    };
    let days = SystemTime::now()
        .duration_since(modified)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0);
    match days {
        0..=6 => Color::Rgb(80, 220, 100),    // fresh — green
        7..=29 => Color::Rgb(160, 210, 90),   // lime
        30..=89 => Color::Rgb(220, 200, 80),  // yellow
        90..=364 => Color::Rgb(230, 150, 60), // orange
        _ => Color::Rgb(230, 80, 70),         // old — red
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_fills_proportionally() {
        assert_eq!(build_bar(0.0, 4), "    ");
        assert_eq!(build_bar(1.0, 4), "████");
        assert_eq!(build_bar(0.5, 4), "██  ");
        // A bar is always exactly `width` display cells wide.
        assert_eq!(build_bar(0.3, 10).chars().count(), 10);
    }

    #[test]
    fn bar_zero_width_is_empty() {
        assert_eq!(build_bar(0.7, 0), "");
    }

    #[test]
    fn sized_event_updates_matching_row_via_index() {
        // A discovered-then-sized round trip must land on the right row, and the
        // path→row index must survive a row being removed mid-scan (which shifts
        // every later position). Regression guard for the O(1) index in `apply`.
        let mut app = App::new(PathBuf::from("/tmp"), 0, 0, ViewState::default());

        for name in ["a", "b", "c"] {
            app.apply(ScanEvent::Discovered {
                path: PathBuf::from(format!("/tmp/{name}")),
                ecosystem: "Node.js",
            });
        }

        // Remove the first row; the remaining rows shift down by one.
        app.remove_rows(&[PathBuf::from("/tmp/a")]);

        // Sizing the last-discovered directory must update *its* row, not a
        // stale position left over from before the removal.
        let sized = DependencyDir {
            path: PathBuf::from("/tmp/c"),
            ecosystem: "Node.js",
            name: "node_modules",
            size_bytes: 42,
            file_count: 3,
            modified: None,
        };
        app.apply(ScanEvent::Sized(sized));

        // Exactly the two surviving rows exist, and only "c" is Done with the
        // right size — nothing was duplicated or mis-targeted.
        assert_eq!(app.rows.len(), 2);
        let c = app
            .rows
            .iter()
            .find(|r| r.path() == Path::new("/tmp/c"))
            .expect("c row present");
        match c {
            Row::Done(d) => assert_eq!(d.size_bytes, 42),
            Row::Pending { .. } => panic!("c should be sized"),
        }
        assert!(matches!(
            app.rows.iter().find(|r| r.path() == Path::new("/tmp/b")),
            Some(Row::Pending { .. })
        ));
    }

    #[test]
    fn age_filter_older_than_semantics() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        let mut app = App::new(PathBuf::from("/tmp"), 0, 0, ViewState::default());
        app.age_filter = AgeFilter::Month1;
        // Modified 60 days ago passes ">1mo"; 1 day ago does not.
        let old = Some(now - Duration::from_secs(60 * 86_400));
        let recent = Some(now - Duration::from_secs(86_400));
        assert!(app.age_passes(old, now));
        assert!(!app.age_passes(recent, now));
        // Unknown timestamps are excluded while an age filter is active.
        assert!(!app.age_passes(None, now));
        // With no age filter, everything (including unknown) passes.
        app.age_filter = AgeFilter::Any;
        assert!(app.age_passes(None, now));
    }

    fn done_row(eco: &'static str, name: &'static str, size: u64) -> Row {
        Row::Done(DependencyDir {
            path: PathBuf::from(format!("/tmp/{name}")),
            ecosystem: eco,
            name,
            size_bytes: size,
            file_count: 1,
            modified: None,
        })
    }

    #[test]
    fn collapsing_a_group_hides_only_its_rows() {
        let mut app = App::new(PathBuf::from("/tmp"), 0, 0, ViewState::default());
        app.scanning = false;
        app.group = true;
        app.rows.push(done_row("Node.js", "node_modules", 100));
        app.rows.push(done_row("Rust / Maven", "target", 200));

        let count = |app: &App| {
            let items = app.display_items(now());
            let headers = items
                .iter()
                .filter(|it| matches!(it, DisplayItem::Header { .. }))
                .count();
            let data = items
                .iter()
                .filter(|it| matches!(it, DisplayItem::Data(_)))
                .count();
            (headers, data)
        };

        // Two groups, each with its single directory shown.
        assert_eq!(count(&app), (2, 2));

        // Collapsing one group keeps both headers but hides that group's row.
        app.collapsed_ecos.insert("Rust / Maven".to_string());
        assert_eq!(count(&app), (2, 1));

        // Reset restores everything.
        app.reset_filters();
        assert!(!app.group);
        assert_eq!(count(&app), (0, 2));
    }

    #[test]
    fn selected_total_sums_only_marked_rows() {
        let mut app = App::new(PathBuf::from("/tmp"), 0, 0, ViewState::default());
        app.rows.push(done_row("Node.js", "node_modules", 100));
        app.rows.push(done_row("Rust / Maven", "target", 250));

        assert_eq!(app.selected_total(), (0, 0));

        app.selected.insert(PathBuf::from("/tmp/target"));
        assert_eq!(app.selected_total(), (1, 250));

        app.selected.insert(PathBuf::from("/tmp/node_modules"));
        assert_eq!(app.selected_total(), (2, 350));
    }

    #[test]
    fn toggle_group_selection_marks_then_clears_whole_group() {
        let mut app = App::new(PathBuf::from("/tmp"), 0, 0, ViewState::default());
        app.scanning = false;
        app.rows.push(done_row("Node.js", "a", 10));
        app.rows.push(done_row("Node.js", "b", 20));
        app.rows.push(done_row("Rust / Maven", "target", 30));

        // First toggle marks every Node.js directory, leaving the other alone.
        app.toggle_group_selection("Node.js");
        assert_eq!(app.selected_total(), (2, 30));
        assert!(!app.selected.contains(&PathBuf::from("/tmp/target")));

        // Toggling again clears the group.
        app.toggle_group_selection("Node.js");
        assert_eq!(app.selected_total(), (0, 0));
    }

    #[test]
    fn select_all_visible_skips_filtered_out_rows() {
        let mut app = App::new(PathBuf::from("/tmp"), 0, 0, ViewState::default());
        app.scanning = false;
        app.rows.push(done_row("Node.js", "big", 5_000_000));
        app.rows.push(done_row("Node.js", "small", 10));
        // A ≥1 MB filter hides the small directory from the view.
        app.size_filter = SizeFilter::Mb1;

        app.toggle_select_all_visible();
        // Only the directory that passes the active filter is marked.
        assert_eq!(app.selected_total(), (1, 5_000_000));
        assert!(!app.selected.contains(&PathBuf::from("/tmp/small")));
    }

    #[test]
    fn permanent_deletion_removes_dir_and_prunes_row() {
        let tmp = std::env::temp_dir().join(format!("dda-del-test-{}", std::process::id()));
        let victim = tmp.join("node_modules");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("a.js"), b"hello").unwrap();
        assert!(victim.exists());

        let mut app = App::new(tmp.clone(), 0, 0, ViewState::default());
        app.scanning = false;
        app.rows.push(Row::Done(DependencyDir {
            path: victim.clone(),
            ecosystem: "Node.js",
            name: "node_modules",
            size_bytes: 5,
            file_count: 1,
            modified: None,
        }));
        app.selected.insert(victim.clone());

        // Deletion runs on a worker thread; pump the drain until it finishes.
        app.start_deletion(true);
        for _ in 0..1000 {
            app.drain_deletion();
            if app.delete_progress.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        // The directory is gone, the row is pruned and the selection is empty.
        assert!(app.delete_progress.is_none(), "deletion did not finish");
        assert!(!victim.exists());
        assert!(app.rows.is_empty());
        assert!(app.selected.is_empty());
        assert!(app
            .action
            .as_deref()
            .unwrap()
            .contains("Deleted 1 directory"));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
