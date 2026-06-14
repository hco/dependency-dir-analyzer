//! Interactive, live-updating terminal UI built on `ratatui` + `crossterm`.
//!
//! The scan runs on a background thread and streams [`ScanEvent`]s back over a
//! channel; the UI thread drains them every tick, so dependency directories
//! appear as soon as they are discovered and their sizes fill in as the
//! parallel sizing completes. The user can scroll, re-sort, group by ecosystem,
//! filter, and quit; on exit the final largest-first results are returned so the
//! caller can leave a static summary in the terminal scrollback.

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

use crate::report::{human_size, relative_age};
use crate::scanner::{self, DependencyDir, ScanEvent, ScanOptions};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const TICK: Duration = Duration::from_millis(100);

/// Run the interactive TUI for a scan of `root`, returning the final list of
/// dependency directories (sorted largest-first and filtered by `min_size` /
/// `top`) once the user quits.
pub fn run(
    root: &Path,
    opts: ScanOptions,
    top: usize,
    min_size: u64,
) -> Result<Vec<DependencyDir>> {
    let (tx, rx) = mpsc::channel();
    let root_buf = root.to_path_buf();
    let scan_thread = thread::spawn(move || {
        scanner::scan_streaming(&root_buf, &opts, tx);
    });

    let mut terminal = ratatui::init();
    let mut app = App::new(root.to_path_buf(), top, min_size);
    let result = app.event_loop(&mut terminal, rx);
    ratatui::restore();

    // The scan thread finishes once its `tx` is dropped at the end of
    // `scan_streaming`; joining keeps us from leaking it.
    let _ = scan_thread.join();

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Size,
    Modified,
    Name,
    Files,
}

impl SortKey {
    fn next(self) -> Self {
        match self {
            SortKey::Size => SortKey::Modified,
            SortKey::Modified => SortKey::Name,
            SortKey::Name => SortKey::Files,
            SortKey::Files => SortKey::Size,
        }
    }

    fn label(self) -> &'static str {
        match self {
            SortKey::Size => "size",
            SortKey::Modified => "modified",
            SortKey::Name => "name",
            SortKey::Files => "files",
        }
    }
}

/// Minimum-size threshold cycler applied on top of the CLI `--min-size`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SizeFilter {
    All,
    Mb1,
    Mb10,
    Mb100,
    Gb1,
}

impl SizeFilter {
    fn next(self) -> Self {
        match self {
            SizeFilter::All => SizeFilter::Mb1,
            SizeFilter::Mb1 => SizeFilter::Mb10,
            SizeFilter::Mb10 => SizeFilter::Mb100,
            SizeFilter::Mb100 => SizeFilter::Gb1,
            SizeFilter::Gb1 => SizeFilter::All,
        }
    }

    fn min_bytes(self) -> u64 {
        match self {
            SizeFilter::All => 0,
            SizeFilter::Mb1 => 1_000_000,
            SizeFilter::Mb10 => 10_000_000,
            SizeFilter::Mb100 => 100_000_000,
            SizeFilter::Gb1 => 1_000_000_000,
        }
    }

    fn label(self) -> &'static str {
        match self {
            SizeFilter::All => "all",
            SizeFilter::Mb1 => "≥1 MB",
            SizeFilter::Mb10 => "≥10 MB",
            SizeFilter::Mb100 => "≥100 MB",
            SizeFilter::Gb1 => "≥1 GB",
        }
    }
}

/// "Modified older than" threshold cycler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgeFilter {
    Any,
    Day1,
    Week1,
    Month1,
    Month6,
    Year1,
}

impl AgeFilter {
    fn next(self) -> Self {
        match self {
            AgeFilter::Any => AgeFilter::Day1,
            AgeFilter::Day1 => AgeFilter::Week1,
            AgeFilter::Week1 => AgeFilter::Month1,
            AgeFilter::Month1 => AgeFilter::Month6,
            AgeFilter::Month6 => AgeFilter::Year1,
            AgeFilter::Year1 => AgeFilter::Any,
        }
    }

    /// Minimum age in seconds a directory must have to pass, or `None` for "any".
    fn min_age_secs(self) -> Option<u64> {
        const DAY: u64 = 86_400;
        match self {
            AgeFilter::Any => None,
            AgeFilter::Day1 => Some(DAY),
            AgeFilter::Week1 => Some(7 * DAY),
            AgeFilter::Month1 => Some(30 * DAY),
            AgeFilter::Month6 => Some(182 * DAY),
            AgeFilter::Year1 => Some(365 * DAY),
        }
    }

    fn label(self) -> &'static str {
        match self {
            AgeFilter::Any => "any",
            AgeFilter::Day1 => ">1d",
            AgeFilter::Week1 => ">1w",
            AgeFilter::Month1 => ">1mo",
            AgeFilter::Month6 => ">6mo",
            AgeFilter::Year1 => ">1y",
        }
    }
}

/// A modal overlay drawn on top of the table; only one can be open at a time.
enum Overlay {
    None,
    Help,
    Ecosystems { cursor: usize },
}

/// One rendered line of the table: an ecosystem group header (only when
/// grouping is on) or an actual directory row.
enum DisplayItem<'a> {
    Header {
        ecosystem: &'static str,
        total: u64,
        count: usize,
        collapsed: bool,
    },
    Data(&'a Row),
}

struct App {
    root: PathBuf,
    rows: Vec<Row>,
    warnings: usize,
    last_warning: Option<String>,
    scanning: bool,
    sort: SortKey,
    group: bool,
    size_filter: SizeFilter,
    age_filter: AgeFilter,
    /// Ecosystems the user has toggled *off* in the ecosystem panel. Empty means
    /// every ecosystem is shown.
    disabled_ecos: HashSet<&'static str>,
    /// Ecosystems whose group is collapsed (header shown, rows hidden). Only has
    /// an effect while grouping is on.
    collapsed_ecos: HashSet<&'static str>,
    overlay: Overlay,
    spinner: usize,
    top: usize,
    min_size: u64,
    state: TableState,
}

impl App {
    fn new(root: PathBuf, top: usize, min_size: u64) -> Self {
        App {
            root,
            rows: Vec::new(),
            warnings: 0,
            last_warning: None,
            scanning: true,
            sort: SortKey::Size,
            group: false,
            size_filter: SizeFilter::All,
            age_filter: AgeFilter::Any,
            disabled_ecos: HashSet::new(),
            collapsed_ecos: HashSet::new(),
            overlay: Overlay::None,
            spinner: 0,
            top,
            min_size,
            state: TableState::default(),
        }
    }

    fn event_loop(
        &mut self,
        terminal: &mut DefaultTerminal,
        rx: mpsc::Receiver<ScanEvent>,
    ) -> Result<()> {
        loop {
            self.drain(&rx);

            terminal.draw(|frame| self.draw(frame))?;

            if event::poll(TICK)? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press && self.handle_key(key.code, key.modifiers) {
                        return Ok(());
                    }
                }
            }
            self.spinner = (self.spinner + 1) % SPINNER.len();
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
                self.rows.push(Row::Pending { path, ecosystem });
            }
            ScanEvent::Sized(dir) => {
                if let Some(row) = self.rows.iter_mut().find(|r| r.path() == dir.path) {
                    *row = Row::Done(dir);
                } else {
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
        // Ctrl-C always quits, regardless of any open overlay.
        if mods.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
            return true;
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
            KeyCode::Enter | KeyCode::Char(' ') => self.toggle_collapse(),
            KeyCode::Char('s') => self.sort = self.sort.next(),
            KeyCode::Char('g') => self.group = !self.group,
            KeyCode::Char('f') => self.size_filter = self.size_filter.next(),
            KeyCode::Char('a') => self.age_filter = self.age_filter.next(),
            KeyCode::Char('e') => self.overlay = Overlay::Ecosystems { cursor: 0 },
            KeyCode::Char('r') => self.reset_filters(),
            KeyCode::Char('?') => self.overlay = Overlay::Help,
            _ => {}
        }
        false
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
                        self.disabled_ecos.insert(eco);
                    }
                }
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                // Toggle all: if anything is disabled, enable everything;
                // otherwise disable everything.
                if self.disabled_ecos.is_empty() {
                    self.disabled_ecos.extend(ecos.iter().copied());
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
            self.collapsed_ecos.insert(eco);
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
    fn display_items(&self, now: SystemTime) -> Vec<DisplayItem> {
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
                    matches!(r, Row::Pending { ecosystem, .. } if !self.disabled_ecos.contains(ecosystem))
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
            let collapsed = self.collapsed_ecos.contains(eco);
            items.push(DisplayItem::Header {
                ecosystem: eco,
                total,
                count,
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
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(4),
        ])
        .split(frame.area());

        self.draw_header(frame, chunks[0]);
        self.draw_table(frame, chunks[1]);
        self.draw_footer(frame, chunks[2]);

        match self.overlay {
            Overlay::Help => self.draw_help(frame),
            Overlay::Ecosystems { cursor } => self.draw_eco_panel(frame, cursor),
            Overlay::None => {}
        }
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let status = if self.scanning {
            format!(" {} scanning", SPINNER[self.spinner])
        } else {
            " ✓ done".to_string()
        };
        let line = Line::from(vec![
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
        let para = Paragraph::new(line).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::new().fg(Color::DarkGray))
                .title(" dependency-dir-analyzer "),
        );
        frame.render_widget(para, area);
    }

    /// Ensure the current selection points at a valid row, snapping it back into
    /// range when the view has shrunk (e.g. after a filter change or a fold).
    fn clamp_selection(&mut self) {
        let len = self.visible_len();
        match self.state.selected() {
            _ if len == 0 => self.state.select(None),
            Some(i) if i >= len => self.state.select(Some(len - 1)),
            None => self.state.select(Some(0)),
            _ => {}
        }
    }

    fn draw_table(&mut self, frame: &mut Frame, area: Rect) {
        // Clamp the selection first so the borrow below stays immutable.
        self.clamp_selection();

        let items = self.display_items(now());

        if items.is_empty() {
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
            return;
        }

        let max_size = items
            .iter()
            .filter_map(|it| match it {
                DisplayItem::Data(Row::Done(d)) => Some(d.size_bytes),
                _ => None,
            })
            .max()
            .unwrap_or(0);

        // Bar column width derives from the available area.
        let bar_width = (area.width as usize).saturating_sub(64).clamp(6, 24);

        // Collect into an owned Vec so the immutable borrow of `self`/`items`
        // ends before the table is rendered with `&mut self.state` below.
        let rows: Vec<TableRow> = items
            .iter()
            .map(|it| self.render_item(it, max_size, bar_width))
            .collect();

        let header = TableRow::new(vec![
            Cell::from("Size"),
            Cell::from(""),
            Cell::from("Files"),
            Cell::from("Modified"),
            Cell::from("Ecosystem"),
            Cell::from("Path"),
        ])
        .style(Style::new().fg(Color::Black).bg(Color::Cyan).bold());

        let widths = [
            Constraint::Length(10),
            Constraint::Length(bar_width as u16),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(16),
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
                collapsed,
            } => {
                let glyph = if *collapsed { "▸" } else { "▾" };
                TableRow::new(vec![
                    Cell::from(human_size(*total)).style(Style::new().fg(Color::White).bold()),
                    Cell::from(""),
                    Cell::from(format!("{count} dir{}", if *count == 1 { "" } else { "s" }))
                        .style(Style::new().fg(Color::Gray)),
                    Cell::from(""),
                    Cell::from(format!("{glyph} {ecosystem}"))
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
                TableRow::new(vec![
                    Cell::from(human_size(d.size_bytes)).style(Style::new().fg(color).bold()),
                    Cell::from(build_bar(frac, bar_width)).style(Style::new().fg(color)),
                    Cell::from(d.file_count.to_string()).style(Style::new().fg(Color::Gray)),
                    Cell::from(relative_age(d.modified))
                        .style(Style::new().fg(age_color(d.modified))),
                    Cell::from(d.ecosystem).style(Style::new().fg(Color::Cyan)),
                    Cell::from(format!("{indent}{}", self.display_path(&d.path))),
                ])
            }
            DisplayItem::Data(Row::Pending { path, ecosystem }) => TableRow::new(vec![
                Cell::from(format!("{} sizing…", SPINNER[self.spinner]))
                    .style(Style::new().fg(Color::DarkGray)),
                Cell::from(build_bar(0.0, bar_width)),
                Cell::from("—").style(Style::new().fg(Color::DarkGray)),
                Cell::from("—").style(Style::new().fg(Color::DarkGray)),
                Cell::from(*ecosystem).style(Style::new().fg(Color::DarkGray)),
                Cell::from(self.display_path(path)).style(Style::new().fg(Color::DarkGray)),
            ]),
        }
    }

    fn has_filters(&self) -> bool {
        self.size_filter != SizeFilter::All
            || self.age_filter != AgeFilter::Any
            || !self.disabled_ecos.is_empty()
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let now = now();
        let sized = self
            .rows
            .iter()
            .filter(|r| matches!(r, Row::Done(_)))
            .count();
        let items = self.display_items(now);
        let shown = items
            .iter()
            .filter(|it| matches!(it, DisplayItem::Data(_)))
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

        let mut chips = vec![
            Span::styled(
                format!(" {} ", human_size(total)),
                Style::new().bg(Color::Green).fg(Color::Black).bold(),
            ),
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
        ];
        chips.push(Span::raw("  "));
        chips.push(filter_chip(
            "size",
            self.size_filter.label(),
            self.size_filter != SizeFilter::All,
        ));
        chips.push(Span::raw("  "));
        chips.push(filter_chip(
            "age",
            self.age_filter.label(),
            self.age_filter != AgeFilter::Any,
        ));
        if !self.disabled_ecos.is_empty() {
            chips.push(Span::raw("  "));
            chips.push(Span::styled(
                format!("eco:−{}", self.disabled_ecos.len()),
                Style::new().fg(Color::Yellow),
            ));
        }
        if self.warnings > 0 {
            chips.push(Span::raw("  "));
            chips.push(Span::styled(
                format!("⚠ {}", self.warnings),
                Style::new().fg(Color::Yellow),
            ));
        }

        let hints = Line::from(vec![Span::styled(
            "↑/↓ move   s sort   g group   ↵ fold   f size   a age   e ecosystem   r reset   ? help   q quit",
            Style::new().fg(Color::DarkGray),
        )]);

        let text = Text::from(vec![Line::from(chips), hints]);
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
            help_line("s", "cycle sort (size · modified · name · files)"),
            help_line("g", "toggle grouping by ecosystem"),
            help_line("Enter / Space", "fold/unfold selected group"),
            help_line("f", "cycle minimum-size filter"),
            help_line("a", "cycle \"modified older than\" filter"),
            help_line("e", "open ecosystem filter (multi-select)"),
            help_line("r", "reset all filters & grouping"),
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
                    let enabled = !self.disabled_ecos.contains(eco);
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
                            *eco,
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

/// Sum of the done-row sizes in a group (pending rows contribute nothing).
fn group_total(rows: &[&Row]) -> u64 {
    rows.iter()
        .filter_map(|r| match r {
            Row::Done(d) => Some(d.size_bytes),
            _ => None,
        })
        .sum()
}

/// A small `label:value` chip for the footer, highlighted when the filter is
/// active.
fn filter_chip(label: &str, value: &str, active: bool) -> Span<'static> {
    let style = if active {
        Style::new().fg(Color::Black).bg(Color::Yellow).bold()
    } else {
        Style::new().fg(Color::DarkGray)
    };
    Span::styled(format!(" {label}:{value} "), style)
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
    fn sort_key_cycles() {
        let mut k = SortKey::Size;
        k = k.next();
        assert_eq!(k, SortKey::Modified);
        k = k.next().next().next();
        assert_eq!(k, SortKey::Size);
    }

    #[test]
    fn size_filter_cycles_and_thresholds_increase() {
        let mut f = SizeFilter::All;
        assert_eq!(f.min_bytes(), 0);
        f = f.next();
        assert_eq!(f, SizeFilter::Mb1);
        // Cycling all the way around returns to All.
        let back = f.next().next().next().next();
        assert_eq!(back, SizeFilter::All);
    }

    #[test]
    fn age_filter_older_than_semantics() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        let mut app = App::new(PathBuf::from("/tmp"), 0, 0);
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
        let mut app = App::new(PathBuf::from("/tmp"), 0, 0);
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
        app.collapsed_ecos.insert("Rust / Maven");
        assert_eq!(count(&app), (2, 1));

        // Reset restores everything.
        app.reset_filters();
        assert!(!app.group);
        assert_eq!(count(&app), (0, 2));
    }
}
