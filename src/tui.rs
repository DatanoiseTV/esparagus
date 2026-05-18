//! Ratatui-based interactive TUI screens.
//!
//! Scope right now: the NVS viewer (the only screen wired up).  A future
//! commit will add a status-dashboard variant for the main flash flow.

use std::io::{self, stdout};
use std::time::Duration;

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event as CtEvent, KeyCode, KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    layout::{Constraint, Direction, Layout},
    prelude::{CrosstermBackend, Terminal},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Row, Table, TableState, Wrap},
};

use crate::error::{Error, Result};
use crate::nvs::{NvsItem, NvsPartition, NvsValue};

/// RAII guard around the terminal raw-mode + alternate-screen setup so that
/// the terminal is always restored on exit, including panic paths.
struct TerminalGuard {
    pub term: Terminal<CrosstermBackend<io::Stdout>>,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode().map_err(|e| Error::Other(format!("enable_raw_mode: {e}")))?;
        let mut out = stdout();
        execute!(out, EnterAlternateScreen, EnableMouseCapture)
            .map_err(|e| Error::Other(format!("enter alt screen: {e}")))?;
        let backend = CrosstermBackend::new(out);
        let term =
            Terminal::new(backend).map_err(|e| Error::Other(format!("Terminal::new: {e}")))?;
        Ok(Self { term })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Best-effort cleanup; ignore errors because we're already on the
        // way out and the user wants their terminal back regardless.
        let _ = execute!(
            self.term.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        );
        let _ = disable_raw_mode();
    }
}

/// Run the NVS-viewer TUI.  Blocks until the user presses `q`, `Esc`, or
/// `Ctrl-C`.  Returns Ok(()) regardless of how the user exited.
pub fn run_nvs_view(partition: &NvsPartition, source_label: &str) -> Result<()> {
    let mut guard = TerminalGuard::enter()?;
    let result = nvs_event_loop(&mut guard.term, partition, source_label);
    drop(guard);
    result
}

struct NvsViewState<'a> {
    rows: Vec<&'a NvsItem>,
    table: TableState,
    filter: String,
    editing_filter: bool,
    /// When `Some`, we're in the per-entry detail view (hex + ASCII).
    /// The usize is the scroll offset of the detail pane (lines from top).
    detail_view: Option<usize>,
}

impl<'a> NvsViewState<'a> {
    fn new(partition: &'a NvsPartition) -> Self {
        let rows: Vec<&NvsItem> = partition.items.iter().collect();
        let mut table = TableState::default();
        if !rows.is_empty() {
            table.select(Some(0));
        }
        Self {
            rows,
            table,
            filter: String::new(),
            editing_filter: false,
            detail_view: None,
        }
    }

    /// The item the table cursor is currently on, after filtering.
    fn selected_item(&self) -> Option<&NvsItem> {
        let filtered = self.filtered();
        let i = self.table.selected()?;
        filtered.get(i).copied()
    }

    fn filtered(&self) -> Vec<&NvsItem> {
        if self.filter.is_empty() {
            return self.rows.clone();
        }
        let needle = self.filter.to_ascii_lowercase();
        self.rows
            .iter()
            .filter(|r| {
                r.namespace.to_ascii_lowercase().contains(&needle)
                    || r.key.to_ascii_lowercase().contains(&needle)
                    || r.value.display().to_ascii_lowercase().contains(&needle)
            })
            .copied()
            .collect()
    }

    fn next(&mut self) {
        let len = self.filtered().len();
        if len == 0 {
            return;
        }
        let i = self.table.selected().unwrap_or(0);
        self.table.select(Some((i + 1).min(len - 1)));
    }

    fn prev(&mut self) {
        let i = self.table.selected().unwrap_or(0);
        if i > 0 {
            self.table.select(Some(i - 1));
        }
    }

    fn page_down(&mut self) {
        let len = self.filtered().len();
        if len == 0 {
            return;
        }
        let i = self.table.selected().unwrap_or(0);
        self.table.select(Some((i + 10).min(len - 1)));
    }

    fn page_up(&mut self) {
        let i = self.table.selected().unwrap_or(0);
        self.table.select(Some(i.saturating_sub(10)));
    }

    fn first(&mut self) {
        if !self.filtered().is_empty() {
            self.table.select(Some(0));
        }
    }

    fn last(&mut self) {
        let len = self.filtered().len();
        if len > 0 {
            self.table.select(Some(len - 1));
        }
    }
}

fn nvs_event_loop(
    term: &mut Terminal<CrosstermBackend<io::Stdout>>,
    partition: &NvsPartition,
    source_label: &str,
) -> Result<()> {
    let mut state = NvsViewState::new(partition);
    loop {
        term.draw(|f| draw_nvs(f, &mut state, partition, source_label))
            .map_err(|e| Error::Other(format!("draw: {e}")))?;
        if event::poll(Duration::from_millis(200))
            .map_err(|e| Error::Other(format!("poll: {e}")))?
        {
            if let CtEvent::Key(k) =
                event::read().map_err(|e| Error::Other(format!("event::read: {e}")))?
            {
                // Always honor Ctrl-C / Ctrl-D as quit.
                if matches!(k.code, KeyCode::Char('c') | KeyCode::Char('d'))
                    && k.modifiers.contains(KeyModifiers::CONTROL)
                {
                    return Ok(());
                }

                // Detail view has its own minimal key set; Esc / q / Enter
                // returns to the table.
                if state.detail_view.is_some() {
                    match k.code {
                        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                            state.detail_view = None;
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            if let Some(s) = state.detail_view.as_mut() {
                                *s = s.saturating_add(1);
                            }
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            if let Some(s) = state.detail_view.as_mut() {
                                *s = s.saturating_sub(1);
                            }
                        }
                        KeyCode::PageDown => {
                            if let Some(s) = state.detail_view.as_mut() {
                                *s = s.saturating_add(10);
                            }
                        }
                        KeyCode::PageUp => {
                            if let Some(s) = state.detail_view.as_mut() {
                                *s = s.saturating_sub(10);
                            }
                        }
                        KeyCode::Home => {
                            state.detail_view = Some(0);
                        }
                        _ => {}
                    }
                    continue;
                }

                if state.editing_filter {
                    match k.code {
                        KeyCode::Esc | KeyCode::Enter => {
                            state.editing_filter = false;
                        }
                        KeyCode::Backspace => {
                            state.filter.pop();
                        }
                        KeyCode::Char(c) => {
                            state.filter.push(c);
                        }
                        _ => {}
                    }
                    continue;
                }
                match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Down | KeyCode::Char('j') => state.next(),
                    KeyCode::Up | KeyCode::Char('k') => state.prev(),
                    KeyCode::PageDown => state.page_down(),
                    KeyCode::PageUp => state.page_up(),
                    KeyCode::Home | KeyCode::Char('g') => state.first(),
                    KeyCode::End | KeyCode::Char('G') => state.last(),
                    KeyCode::Char('/') => {
                        state.editing_filter = true;
                    }
                    KeyCode::Enter if state.selected_item().is_some() => {
                        // Drop into the per-entry hex / ASCII detail.
                        state.detail_view = Some(0);
                    }
                    _ => {}
                }
            }
        }
    }
}

fn draw_nvs(
    f: &mut ratatui::Frame,
    state: &mut NvsViewState,
    partition: &NvsPartition,
    source_label: &str,
) {
    if state.detail_view.is_some() {
        // Capture the selected item BEFORE we take a mutable borrow of
        // detail_view for the scroll offset.
        let selected = state.selected_item().cloned();
        let scroll = state.detail_view.unwrap_or(0);
        if let Some(item) = selected {
            draw_detail(f, &item, scroll);
            return;
        }
        // Selection invalidated mid-flight; fall through to the table.
        state.detail_view = None;
    }
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Min(1),    // table
            Constraint::Length(3), // footer / filter
        ])
        .split(area);

    // Header
    let total = partition.items.len();
    let visible = state.filtered().len();
    let header_text = format!(
        " esparagus nvs view — {} entries ({} shown) — source: {}",
        total, visible, source_label
    );
    let header = Paragraph::new(header_text).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" NVS partition "),
    );
    f.render_widget(header, chunks[0]);

    // Table
    let header_cells = ["Namespace", "Key", "Type", "Value"]
        .iter()
        .map(|h| ratatui::text::Span::styled(*h, Style::default().add_modifier(Modifier::BOLD)))
        .collect::<Vec<_>>();
    let header_row = Row::new(header_cells).height(1);

    let rows: Vec<Row> = state
        .filtered()
        .into_iter()
        .map(|it| {
            Row::new(vec![
                it.namespace.clone(),
                it.key.clone(),
                it.ty.name(),
                it.value.display(),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(18),
        Constraint::Length(20),
        Constraint::Length(12),
        Constraint::Min(20),
    ];
    let table = Table::new(rows, widths)
        .header(header_row)
        .block(Block::default().borders(Borders::ALL).title(" entries "))
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(table, chunks[1], &mut state.table);

    // Footer hint extends now to mention Enter.
    let footer_text = if state.editing_filter {
        format!(" filter: {}_   [Enter/Esc: done] ", state.filter)
    } else if state.filter.is_empty() {
        " [↑/↓ j/k navigate · Enter detail · PgUp/PgDn · g/G first/last · / filter · q quit] "
            .into()
    } else {
        format!(
            " filter: {}   [Enter: detail · /: edit · q: quit] ",
            state.filter.clone()
        )
    };
    let footer = Paragraph::new(footer_text).block(Block::default().borders(Borders::ALL));
    f.render_widget(footer, chunks[2]);
}

/// Detail view for one NVS entry: scrollable hex + ASCII dump, colorised
/// by byte class so you can scan a blob and tell what's in it.
fn draw_detail(f: &mut ratatui::Frame, item: &NvsItem, scroll: usize) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Length(4), // typed-summary panel
            Constraint::Min(1),    // hex+ascii
            Constraint::Length(3), // footer
        ])
        .split(area);

    let title = format!(
        " {} . {}    [{}] ",
        item.namespace,
        item.key,
        item.ty.name()
    );
    let header = Paragraph::new(title.clone()).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" entry detail "),
    );
    f.render_widget(header, chunks[0]);

    // Typed summary — show the value as a typed string (number, decoded
    // string, byte count for blobs).
    let summary_lines: Vec<Line> = match &item.value {
        NvsValue::U8(v) => vec![Line::from(vec![
            Span::raw("u8 = "),
            Span::styled(v.to_string(), Style::default().fg(Color::Cyan)),
            Span::raw(format!("  hex 0x{:02x}  bin 0b{:08b}", v, v)),
        ])],
        NvsValue::I8(v) => vec![Line::from(vec![
            Span::raw("i8 = "),
            Span::styled(v.to_string(), Style::default().fg(Color::Cyan)),
        ])],
        NvsValue::U16(v) => vec![Line::from(vec![
            Span::raw("u16 = "),
            Span::styled(v.to_string(), Style::default().fg(Color::Cyan)),
            Span::raw(format!("  hex 0x{:04x}", v)),
        ])],
        NvsValue::I16(v) => vec![Line::from(vec![
            Span::raw("i16 = "),
            Span::styled(v.to_string(), Style::default().fg(Color::Cyan)),
        ])],
        NvsValue::U32(v) => vec![Line::from(vec![
            Span::raw("u32 = "),
            Span::styled(v.to_string(), Style::default().fg(Color::Cyan)),
            Span::raw(format!("  hex 0x{:08x}", v)),
        ])],
        NvsValue::I32(v) => vec![Line::from(vec![
            Span::raw("i32 = "),
            Span::styled(v.to_string(), Style::default().fg(Color::Cyan)),
        ])],
        NvsValue::U64(v) => vec![Line::from(vec![
            Span::raw("u64 = "),
            Span::styled(v.to_string(), Style::default().fg(Color::Cyan)),
            Span::raw(format!("  hex 0x{:016x}", v)),
        ])],
        NvsValue::I64(v) => vec![Line::from(vec![
            Span::raw("i64 = "),
            Span::styled(v.to_string(), Style::default().fg(Color::Cyan)),
        ])],
        NvsValue::String(s) => vec![
            Line::from(vec![
                Span::raw("string ("),
                Span::styled(format!("{}B", s.len()), Style::default().fg(Color::Yellow)),
                Span::raw(") = "),
            ]),
            Line::from(vec![Span::styled(
                format!("  {:?}", s),
                Style::default().fg(Color::Green),
            )]),
        ],
        NvsValue::Blob { bytes } | NvsValue::Raw { bytes } => vec![Line::from(vec![
            Span::raw("blob "),
            Span::styled(
                format!("{}B", bytes.len()),
                Style::default().fg(Color::Yellow),
            ),
        ])],
    };
    let summary = Paragraph::new(summary_lines)
        .block(Block::default().borders(Borders::ALL).title(" value "))
        .wrap(Wrap { trim: false });
    f.render_widget(summary, chunks[1]);

    // Hex + ASCII view.
    let raw = value_bytes(&item.value);
    let hex_pane_height = chunks[2].height.saturating_sub(2) as usize; // -2 for borders
    let mut lines: Vec<Line> = Vec::new();
    for (i, chunk) in raw.chunks(16).enumerate().skip(scroll) {
        if lines.len() >= hex_pane_height {
            break;
        }
        let addr = i * 16;
        let mut spans: Vec<Span> = Vec::with_capacity(48);
        spans.push(Span::styled(
            format!("{:08x}  ", addr),
            Style::default().fg(Color::DarkGray),
        ));
        // Hex pairs.
        for (j, b) in chunk.iter().enumerate() {
            spans.push(Span::styled(
                format!("{:02x}", b),
                Style::default().fg(byte_color(*b)),
            ));
            spans.push(Span::raw(" "));
            if j == 7 {
                spans.push(Span::raw(" "));
            }
        }
        // Pad missing hex columns so the ASCII gutter aligns.
        for j in chunk.len()..16 {
            spans.push(Span::raw("   "));
            if j == 7 {
                spans.push(Span::raw(" "));
            }
        }
        spans.push(Span::raw(" |"));
        for b in chunk {
            let ch = if (0x20..0x7F).contains(b) {
                *b as char
            } else {
                '.'
            };
            spans.push(Span::styled(
                ch.to_string(),
                Style::default().fg(byte_color(*b)),
            ));
        }
        spans.push(Span::raw("|"));
        lines.push(Line::from(spans));
    }
    if raw.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no bytes)",
            Style::default().fg(Color::DarkGray),
        )));
    }
    let hex_view = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" hex / ascii "),
    );
    f.render_widget(hex_view, chunks[2]);

    let footer =
        Paragraph::new(" [↑/↓ j/k scroll · PgUp/PgDn · Home top · Enter/q/Esc back to table] ")
            .block(Block::default().borders(Borders::ALL));
    f.render_widget(footer, chunks[3]);
}

/// Render an NvsValue as its raw byte representation for the hex pane.
/// Scalars are little-endian (matching how NVS stores them on flash).
fn value_bytes(value: &NvsValue) -> Vec<u8> {
    match value {
        NvsValue::U8(v) => vec![*v],
        NvsValue::I8(v) => vec![*v as u8],
        NvsValue::U16(v) => v.to_le_bytes().to_vec(),
        NvsValue::I16(v) => v.to_le_bytes().to_vec(),
        NvsValue::U32(v) => v.to_le_bytes().to_vec(),
        NvsValue::I32(v) => v.to_le_bytes().to_vec(),
        NvsValue::U64(v) => v.to_le_bytes().to_vec(),
        NvsValue::I64(v) => v.to_le_bytes().to_vec(),
        NvsValue::String(s) => s.as_bytes().to_vec(),
        NvsValue::Blob { bytes } | NvsValue::Raw { bytes } => bytes.clone(),
    }
}

/// Per-byte foreground colour for the hex pane. Bytes are grouped so a
/// hex dump tells a story at a glance:
///   * 0x00     dim grey      (zeros / nulls)
///   * 0xFF     yellow        (flash-erase pattern / sentinels)
///   * printable ASCII  green (strings stand out)
///   * other low control bytes  blue
///   * other high bytes         red
fn byte_color(b: u8) -> Color {
    match b {
        0x00 => Color::DarkGray,
        0xFF => Color::Yellow,
        b if (0x20..0x7F).contains(&b) => Color::Green,
        b if b < 0x20 => Color::Blue,
        _ => Color::Red,
    }
}
