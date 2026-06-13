//! Interactive, live-updating terminal UI built on `ratatui` + `crossterm`.
//!
//! The scan runs on a background thread and streams [`ScanEvent`]s back over a
//! channel; the UI thread drains them every tick, so dependency directories
//! appear as soon as they are discovered and their sizes fill in as the
//! parallel sizing completes. The user can scroll, re-sort, and quit; on exit
//! the final sorted results are returned so the caller can leave a static
//! summary in the terminal scrollback.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row as TableRow, Table, TableState, Wrap};
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

struct App {
    root: PathBuf,
    rows: Vec<Row>,
    warnings: usize,
    last_warning: Option<String>,
    scanning: bool,
    sort: SortKey,
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
        let visible = self.visible_len();
        match code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => return true,
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1, visible),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1, visible),
            KeyCode::PageDown => self.move_selection(10, visible),
            KeyCode::PageUp => self.move_selection(-10, visible),
            KeyCode::Home | KeyCode::Char('g') => {
                if visible > 0 {
                    self.state.select(Some(0));
                }
            }
            KeyCode::End | KeyCode::Char('G') => {
                if visible > 0 {
                    self.state.select(Some(visible - 1));
                }
            }
            KeyCode::Char('s') => self.sort = self.sort.next(),
            _ => {}
        }
        false
    }

    fn move_selection(&mut self, delta: isize, len: usize) {
        if len == 0 {
            self.state.select(None);
            return;
        }
        let current = self.state.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, len as isize - 1) as usize;
        self.state.select(Some(next));
    }

    /// Build the ordered list of rows to display this frame: completed rows
    /// (filtered by `min_size`, sorted by the active key, capped by `top`)
    /// followed by any still-pending rows while the scan is running.
    fn visible(&self) -> Vec<&Row> {
        let mut done: Vec<&Row> = self
            .rows
            .iter()
            .filter(|r| match r {
                Row::Done(d) => d.size_bytes >= self.min_size,
                Row::Pending { .. } => false,
            })
            .collect();

        done.sort_by(|a, b| match (a, b) {
            (Row::Done(a), Row::Done(b)) => self.compare(a, b),
            _ => std::cmp::Ordering::Equal,
        });

        if self.top > 0 && done.len() > self.top {
            done.truncate(self.top);
        }

        if self.scanning {
            done.extend(
                self.rows
                    .iter()
                    .filter(|r| matches!(r, Row::Pending { .. })),
            );
        }
        done
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

    fn visible_len(&self) -> usize {
        self.visible().len()
    }

    /// The completed, filtered, sorted-by-size results to hand back to the
    /// caller when the UI exits.
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

    fn draw(&mut self, frame: &mut Frame) {
        let chunks = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(3),
        ])
        .split(frame.area());

        self.draw_header(frame, chunks[0]);
        self.draw_table(frame, chunks[1]);
        self.draw_footer(frame, chunks[2]);
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

    fn draw_table(&mut self, frame: &mut Frame, area: Rect) {
        // Clamp the selection to the current list before borrowing it for the
        // frame (this borrow of `self` ends with the statement).
        let len = self.visible_len();
        match self.state.selected() {
            _ if len == 0 => self.state.select(None),
            Some(i) if i >= len => self.state.select(Some(len - 1)),
            None => self.state.select(Some(0)),
            _ => {}
        }

        let visible = self.visible();

        if visible.is_empty() {
            let msg = if self.scanning {
                "Scanning…".to_string()
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

        let max_size = visible
            .iter()
            .filter_map(|r| match r {
                Row::Done(d) => Some(d.size_bytes),
                _ => None,
            })
            .max()
            .unwrap_or(0);

        // Bar column width derives from the available area.
        let bar_width = (area.width as usize).saturating_sub(64).clamp(6, 24);

        let rows = visible.iter().map(|row| match row {
            Row::Done(d) => {
                let frac = if max_size > 0 {
                    d.size_bytes as f64 / max_size as f64
                } else {
                    0.0
                };
                let color = size_color(frac);
                TableRow::new(vec![
                    Cell::from(human_size(d.size_bytes)).style(Style::new().fg(color).bold()),
                    Cell::from(build_bar(frac, bar_width)).style(Style::new().fg(color)),
                    Cell::from(d.file_count.to_string()).style(Style::new().fg(Color::Gray)),
                    Cell::from(relative_age(d.modified))
                        .style(Style::new().fg(age_color(d.modified))),
                    Cell::from(d.ecosystem).style(Style::new().fg(Color::Cyan)),
                    Cell::from(self.display_path(&d.path)),
                ])
            }
            Row::Pending { path, ecosystem } => TableRow::new(vec![
                Cell::from(format!("{} sizing…", SPINNER[self.spinner]))
                    .style(Style::new().fg(Color::DarkGray)),
                Cell::from(build_bar(0.0, bar_width)),
                Cell::from("—").style(Style::new().fg(Color::DarkGray)),
                Cell::from("—").style(Style::new().fg(Color::DarkGray)),
                Cell::from(*ecosystem).style(Style::new().fg(Color::DarkGray)),
                Cell::from(self.display_path(path)).style(Style::new().fg(Color::DarkGray)),
            ]),
        });

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

    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let sized = self
            .rows
            .iter()
            .filter(|r| matches!(r, Row::Done(_)))
            .count();
        let total: u64 = self
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Done(d) if d.size_bytes >= self.min_size => Some(d.size_bytes),
                _ => None,
            })
            .sum();
        let shown = self.visible_len();

        let mut spans = vec![
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
                format!("sort: {}", self.sort.label()),
                Style::new().fg(Color::Magenta),
            ),
        ];
        if self.warnings > 0 {
            spans.push(Span::raw("   "));
            spans.push(Span::styled(
                format!("⚠ {} warning(s)", self.warnings),
                Style::new().fg(Color::Yellow),
            ));
        }

        let hints = Line::from(vec![Span::styled(
            "↑/↓ move   s sort   g/G top/bottom   q quit",
            Style::new().fg(Color::DarkGray),
        )]);

        let text = Text::from(vec![Line::from(spans), hints]);
        let para = Paragraph::new(text).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::new().fg(Color::DarkGray)),
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

/// Colour the age column: stale directories (good cleanup candidates) trend
/// green, recently-touched ones red.
fn age_color(modified: Option<SystemTime>) -> Color {
    let Some(modified) = modified else {
        return Color::DarkGray;
    };
    let days = SystemTime::now()
        .duration_since(modified)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0);
    match days {
        0..=6 => Color::Red,
        7..=29 => Color::Yellow,
        30..=89 => Color::Rgb(180, 200, 120),
        _ => Color::Green,
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
}
