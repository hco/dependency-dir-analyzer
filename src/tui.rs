//! Interactive, live-updating terminal UI built on `ratatui` + `crossterm`.
//!
//! The scan runs on a background thread and streams [`ScanEvent`]s back over a
//! channel; the UI thread drains them every tick, so dependency directories
//! appear as soon as they are discovered and their sizes fill in as the
//! parallel sizing completes. The user can scroll, re-sort, group by ecosystem,
//! filter, mark directories for deletion, and quit; on exit the final
//! largest-first results (minus anything deleted) are returned so the caller can
//! leave a static summary in the terminal scrollback.
//!
//! The UI is mouse-aware as well as keyboard-driven: the scroll wheel moves the
//! selection, a left click highlights the row under the cursor, and the header
//! filter chips, footer status chips and overlay controls are all clickable,
//! each bound to the same action as its keyboard shortcut.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use ratatui::layout::Position;
use ratatui::prelude::*;
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Paragraph, Row as TableRow, Table, TableState, Wrap,
};
use ratatui::DefaultTerminal;

use crate::pnpm;
use crate::report::{group_thousands, human_size, megabytes_size, relative_age};
use crate::scanner::DirKind;
use crate::scanner::{self, DependencyDir, ScanEvent, ScanOptions};
use crate::state::{
    self, AgeFilter, IconStyle, SizeFilter, SizeMode, SizeUnit, SortKey, StoreAction, ViewState,
};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Redraw cadence while something is animating (the scan or a deletion is
/// running): fast enough for a smooth spinner and live-streaming rows.
const TICK: Duration = Duration::from_millis(100);
/// Once the scan is finished and nothing is animating the screen is static, so
/// the event loop blocks for up to this long waiting for input instead of
/// busy-redrawing every `TICK` — keeping idle CPU usage near zero. A resize or
/// key press wakes it sooner.
const IDLE_TICK: Duration = Duration::from_secs(3600);

/// How many rows one scroll-wheel notch moves the selection, matching the
/// conventional "three lines per notch" of most terminals.
const SCROLL_STEP: isize = 3;

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
    size_mode: Option<SizeMode>,
    mouse: bool,
) -> Result<Vec<DependencyDir>> {
    let (tx, rx) = mpsc::channel();
    let root_buf = root.to_path_buf();
    // Lets us tell the scan to stop when the user quits, so we don't block on
    // `join` below while it finishes sizing a tree nobody is looking at anymore.
    let cancel = Arc::new(AtomicBool::new(false));
    let scan_cancel = Arc::clone(&cancel);
    let scan_thread = thread::spawn(move || {
        scanner::scan_streaming(&root_buf, &opts, tx, scan_cancel);
    });

    // Restore the view preferences last used for this directory. A `--icons`
    // flag overrides the remembered icon style just for this run.
    let mut view = state::load(root);
    if let Some(style) = icons {
        view.icon_style = style;
    }
    if let Some(mode) = size_mode {
        view.size_mode = mode;
    }

    let mut terminal = ratatui::init();
    // Capture the mouse so the UI can react to clicks and the scroll wheel,
    // unless the user opted out. Capture takes over the mouse, suppressing the
    // terminal's native click-drag text selection and scrollback while the TUI
    // runs (hold Shift to bypass in most terminals) — which is exactly why
    // `--no-mouse` / `DDA_NO_MOUSE` / the `mouse` config setting exist. Enabling
    // is best-effort: a terminal that rejects it just stays keyboard-only.
    if mouse {
        let _ = execute!(std::io::stdout(), EnableMouseCapture);
    }
    let mut app = App::new(root.to_path_buf(), top, min_size, view);
    app.mouse_enabled = mouse;
    let settings = state::settings();
    app.trash_notice_pending = !settings.trash_removal_acknowledged;
    app.store_action = settings.store_action;
    app.follow_symlinks = opts.follow_symlinks;
    app.snapshots = crate::usage::has_local_snapshots();
    let result = app.event_loop(&mut terminal, rx);
    if mouse {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
    }
    ratatui::restore();

    // Tell an unfinished scan to stop before we wait on it, so quitting mid-scan
    // returns right away instead of blocking until the whole tree is sized.
    cancel.store(true, Ordering::Relaxed);
    // The scan thread finishes once its `tx` is dropped at the end of
    // `scan_streaming`; joining keeps us from leaking it.
    let _ = scan_thread.join();

    // Remember the current view for next time (best-effort; only on clean exit).
    if result.is_ok() {
        state::save(root, &app.view_state());
        // The store action is a machine-wide choice, saved with the settings.
        let mut settings = state::settings();
        if settings.store_action != app.store_action {
            settings.store_action = app.store_action;
            state::save_settings(&settings);
        }
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
    /// One-off notice for users upgrading from a version that moved deletions
    /// to the system trash: shown before their first confirmation dialog so
    /// the change to permanent deletion doesn't catch them out.
    TrashRemovedNotice,
    /// Confirmation gate before permanently deleting the marked directories.
    ConfirmDelete,
}

/// The result of trying to remove one directory, sent back from the worker
/// thread as it goes so the UI can update without blocking.
enum DeleteOutcome {
    Removed {
        path: PathBuf,
        size: u64,
    },
    /// A pnpm store was pruned in place; `new` is it re-measured afterwards.
    Pruned {
        old: DependencyDir,
        new: DependencyDir,
    },
    Failed {
        path: PathBuf,
        /// What was attempted (`delete` or `prune`), for the warning.
        verb: &'static str,
        error: String,
    },
}

/// Live state of an in-flight deletion, driving the progress modal.
struct DeleteProgress {
    done: usize,
    total: usize,
    reclaimed: u64,
    failures: usize,
    /// How many of `done` were stores pruned rather than removed.
    pruned: usize,
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

/// A control the user can click. Regions carrying one of these are registered
/// afresh every frame (see [`App::clicks`]) and hit-tested on a left click. Each
/// variant mirrors a keyboard action so the two input paths stay in lock-step.
#[derive(Clone, Copy)]
enum ClickAction {
    // Header filter bar.
    CycleSize,
    CycleAge,
    OpenEco,
    ResetFilters,
    // Footer status chips.
    CycleSort,
    ToggleGroup,
    CycleUnit,
    CycleIcons,
    ToggleSizeMode,
    ToggleStoreAction,
    // Ecosystem overlay.
    EcoToggle(usize),
    EcoAllNone,
    // Trash-removal notice.
    NoticeContinue,
    // Confirm-delete overlay (the cancel button is shared with the notice).
    ConfirmYes,
    ConfirmCancel,
}

/// A single line of styled spans that also remembers which sub-runs are
/// clickable. Widths are tracked with [`Span::width`] as spans are appended, so
/// callers get the exact horizontal extent of each clickable run without
/// re-measuring the finished line. Offsets are relative to the line start; the
/// caller translates them to screen coordinates once it knows the origin.
#[derive(Default)]
struct ClickLine {
    spans: Vec<Span<'static>>,
    /// `(start_offset, width, action)` for each clickable run, in cells.
    regions: Vec<(u16, u16, ClickAction)>,
    width: u16,
}

impl ClickLine {
    /// Append non-interactive spans.
    fn text(&mut self, span: Span<'static>) {
        self.width += span.width() as u16;
        self.spans.push(span);
    }

    /// Append a run of spans as one clickable target for `action`.
    fn button(&mut self, spans: Vec<Span<'static>>, action: ClickAction) {
        let start = self.width;
        let w: u16 = spans.iter().map(|s| s.width() as u16).sum();
        self.regions.push((start, w, action));
        self.width += w;
        self.spans.extend(spans);
    }

    /// Push this line's clickable runs into `sink`, translated so the line's
    /// left edge sits at (`x`, `y`) on screen.
    fn register(&self, sink: &mut Vec<(Rect, ClickAction)>, x: u16, y: u16) {
        for &(start, w, action) in &self.regions {
            sink.push((
                Rect {
                    x: x.saturating_add(start),
                    y,
                    width: w,
                    height: 1,
                },
                action,
            ));
        }
    }

    fn into_line(self) -> Line<'static> {
        Line::from(self.spans)
    }
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
    /// Which byte count the size column shows, sorts and filters by. Deletion
    /// figures always use the freeable bytes regardless.
    size_mode: SizeMode,
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
    /// Clickable regions registered during the most recent [`App::draw`]. A left
    /// click is hit-tested against these (topmost/last-drawn wins by draw order,
    /// gated by the active overlay). Rebuilt every frame.
    clicks: Vec<(Rect, ClickAction)>,
    /// The table's data-row area (inside the border, below the column header)
    /// from the last draw, so a click can be mapped to a row via the current
    /// scroll offset. `None` when the table isn't showing rows.
    table_area: Option<Rect>,
    /// Whether mouse capture is active this run (see [`run`]). Only affects what
    /// the help overlay advertises — with capture off no mouse events arrive, so
    /// the handlers are simply dormant. Defaults to `true`; [`run`] sets it.
    mouse_enabled: bool,
    /// Whether this user still has to be told that deletions no longer go to
    /// the system trash (see [`state::Settings::trash_removal_acknowledged`]).
    /// Cleared, and persisted, on their first confirmed deletion. Defaults to
    /// `false`; [`run`] sets it.
    trash_notice_pending: bool,
    /// Whether local Time Machine snapshots exist, which keep deleted data
    /// on disk until they expire. Defaults to `false`; [`run`] sets it.
    snapshots: bool,
    /// What deleting a marked pnpm store does. Remembered globally; [`run`]
    /// loads it from the settings.
    store_action: StoreAction,
    /// Whether the scan follows symlinks, so a pruned store is re-measured
    /// the same way it was measured.
    follow_symlinks: bool,
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
            size_mode: view.size_mode,
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
            clicks: Vec::new(),
            table_area: None,
            mouse_enabled: true,
            trash_notice_pending: false,
            snapshots: false,
            store_action: StoreAction::default(),
            follow_symlinks: false,
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
            size_mode: self.size_mode,
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
                match event::read()? {
                    Event::Key(key) => {
                        if key.kind == KeyEventKind::Press
                            && self.handle_key(key.code, key.modifiers)
                        {
                            return Ok(());
                        }
                    }
                    Event::Mouse(mouse) => self.handle_mouse(mouse),
                    _ => {}
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
            Overlay::TrashRemovedNotice => {
                self.handle_notice_key(code);
                false
            }
            Overlay::ConfirmDelete => {
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
            KeyCode::Char('f') => self.size_mode = self.size_mode.toggle(),
            KeyCode::Char('p') => self.store_action = self.store_action.toggle(),
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

    fn handle_notice_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.overlay = Overlay::ConfirmDelete;
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => {
                self.overlay = Overlay::None;
            }
            _ => {}
        }
    }

    fn handle_confirm_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.confirm_deletion(),
            KeyCode::Char('p') => self.store_action = self.store_action.toggle(),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => {
                self.overlay = Overlay::None;
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

    /// Dispatch a mouse event. Scroll moves the selection (or the ecosystem
    /// cursor when that panel is open); a left click is hit-tested against the
    /// regions registered on the last frame.
    fn handle_mouse(&mut self, mouse: MouseEvent) {
        // Mirror the keyboard rule: while a deletion runs the UI is read-only.
        if self.delete_progress.is_some() {
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollDown => self.scroll(1),
            MouseEventKind::ScrollUp => self.scroll(-1),
            MouseEventKind::Down(MouseButton::Left) => self.handle_click(mouse.column, mouse.row),
            _ => {}
        }
    }

    /// A scroll-wheel notch: move the ecosystem-panel cursor by one when it is
    /// open, otherwise move the table selection by [`SCROLL_STEP`] rows.
    fn scroll(&mut self, delta: isize) {
        if matches!(self.overlay, Overlay::Ecosystems { .. }) {
            let len = self.ecosystems_present().len();
            if let Overlay::Ecosystems { cursor } = &mut self.overlay {
                if len > 0 {
                    *cursor = (*cursor as isize + delta).clamp(0, len as isize - 1) as usize;
                }
            }
        } else if matches!(self.overlay, Overlay::None) {
            self.move_selection(delta * SCROLL_STEP);
        }
    }

    /// Handle a left click at (`col`, `row`). Help dismisses on any click;
    /// otherwise the first registered region that both contains the point and is
    /// valid for the current overlay wins, and failing that a click inside the
    /// table selects the row under the cursor.
    fn handle_click(&mut self, col: u16, row: u16) {
        if let Overlay::Help = self.overlay {
            self.overlay = Overlay::None;
            return;
        }
        let point = Position { x: col, y: row };
        // Only consider regions allowed in the current mode, so chrome drawn
        // behind an open overlay isn't triggered through the modal.
        let hit = self
            .clicks
            .iter()
            .find(|(rect, action)| rect.contains(point) && self.action_allowed(*action))
            .map(|(_, action)| *action);
        if let Some(action) = hit {
            self.apply_click_action(action);
            return;
        }
        // No chrome hit: in the main view, a click in the table highlights the
        // row under the cursor.
        if matches!(self.overlay, Overlay::None) {
            if let Some(index) = self.row_at(col, row) {
                self.state.select(Some(index));
            }
        }
    }

    /// Whether `action` may fire given the overlay currently on top.
    fn action_allowed(&self, action: ClickAction) -> bool {
        use ClickAction::*;
        match self.overlay {
            Overlay::None => matches!(
                action,
                CycleSize
                    | CycleAge
                    | OpenEco
                    | ResetFilters
                    | CycleSort
                    | ToggleGroup
                    | CycleUnit
                    | CycleIcons
                    | ToggleSizeMode
                    | ToggleStoreAction
            ),
            Overlay::Ecosystems { .. } => matches!(action, EcoToggle(_) | EcoAllNone),
            Overlay::TrashRemovedNotice => matches!(action, NoticeContinue | ConfirmCancel),
            Overlay::ConfirmDelete => {
                matches!(action, ConfirmYes | ConfirmCancel | ToggleStoreAction)
            }
            Overlay::Help => false,
        }
    }

    /// Perform the action bound to a clicked control.
    fn apply_click_action(&mut self, action: ClickAction) {
        match action {
            ClickAction::CycleSize => self.size_filter = self.size_filter.next(),
            ClickAction::CycleAge => self.age_filter = self.age_filter.next(),
            ClickAction::OpenEco => self.overlay = Overlay::Ecosystems { cursor: 0 },
            ClickAction::ResetFilters => self.reset_filters(),
            ClickAction::CycleSort => self.sort = self.sort.next(),
            ClickAction::ToggleGroup => self.group = !self.group,
            ClickAction::CycleUnit => self.size_unit = self.size_unit.toggle(),
            ClickAction::CycleIcons => self.icon_style = self.icon_style.cycle(),
            ClickAction::ToggleSizeMode => self.size_mode = self.size_mode.toggle(),
            ClickAction::ToggleStoreAction => self.store_action = self.store_action.toggle(),
            ClickAction::EcoToggle(i) => self.toggle_eco_at(i),
            ClickAction::EcoAllNone => self.toggle_all_ecos(),
            ClickAction::NoticeContinue => self.overlay = Overlay::ConfirmDelete,
            ClickAction::ConfirmYes => self.confirm_deletion(),
            ClickAction::ConfirmCancel => self.overlay = Overlay::None,
        }
    }

    /// Toggle the ecosystem at index `i` (as listed in the panel) and move the
    /// keyboard cursor to it, so mouse and keyboard stay in sync.
    fn toggle_eco_at(&mut self, i: usize) {
        let ecos = self.ecosystems_present();
        if let Some(&eco) = ecos.get(i) {
            if !self.disabled_ecos.remove(eco) {
                self.disabled_ecos.insert(eco.to_string());
            }
            if let Overlay::Ecosystems { cursor } = &mut self.overlay {
                *cursor = i;
            }
        }
    }

    /// Enable every ecosystem if any is disabled, otherwise disable them all —
    /// the mouse equivalent of `a` in the ecosystem panel.
    fn toggle_all_ecos(&mut self) {
        let ecos = self.ecosystems_present();
        if self.disabled_ecos.is_empty() {
            self.disabled_ecos
                .extend(ecos.iter().map(|e| e.to_string()));
        } else {
            self.disabled_ecos.clear();
        }
    }

    /// The display-item index under a click at (`col`, `row`), or `None` if the
    /// point is outside the table's data-row area or past the last row.
    fn row_at(&self, col: u16, row: u16) -> Option<usize> {
        let area = self.table_area?;
        if !area.contains(Position { x: col, y: row }) {
            return None;
        }
        let index = self.state.offset() + (row - area.y) as usize;
        (index < self.visible_len()).then_some(index)
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

    /// Open the delete-confirmation overlay — preceded by the trash-removal
    /// notice for upgrading users who haven't deleted anything since. A no-op
    /// (with a nudge) when nothing is marked.
    fn open_confirm_delete(&mut self) {
        if self.selected_total().0 == 0 {
            self.action = Some("Nothing marked — press space to mark directories.".to_string());
            return;
        }
        self.overlay = if self.trash_notice_pending {
            Overlay::TrashRemovedNotice
        } else {
            Overlay::ConfirmDelete
        };
    }

    /// The user confirmed the dialog: close it and start deleting. The first
    /// confirmation also settles the trash-removal notice for good.
    fn confirm_deletion(&mut self) {
        self.overlay = Overlay::None;
        if self.trash_notice_pending {
            self.trash_notice_pending = false;
            let mut settings = state::settings();
            settings.trash_removal_acknowledged = true;
            state::save_settings(&settings);
        }
        self.start_deletion();
    }

    /// Count and total size of the directories currently marked for deletion.
    fn selected_total(&self) -> (usize, u64) {
        self.rows
            .iter()
            .filter_map(|r| match r {
                Row::Done(d) if self.selected.contains(&d.path) => {
                    Some(d.bytes(SizeMode::Freeable))
                }
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
                    Some((d.path.clone(), d.bytes(SizeMode::Freeable)))
                }
                _ => None,
            })
            .collect();
        dirs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        dirs
    }

    /// Begin permanently deleting every marked directory. The removal runs on a
    /// worker thread — removing a large tree can take a while, and doing it
    /// inline would freeze the UI — while `drain_deletion` applies the streamed
    /// results each tick.
    fn start_deletion(&mut self) {
        let mut targets: Vec<DependencyDir> = self
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Done(d) if self.selected.contains(&d.path) => Some(d.clone()),
                _ => None,
            })
            .collect();
        if targets.is_empty() {
            return;
        }
        // Projects go first: pruning a store afterwards also drops the
        // packages only those just-deleted projects were holding on to.
        targets.sort_by_key(|d| d.kind == DirKind::PnpmStore);
        let store_action = self.store_action;
        let follow = self.follow_symlinks;
        self.action = None;
        self.delete_progress = Some(DeleteProgress {
            done: 0,
            total: targets.len(),
            reclaimed: 0,
            failures: 0,
            pruned: 0,
        });

        let (tx, rx) = mpsc::channel();
        self.delete_rx = Some(rx);
        thread::spawn(move || {
            for dir in targets {
                let outcome = delete_target(dir, store_action, follow);
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
                Ok(outcome) => self.apply_delete_outcome(outcome),
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.finish_deletion();
                    return;
                }
            }
        }
    }

    /// Apply one outcome streamed back from the deletion worker.
    fn apply_delete_outcome(&mut self, outcome: DeleteOutcome) {
        match outcome {
            DeleteOutcome::Removed { path, size } => {
                self.remove_rows(std::slice::from_ref(&path));
                self.selected.remove(&path);
                if let Some(p) = &mut self.delete_progress {
                    p.done += 1;
                    p.reclaimed += size;
                }
            }
            DeleteOutcome::Pruned { old, new } => {
                self.selected.remove(&old.path);
                if let Some(p) = &mut self.delete_progress {
                    p.done += 1;
                    p.pruned += 1;
                    p.reclaimed += old.allocated_bytes.saturating_sub(new.allocated_bytes);
                }
                if let Some(&i) = self.index.get(&old.path) {
                    self.rows[i] = Row::Done(new);
                }
            }
            DeleteOutcome::Failed { path, verb, error } => {
                self.warnings += 1;
                self.last_warning = Some(format!("failed to {verb} {}: {error}", path.display()));
                self.selected.remove(&path);
                if let Some(p) = &mut self.delete_progress {
                    p.done += 1;
                    p.failures += 1;
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
        let removed = p.done - p.failures - p.pruned;
        let mut done = Vec::new();
        if removed > 0 || p.pruned == 0 {
            done.push(format!(
                "Deleted {removed} director{}",
                if removed == 1 { "y" } else { "ies" }
            ));
        }
        if p.pruned > 0 {
            done.push(format!(
                "pruned {} store{}",
                p.pruned,
                if p.pruned == 1 { "" } else { "s" }
            ));
        }
        let mut msg = format!(
            "{} · reclaimed {}",
            done.join(", "),
            self.fmt_size(p.reclaimed)
        );
        if p.failures > 0 {
            // The last failure's text, so e.g. the prune command to run by
            // hand is right there instead of only counted.
            msg = format!(
                "{msg} · {} failed: {}",
                p.failures,
                self.last_warning.as_deref().unwrap_or("see ⚠")
            );
        }
        self.action = Some(msg);
    }

    /// Whether any row (sized or not) is a pnpm store.
    fn has_store_rows(&self) -> bool {
        self.rows
            .iter()
            .any(|r| r.ecosystem() == pnpm::STORE_ECOSYSTEM)
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
        d.bytes(self.size_mode) >= self.min_size
            && d.bytes(self.size_mode) >= self.size_filter.min_bytes()
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
            group_total(&b.1, self.size_mode)
                .cmp(&group_total(&a.1, self.size_mode))
                .then_with(|| a.0.cmp(b.0))
        });

        let mut items = Vec::new();
        for (eco, rows) in ordered {
            let total = group_total(&rows, self.size_mode);
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
            SortKey::Size => b.bytes(self.size_mode).cmp(&a.bytes(self.size_mode)),
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
                Row::Done(d) if d.bytes(self.size_mode) >= self.min_size => Some(d.clone()),
                _ => None,
            })
            .collect();
        scanner::sort_by_size(&mut done, self.size_mode);
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
        // Clickable regions are rebuilt from scratch each frame so they always
        // reflect what is actually on screen right now.
        self.clicks.clear();
        self.table_area = None;

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
            Overlay::TrashRemovedNotice => self.draw_trash_notice(frame),
            Overlay::ConfirmDelete => self.draw_confirm_delete(frame),
            Overlay::None => {}
        }
    }

    /// A spinner-and-progress modal shown while the worker thread removes the
    /// marked directories, so a slow removal reads as "working", not "frozen".
    fn draw_deleting(&self, frame: &mut Frame) {
        let Some(p) = &self.delete_progress else {
            return;
        };
        let lines = vec![
            Line::from(vec![
                Span::styled(
                    format!(" {} ", SPINNER[self.spinner]),
                    Style::new().fg(Color::Yellow).bold(),
                ),
                Span::styled(
                    format!("Deleting {} of {}…", (p.done + 1).min(p.total), p.total),
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

    fn draw_header(&mut self, frame: &mut Frame, area: Rect) {
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
        // The filter bar is the second inner line: below the top border and the
        // title line, inset one cell from the left border.
        let filters = self.filter_click_line();
        filters.register(&mut self.clicks, area.x + 1, area.y + 2);
        let text = Text::from(vec![title, filters.into_line()]);
        let para = Paragraph::new(text).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::new().fg(Color::DarkGray))
                .title(" dependency-dir-analyzer "),
        );
        frame.render_widget(para, area);
    }

    /// The filter bar shown as the second header line: each row filter labelled
    /// with its keyboard shortcut, so `s`/`a`/`e` (and `r` to reset) are visible
    /// without opening help. Active filters are highlighted; the reset hint lights
    /// up only when there is something to reset. Each chip is also a click target
    /// bound to the same action as its key.
    fn filter_click_line(&self) -> ClickLine {
        let eco_active = !self.disabled_ecos.is_empty();
        let eco_value = if eco_active {
            format!("{} hidden", self.disabled_ecos.len())
        } else {
            "all".to_string()
        };

        let mut line = ClickLine::default();
        line.text(Span::styled(
            "Filters:   ",
            Style::new().fg(Color::Gray).bold(),
        ));
        line.button(
            top_filter_chip(
                "s",
                "size",
                self.size_filter.label(),
                self.size_filter != SizeFilter::All,
            ),
            ClickAction::CycleSize,
        );
        line.text(Span::raw("  "));
        line.button(
            top_filter_chip(
                "a",
                "age",
                self.age_filter.label(),
                self.age_filter != AgeFilter::Any,
            ),
            ClickAction::CycleAge,
        );
        line.text(Span::raw("  "));
        line.button(
            top_filter_chip("e", "eco", &eco_value, eco_active),
            ClickAction::OpenEco,
        );
        line.text(Span::raw("   "));
        line.button(
            vec![
                Span::styled(
                    "[r]",
                    Style::new()
                        .fg(if self.has_filters() {
                            Color::Cyan
                        } else {
                            Color::DarkGray
                        })
                        .bold(),
                ),
                Span::styled(
                    " reset",
                    Style::new().fg(if self.has_filters() {
                        Color::Gray
                    } else {
                        Color::DarkGray
                    }),
                ),
            ],
            ClickAction::ResetFilters,
        );
        line
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
                DisplayItem::Data(Row::Done(d)) => Some(d.bytes(self.size_mode)),
                _ => None,
            })
            .max()
            .unwrap_or(0);

        // Bar column width derives from the available area (the leading
        // checkbox column and its spacing take a few more cells than before).
        let bar_width = (area.width as usize).saturating_sub(76).clamp(6, 24);

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
            Cell::from(Text::from(self.size_mode.column_title()).right_aligned()),
            Cell::from(""),
            Cell::from(Text::from("Files").right_aligned()),
            Cell::from("Modified"),
            Cell::from("Ecosystem"),
            Cell::from("Path"),
        ])
        .style(Style::new().fg(Color::Black).bg(Color::Cyan).bold());

        let widths = [
            Constraint::Length(1),
            // Room for `12,300 MB ⇄`.
            Constraint::Length(14),
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

        // Remember where the data rows live (inside the border, below the
        // one-line column header) so a click can be mapped back to a row.
        if area.height > 3 && area.width > 2 {
            self.table_area = Some(Rect {
                x: area.x + 1,
                y: area.y + 2,
                width: area.width - 2,
                height: area.height - 3,
            });
        }

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
                    d.bytes(self.size_mode) as f64 / max_size as f64
                } else {
                    0.0
                };
                let color = size_color(frac);
                let indent = if self.group { "  " } else { "" };
                let marked = self.selected.contains(&d.path);
                let mark = if marked { "✓" } else { " " };
                let row = TableRow::new(vec![
                    Cell::from(mark).style(Style::new().fg(SELECT_FG).bold()),
                    Cell::from(Text::from(self.size_cell(d)).right_aligned())
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

    fn draw_footer(&mut self, frame: &mut Frame, area: Rect, shown: usize) {
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
                Row::Done(d) if self.passes(d, now) => Some(d.bytes(self.size_mode)),
                _ => None,
            })
            .sum();

        // The deletion set gets its own prominent, attention-grabbing chip right
        // after the reclaimable-space total.
        let (sel_count, sel_bytes) = self.selected_total();
        let mut chips = ClickLine::default();
        chips.text(Span::styled(
            format!(" {} ", self.fmt_size(total)),
            Style::new().bg(Color::Green).fg(Color::Black).bold(),
        ));
        if sel_count > 0 {
            chips.text(Span::raw(" "));
            chips.text(Span::styled(
                format!(" ✗ {sel_count} marked · {} ", self.fmt_size(sel_bytes)),
                Style::new().bg(Color::Red).fg(Color::White).bold(),
            ));
        }
        chips.text(Span::raw("  "));
        chips.text(Span::styled(
            format!("{shown} shown · {sized} sized · {} found", self.rows.len()),
            Style::new().fg(Color::Gray),
        ));
        chips.text(Span::raw("   "));
        // show/sort/group/unit/icons double as click targets for their key
        // actions.
        chips.button(
            vec![Span::styled(
                format!("show:{}", self.size_mode.label()),
                Style::new().fg(Color::Green),
            )],
            ClickAction::ToggleSizeMode,
        );
        chips.text(Span::raw("  "));
        chips.button(
            vec![Span::styled(
                format!("sort:{}", self.sort.label()),
                Style::new().fg(Color::Magenta),
            )],
            ClickAction::CycleSort,
        );
        chips.text(Span::raw("  "));
        chips.button(
            vec![Span::styled(
                format!("group:{}", if self.group { "eco" } else { "off" }),
                Style::new().fg(if self.group {
                    Color::Cyan
                } else {
                    Color::DarkGray
                }),
            )],
            ClickAction::ToggleGroup,
        );
        chips.text(Span::raw("  "));
        chips.button(
            vec![Span::styled(
                format!("unit:{}", self.size_unit.label()),
                Style::new().fg(Color::Blue),
            )],
            ClickAction::CycleUnit,
        );
        chips.text(Span::raw("  "));
        chips.button(
            vec![Span::styled(
                format!("icons:{}", self.icon_style.label()),
                Style::new().fg(Color::Blue),
            )],
            ClickAction::CycleIcons,
        );
        // Only worth a chip once there is a store to act on; then it is
        // prominent, since one mode removes the whole store.
        if self.has_store_rows() {
            chips.text(Span::raw("  "));
            let style = match self.store_action {
                StoreAction::Prune => Style::new().fg(Color::Green),
                StoreAction::Delete => Style::new().fg(Color::Red).bold(),
            };
            chips.button(
                vec![Span::styled(
                    format!("store:{}", self.store_action.label()),
                    style,
                )],
                ClickAction::ToggleStoreAction,
            );
        }
        if self.warnings > 0 {
            chips.text(Span::raw("  "));
            chips.text(Span::styled(
                format!("⚠ {}", self.warnings),
                Style::new().fg(Color::Yellow),
            ));
        }

        // The chips are the first inner footer line (below the top border).
        chips.register(&mut self.clicks, area.x + 1, area.y + 1);

        // Two hint rows: view controls on top, the selection/deletion verbs
        // (which the footer chip already advertises with "press d to delete")
        // below.
        let mut hints_view = Line::from(vec![Span::styled(
            "↑/↓ move   f freeable/on disk   S sort   g group   u unit   i icons   ↵ fold",
            Style::new().fg(Color::DarkGray),
        )]);
        let any_shared = self
            .rows
            .iter()
            .any(|r| matches!(r, Row::Done(d) if d.mostly_shared() && self.passes(d, now)));
        if any_shared {
            hints_view.push_span(Span::styled(
                "   ⇄ mostly shared — deleting frees little",
                Style::new().fg(Color::Yellow),
            ));
        }
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
        let mut hints_action = hints_action;
        if self.snapshots {
            hints_action.push_span(Span::styled(
                "   Time Machine snapshots: space may only free up once they expire",
                Style::new().fg(Color::Yellow),
            ));
        }

        // The transient result of the last action (e.g. a deletion) takes the
        // view-hint line until the selection changes again, so a long one,
        // such as a prune command to run by hand, stays readable.
        if let Some(msg) = &self.action {
            hints_view = Line::from(Span::styled(
                msg.clone(),
                Style::new().fg(Color::Green).italic(),
            ));
        }
        let text = Text::from(vec![chips.into_line(), hints_view, hints_action]);
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
            help_line("f", "toggle size shown (freeable · on disk)"),
            help_line("u", "toggle size units (MB · auto)"),
            help_line("p", "pnpm stores: prune (default) or delete whole"),
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
            if self.mouse_enabled {
                help_line(
                    "mouse",
                    "scroll to move · click a row · click filters/chips",
                )
            } else {
                help_line("mouse", "off (--no-mouse / DDA_NO_MOUSE / config)")
            },
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

    fn draw_eco_panel(&mut self, frame: &mut Frame, cursor: usize) {
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

        // Build the hint line as a click line so "a all/none" is a target.
        let dim = Style::new().fg(Color::DarkGray).italic();
        let mut hint = ClickLine::default();
        hint.text(Span::styled("space toggle · ", dim));
        hint.button(
            vec![Span::styled("a all/none", dim)],
            ClickAction::EcoAllNone,
        );
        hint.text(Span::styled(" · Esc close", dim));

        let hint_index = content.len();
        let total_lines = content.len() + 1;
        let area = centered_rect(frame.area(), 46, total_lines as u16 + 2);

        // Each ecosystem row toggles that ecosystem on click; the hint's
        // all/none run toggles them all. Rows sit directly below the top border.
        for i in 0..ecos.len() {
            self.clicks.push((
                Rect {
                    x: area.x + 1,
                    y: area.y + 1 + i as u16,
                    width: area.width.saturating_sub(2),
                    height: 1,
                },
                ClickAction::EcoToggle(i),
            ));
        }
        hint.register(&mut self.clicks, area.x + 1, area.y + 1 + hint_index as u16);
        content.push(hint.into_line());

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
    /// space to be reclaimed, a reminder that deletion is irreversible (plus
    /// the trash-removal note for upgrading users) and the keys.
    fn draw_confirm_delete(&mut self, frame: &mut Frame) {
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
        let stores: HashSet<&Path> = self
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Done(d) if d.kind == DirKind::PnpmStore => Some(d.path.as_path()),
                _ => None,
            })
            .collect();
        let any_store = dirs.iter().any(|(p, _)| stores.contains(p.as_path()));
        for (path, size) in dirs.iter().take(MAX_LISTED) {
            let mut line = vec![Span::styled(
                format!("  {:>9}  ", self.fmt_size(*size)),
                Style::new().fg(Color::Gray),
            )];
            // Say what happens to a store before its (possibly cut off) path.
            if stores.contains(path.as_path()) {
                line.push(match store_action_for(path, self.store_action) {
                    StoreAction::Prune if self.store_action == StoreAction::Delete => Span::styled(
                        "prune (projects link into it) ",
                        Style::new().fg(Color::Green).bold(),
                    ),
                    StoreAction::Prune => {
                        Span::styled("prune ", Style::new().fg(Color::Green).bold())
                    }
                    StoreAction::Delete => {
                        Span::styled("DELETE whole store ", Style::new().fg(Color::Red).bold())
                    }
                });
            }
            line.push(Span::styled(
                self.display_path(path),
                Style::new().fg(Color::White),
            ));
            lines.push(Line::from(line));
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

        lines.push(Line::from(Span::styled(
            "Permanent — this cannot be undone.",
            Style::new().fg(Color::Rgb(255, 150, 150)),
        )));
        if self.trash_notice_pending {
            lines.push(Line::from(vec![
                Span::styled(
                    " NOTE ",
                    Style::new().bg(Color::Yellow).fg(Color::Black).bold(),
                ),
                Span::styled(
                    " no longer moved to the trash.",
                    Style::new().fg(Color::Yellow),
                ),
            ]));
        }
        lines.push(Line::raw(""));

        // The key legend doubles as buttons: confirm / cancel.
        let mut keys = ClickLine::default();
        keys.button(
            vec![
                Span::styled("  y/↵ ", Style::new().fg(Color::Red).bold()),
                Span::styled("delete", Style::new().fg(Color::Gray)),
            ],
            ClickAction::ConfirmYes,
        );
        keys.text(Span::styled("    ", Style::new().fg(Color::Gray)));
        keys.button(
            vec![
                Span::styled("n/Esc ", Style::new().fg(Color::Cyan).bold()),
                Span::styled("cancel", Style::new().fg(Color::Gray)),
            ],
            ClickAction::ConfirmCancel,
        );
        if any_store {
            keys.text(Span::styled("    ", Style::new().fg(Color::Gray)));
            keys.button(
                vec![
                    Span::styled("p ", Style::new().fg(Color::Green).bold()),
                    Span::styled("prune/delete store", Style::new().fg(Color::Gray)),
                ],
                ClickAction::ToggleStoreAction,
            );
        }

        let keys_index = lines.len();
        let area = centered_rect(frame.area(), 70, (lines.len() + 1) as u16 + 2);
        keys.register(&mut self.clicks, area.x + 1, area.y + 1 + keys_index as u16);
        lines.push(keys.into_line());

        frame.render_widget(Clear, area);
        let para = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::new().fg(Color::Red))
                .title(Span::styled(
                    " ⚠ Permanently delete? ",
                    Style::new().fg(Color::Red).bold(),
                ))
                .style(Style::new().bg(Color::Rgb(24, 18, 22))),
        );
        frame.render_widget(para, area);
    }

    /// The one-off notice shown to upgrading users before their first
    /// confirmation dialog: deletions used to go to the system trash, now they
    /// are permanent.
    fn draw_trash_notice(&mut self, frame: &mut Frame) {
        let text = Style::new().fg(Color::White);
        // Kept to short lines so the modal fits narrow terminals unclipped.
        let mut lines = vec![
            Line::from(Span::styled("Deletions are now permanent.", text.bold())),
            Line::raw(""),
            Line::from(Span::styled("Earlier versions of dda moved deleted", text)),
            Line::from(Span::styled(
                "directories to your system trash. Now they",
                text,
            )),
            Line::from(Span::styled("are removed right away and cannot be", text)),
            Line::from(Span::styled("recovered.", text)),
            Line::raw(""),
        ];

        let mut keys = ClickLine::default();
        keys.button(
            vec![
                Span::styled("  ↵ ", Style::new().fg(Color::Yellow).bold()),
                Span::styled("got it, continue", Style::new().fg(Color::Gray)),
            ],
            ClickAction::NoticeContinue,
        );
        keys.text(Span::styled("    ", Style::new().fg(Color::Gray)));
        keys.button(
            vec![
                Span::styled("Esc ", Style::new().fg(Color::Cyan).bold()),
                Span::styled("cancel", Style::new().fg(Color::Gray)),
            ],
            ClickAction::ConfirmCancel,
        );

        let keys_index = lines.len();
        let area = centered_rect(frame.area(), 70, (lines.len() + 1) as u16 + 2);
        keys.register(&mut self.clicks, area.x + 1, area.y + 1 + keys_index as u16);
        lines.push(keys.into_line());

        frame.render_widget(Clear, area);
        let para = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::new().fg(Color::Yellow))
                .title(Span::styled(
                    " ⚠ Heads up: no more trash ",
                    Style::new().fg(Color::Yellow).bold(),
                ))
                .style(Style::new().bg(Color::Rgb(24, 22, 16))),
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
    /// A row's size in the current mode, flagged with `⇄` when deleting it
    /// would free little because most of it is shared outside.
    fn size_cell(&self, d: &DependencyDir) -> String {
        let size = self.fmt_size(d.bytes(self.size_mode));
        if d.mostly_shared() {
            format!("{size} ⇄")
        } else {
            size
        }
    }

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

/// Delete (or, for a pnpm store in prune mode, prune) one marked directory,
/// for the deletion worker.
fn delete_target(dir: DependencyDir, store_action: StoreAction, follow: bool) -> DeleteOutcome {
    if dir.kind == DirKind::PnpmStore {
        if store_action_for(&dir.path, store_action) == StoreAction::Prune {
            return prune_store(dir, follow);
        }
        // Re-checked right before removing: store paths come from files in
        // the scanned tree, so never remove one that isn't laid out as a store.
        if !pnpm::looks_like_store(&dir.path) {
            return DeleteOutcome::Failed {
                path: dir.path,
                verb: "delete",
                error: "does not look like a pnpm store".to_string(),
            };
        }
    }
    match std::fs::remove_dir_all(&dir.path) {
        Ok(()) => DeleteOutcome::Removed {
            size: dir.freeable_bytes,
            path: dir.path,
        },
        Err(e) => DeleteOutcome::Failed {
            path: dir.path,
            verb: "delete",
            error: e.to_string(),
        },
    }
}

/// What actually happens to `store` when `chosen` is the store action: a
/// store whose `links/` projects' `node_modules` point into (pnpm 11) is only
/// ever pruned, since removing it would break every one of those projects
/// until its next `pnpm install`.
fn store_action_for(store: &Path, chosen: StoreAction) -> StoreAction {
    if pnpm::projects_link_into(store) {
        StoreAction::Prune
    } else {
        chosen
    }
}

/// Prune a marked pnpm store and re-measure it, for the deletion worker.
fn prune_store(old: DependencyDir, follow_symlinks: bool) -> DeleteOutcome {
    match pnpm::prune(&old.path) {
        Ok(()) => {
            let never = AtomicBool::new(false);
            let new = scanner::size_dir(
                old.path.clone(),
                DirKind::PnpmStore,
                pnpm::STORE_ECOSYSTEM,
                pnpm::STORE_ECOSYSTEM,
                follow_symlinks,
                &never,
            );
            DeleteOutcome::Pruned { old, new }
        }
        Err(error) => DeleteOutcome::Failed {
            path: old.path,
            verb: "prune",
            error,
        },
    }
}

fn now() -> SystemTime {
    SystemTime::now()
}

/// Sum of the done-row sizes in a group by `mode` (pending rows contribute
/// nothing).
fn group_total(rows: &[&Row], mode: SizeMode) -> u64 {
    rows.iter()
        .filter_map(|r| match r {
            Row::Done(d) => Some(d.bytes(mode)),
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
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Render the app once against a headless backend so the click regions and
    /// table area reflect a real frame, then return the terminal (kept alive by
    /// the caller if needed).
    fn render(app: &mut App, width: u16, height: u16) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
    }

    #[test]
    fn clicking_the_sort_chip_cycles_the_sort() {
        let mut app = App::new(PathBuf::from("/tmp"), 0, 0, ViewState::default());
        app.scanning = false;
        app.rows.push(done_row("Node.js", "node_modules", 100));
        render(&mut app, 120, 24);

        let (rect, _) = app
            .clicks
            .iter()
            .find(|(_, a)| matches!(a, ClickAction::CycleSort))
            .copied()
            .expect("footer sort chip should register a click region");

        let before = app.sort;
        app.handle_click(rect.x, rect.y);
        assert_eq!(app.sort, before.next());
    }

    #[test]
    fn clicking_a_table_row_selects_it() {
        let mut app = App::new(PathBuf::from("/tmp"), 0, 0, ViewState::default());
        app.scanning = false;
        // Default sort is size-descending, so "a" (100) precedes "b" (50).
        app.rows.push(done_row("Node.js", "a", 100));
        app.rows.push(done_row("Node.js", "b", 50));
        render(&mut app, 120, 24);

        let area = app.table_area.expect("table area recorded after a render");
        // Click the second data row.
        app.handle_click(area.x, area.y + 1);
        assert_eq!(app.state.selected(), Some(1));
    }

    #[test]
    fn scroll_moves_the_selection_by_a_step() {
        let mut app = App::new(PathBuf::from("/tmp"), 0, 0, ViewState::default());
        app.scanning = false;
        for _ in 0..10 {
            app.rows.push(done_row("Node.js", "n", 100));
        }
        // From the top, one notch down advances by SCROLL_STEP rows.
        app.select_first();
        app.scroll(1);
        assert_eq!(app.state.selected(), Some(SCROLL_STEP as usize));
    }

    #[test]
    fn clicking_an_ecosystem_row_toggles_it() {
        let mut app = App::new(PathBuf::from("/tmp"), 0, 0, ViewState::default());
        app.scanning = false;
        app.rows.push(done_row("Node.js", "node_modules", 100));
        app.rows.push(done_row("Rust / Maven", "target", 200));
        app.overlay = Overlay::Ecosystems { cursor: 0 };
        render(&mut app, 120, 24);

        // ecosystems_present() is sorted, so index 0 is "Node.js".
        let (rect, _) = app
            .clicks
            .iter()
            .find(|(_, a)| matches!(a, ClickAction::EcoToggle(0)))
            .copied()
            .expect("first ecosystem row should register a click region");

        assert!(!app.disabled_ecos.contains("Node.js"));
        app.handle_click(rect.x, rect.y);
        assert!(app.disabled_ecos.contains("Node.js"));
        // Clicking again re-enables it.
        app.handle_click(rect.x, rect.y);
        assert!(!app.disabled_ecos.contains("Node.js"));
    }

    #[test]
    fn chrome_clicks_are_ignored_while_an_overlay_is_open() {
        let mut app = App::new(PathBuf::from("/tmp"), 0, 0, ViewState::default());
        app.scanning = false;
        app.rows.push(done_row("Node.js", "node_modules", 100));
        // Confirm dialog is up; the sort chip is still drawn behind it.
        app.overlay = Overlay::ConfirmDelete;
        render(&mut app, 120, 24);

        let (rect, _) = app
            .clicks
            .iter()
            .find(|(_, a)| matches!(a, ClickAction::CycleSort))
            .copied()
            .expect("sort chip is drawn behind the modal");

        let before = app.sort;
        app.handle_click(rect.x, rect.y);
        // The chrome action must not fire through the modal.
        assert_eq!(app.sort, before);
    }

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
            kind: DirKind::Dependency,
            allocated_bytes: 42,
            freeable_bytes: 42,
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
            Row::Done(d) => assert_eq!(d.freeable_bytes, 42),
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
            kind: DirKind::Dependency,
            allocated_bytes: size,
            freeable_bytes: size,
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

    fn sized_row(name: &'static str, allocated: u64, freeable: u64) -> Row {
        Row::Done(DependencyDir {
            path: PathBuf::from(format!("/tmp/{name}")),
            ecosystem: "Node.js",
            name: "node_modules",
            kind: DirKind::Dependency,
            allocated_bytes: allocated,
            freeable_bytes: freeable,
            file_count: 1,
            modified: None,
        })
    }

    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn size_mode_toggle_changes_sort_and_totals() {
        let mut app = App::new(PathBuf::from("/tmp"), 0, 0, ViewState::default());
        app.scanning = false;
        app.rows.push(sized_row("a", 100, 90));
        app.rows.push(sized_row("b", 200, 10));
        let order = |app: &App| -> Vec<PathBuf> {
            app.display_items(now())
                .iter()
                .filter_map(|it| match it {
                    DisplayItem::Data(r) => Some(r.path().to_path_buf()),
                    _ => None,
                })
                .collect()
        };

        assert_eq!(app.size_mode, SizeMode::Freeable);
        assert_eq!(
            order(&app),
            [PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")]
        );

        app.handle_key(KeyCode::Char('f'), KeyModifiers::NONE);
        assert_eq!(app.size_mode, SizeMode::Allocated);
        assert_eq!(
            order(&app),
            [PathBuf::from("/tmp/b"), PathBuf::from("/tmp/a")]
        );
        assert_eq!(app.view_state().size_mode, SizeMode::Allocated);
    }

    #[test]
    fn clicking_the_size_mode_chip_toggles_it() {
        let mut app = App::new(PathBuf::from("/tmp"), 0, 0, ViewState::default());
        app.scanning = false;
        app.rows.push(done_row("Node.js", "node_modules", 100));
        render(&mut app, 140, 24);

        let (rect, _) = app
            .clicks
            .iter()
            .find(|(_, a)| matches!(a, ClickAction::ToggleSizeMode))
            .copied()
            .expect("footer size-mode chip should register a click region");

        app.handle_click(rect.x, rect.y);
        assert_eq!(app.size_mode, SizeMode::Allocated);
    }

    #[test]
    fn deletion_numbers_use_freeable() {
        let mut app = App::new(PathBuf::from("/tmp"), 0, 0, ViewState::default());
        app.rows.push(sized_row("a", 100, 90));
        app.size_mode = SizeMode::Allocated;
        app.selected.insert(PathBuf::from("/tmp/a"));
        assert_eq!(app.selected_total(), (1, 90));
        assert_eq!(app.selected_dirs(), [(PathBuf::from("/tmp/a"), 90)]);
    }

    #[test]
    fn mostly_shared_rows_show_marker() {
        let mut app = App::new(PathBuf::from("/tmp"), 0, 0, ViewState::default());
        app.scanning = false;
        app.rows.push(sized_row("shared", 1000, 10));
        let out = screen(&mut app, 140, 24);
        assert!(out.contains("⇄"), "{out}");
        assert!(out.contains("mostly shared"), "{out}");

        // Nothing shared, nothing to explain.
        let mut app = App::new(PathBuf::from("/tmp"), 0, 0, ViewState::default());
        app.scanning = false;
        app.rows.push(sized_row("own", 1000, 1000));
        let out = screen(&mut app, 140, 24);
        assert!(!out.contains("mostly shared"), "{out}");
    }

    #[test]
    fn snapshot_hint_shows_only_with_snapshots() {
        let mut app = App::new(PathBuf::from("/tmp"), 0, 0, ViewState::default());
        app.scanning = false;
        app.rows.push(done_row("Node.js", "node_modules", 100));
        assert!(!screen(&mut app, 140, 24).contains("Time Machine"));
        app.snapshots = true;
        assert!(screen(&mut app, 140, 24).contains("Time Machine snapshots"));
    }

    fn store_dir_row(path: PathBuf, allocated: u64, freeable: u64) -> DependencyDir {
        DependencyDir {
            path,
            ecosystem: crate::pnpm::STORE_ECOSYSTEM,
            name: crate::pnpm::STORE_ECOSYSTEM,
            kind: DirKind::PnpmStore,
            allocated_bytes: allocated,
            freeable_bytes: freeable,
            file_count: 1,
            modified: None,
        }
    }

    #[test]
    fn store_chip_only_shows_with_store_rows() {
        let mut app = App::new(PathBuf::from("/tmp"), 0, 0, ViewState::default());
        app.scanning = false;
        app.rows.push(done_row("Node.js", "node_modules", 100));
        assert!(!screen(&mut app, 160, 24).contains("store:"));

        app.rows.push(Row::Done(store_dir_row(
            PathBuf::from("/s/store/v10"),
            500,
            200,
        )));
        assert!(screen(&mut app, 160, 24).contains("store:prune"));
        let (rect, _) = app
            .clicks
            .iter()
            .find(|(_, a)| matches!(a, ClickAction::ToggleStoreAction))
            .copied()
            .expect("store chip should register a click region");

        app.handle_key(KeyCode::Char('p'), KeyModifiers::NONE);
        assert_eq!(app.store_action, StoreAction::Delete);
        assert!(screen(&mut app, 160, 24).contains("store:delete"));

        app.handle_click(rect.x, rect.y);
        assert_eq!(app.store_action, StoreAction::Prune);
    }

    #[test]
    fn confirm_dialog_names_store_action() {
        let mut app = App::new(PathBuf::from("/tmp"), 0, 0, ViewState::default());
        app.scanning = false;
        let store = PathBuf::from("/s/store/v10");
        app.rows
            .push(Row::Done(store_dir_row(store.clone(), 500, 200)));
        app.selected.insert(store);
        app.handle_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert!(matches!(app.overlay, Overlay::ConfirmDelete));

        let out = screen(&mut app, 140, 30);
        assert!(out.contains("prune"), "{out}");
        assert!(!out.contains("DELETE whole store"), "{out}");

        // `p` flips the action from inside the dialog, too.
        app.handle_key(KeyCode::Char('p'), KeyModifiers::NONE);
        assert!(matches!(app.overlay, Overlay::ConfirmDelete));
        assert!(screen(&mut app, 140, 30).contains("DELETE whole store"));
    }

    /// A pnpm 11 style store: projects' `node_modules` symlink into `links/`.
    fn linked_store(tag: &str) -> PathBuf {
        let tmp = std::env::temp_dir().join(format!("dda-store-{tag}-{}", std::process::id()));
        let store = tmp.join("v11");
        std::fs::create_dir_all(store.join("files")).unwrap();
        std::fs::create_dir_all(store.join("links")).unwrap();
        std::fs::write(store.join("index.db"), b"").unwrap();
        store
    }

    #[test]
    fn stores_projects_link_into_are_only_ever_pruned() {
        let store = linked_store("linked-action");
        assert_eq!(
            store_action_for(&store, StoreAction::Delete),
            StoreAction::Prune
        );
        assert_eq!(
            store_action_for(&store, StoreAction::Prune),
            StoreAction::Prune
        );
        std::fs::remove_dir_all(store.join("links")).unwrap();
        assert_eq!(
            store_action_for(&store, StoreAction::Delete),
            StoreAction::Delete
        );
        let _ = std::fs::remove_dir_all(store.parent().unwrap());
    }

    #[test]
    fn confirm_dialog_never_offers_deleting_a_linked_store() {
        let store = linked_store("linked-dialog");
        let mut app = App::new(PathBuf::from("/tmp"), 0, 0, ViewState::default());
        app.scanning = false;
        app.store_action = StoreAction::Delete;
        app.rows
            .push(Row::Done(store_dir_row(store.clone(), 500, 200)));
        app.selected.insert(store.clone());
        app.handle_key(KeyCode::Char('d'), KeyModifiers::NONE);

        let out = screen(&mut app, 160, 30);
        assert!(!out.contains("DELETE whole store"), "{out}");
        assert!(out.contains("prune"), "{out}");
        assert!(out.contains("projects link into it"), "{out}");
        let _ = std::fs::remove_dir_all(store.parent().unwrap());
    }

    #[test]
    fn prune_failure_keeps_row_and_warns() {
        let mut app = App::new(PathBuf::from("/tmp"), 0, 0, ViewState::default());
        let store = PathBuf::from("/s/store/v10");
        app.rows
            .push(Row::Done(store_dir_row(store.clone(), 500, 200)));
        app.selected.insert(store.clone());
        app.delete_progress = Some(DeleteProgress {
            done: 0,
            total: 1,
            reclaimed: 0,
            failures: 0,
            pruned: 0,
        });

        app.apply_delete_outcome(DeleteOutcome::Failed {
            path: store.clone(),
            verb: "prune",
            error: "pnpm not found; run: pnpm store prune --store-dir /s/store".to_string(),
        });
        app.finish_deletion();

        assert_eq!(app.rows.len(), 1);
        assert_eq!(app.warnings, 1);
        let action = app.action.clone().unwrap();
        assert!(
            action.contains("pnpm store prune --store-dir /s/store"),
            "{action}"
        );
    }

    #[test]
    fn pruned_store_row_is_replaced_and_counts_reclaimed() {
        let mut app = App::new(PathBuf::from("/tmp"), 0, 0, ViewState::default());
        let store = PathBuf::from("/s/store/v10");
        let old = store_dir_row(store.clone(), 500, 200);
        app.rows.push(Row::Done(old.clone()));
        app.rebuild_index();
        app.selected.insert(store.clone());
        app.delete_progress = Some(DeleteProgress {
            done: 0,
            total: 1,
            reclaimed: 0,
            failures: 0,
            pruned: 0,
        });

        let new = store_dir_row(store.clone(), 320, 20);
        app.apply_delete_outcome(DeleteOutcome::Pruned { old, new });

        assert_eq!(app.delete_progress.as_ref().unwrap().reclaimed, 180);
        assert!(app.selected.is_empty());
        match &app.rows[0] {
            Row::Done(d) => assert_eq!(d.allocated_bytes, 320),
            Row::Pending { .. } => panic!("store row should stay sized"),
        }
    }

    #[test]
    fn delete_mode_refuses_a_dir_that_is_not_a_store() {
        let tmp = std::env::temp_dir().join(format!("dda-store-keep-{}", std::process::id()));
        let not_store = tmp.join("v10");
        std::fs::create_dir_all(&not_store).unwrap();
        std::fs::write(not_store.join("precious"), b"x").unwrap();

        let mut app = App::new(tmp.clone(), 0, 0, ViewState::default());
        app.scanning = false;
        app.store_action = StoreAction::Delete;
        app.rows
            .push(Row::Done(store_dir_row(not_store.clone(), 1, 1)));
        app.selected.insert(not_store.clone());

        app.start_deletion();
        for _ in 0..1000 {
            app.drain_deletion();
            if app.delete_progress.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        assert!(not_store.join("precious").exists());
        assert_eq!(app.warnings, 1);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn delete_mode_removes_store_dir() {
        let tmp = std::env::temp_dir().join(format!("dda-store-del-{}", std::process::id()));
        let store = tmp.join("v10");
        std::fs::create_dir_all(store.join("files").join("00")).unwrap();
        std::fs::create_dir_all(store.join("index")).unwrap();
        std::fs::write(store.join("files").join("00").join("blob"), b"x").unwrap();

        let mut app = App::new(tmp.clone(), 0, 0, ViewState::default());
        app.scanning = false;
        app.store_action = StoreAction::Delete;
        app.rows.push(Row::Done(store_dir_row(store.clone(), 1, 1)));
        app.selected.insert(store.clone());

        app.start_deletion();
        for _ in 0..1000 {
            app.drain_deletion();
            if app.delete_progress.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        assert!(app.delete_progress.is_none(), "deletion did not finish");
        assert!(!store.exists());
        assert!(app.rows.is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
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
    fn trash_notice_precedes_the_confirm_dialog_until_acknowledged() {
        let mut app = App::new(PathBuf::from("/tmp"), 0, 0, ViewState::default());
        app.scanning = false;
        app.rows.push(done_row("Node.js", "node_modules", 100));
        app.selected.insert(PathBuf::from("/tmp/node_modules"));

        // Without a pending notice, `d` goes straight to the confirmation.
        app.handle_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert!(matches!(app.overlay, Overlay::ConfirmDelete));
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);

        // An upgrading user sees the notice first; cancelling keeps it pending.
        app.trash_notice_pending = true;
        app.handle_key(KeyCode::Char('d'), KeyModifiers::NONE);
        assert!(matches!(app.overlay, Overlay::TrashRemovedNotice));
        app.handle_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(app.overlay, Overlay::None));
        assert!(app.trash_notice_pending);

        // Continuing leads on to the confirmation, which repeats the note.
        app.handle_key(KeyCode::Char('d'), KeyModifiers::NONE);
        app.handle_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(app.overlay, Overlay::ConfirmDelete));
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(screen.contains("no longer moved to the trash"));
    }

    #[test]
    fn deletion_removes_dir_and_prunes_row() {
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
            kind: DirKind::Dependency,
            allocated_bytes: 5,
            freeable_bytes: 5,
            file_count: 1,
            modified: None,
        }));
        app.selected.insert(victim.clone());

        // Deletion runs on a worker thread; pump the drain until it finishes.
        app.start_deletion();
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
