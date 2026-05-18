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
    style::{Modifier, Style},
    widgets::{Block, Borders, Paragraph, Row, Table, TableState},
};

use crate::error::{Error, Result};
use crate::nvs::{NvsItem, NvsPartition};

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
        }
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
                if state.editing_filter {
                    match k.code {
                        KeyCode::Esc | KeyCode::Enter => {
                            state.editing_filter = false;
                        }
                        KeyCode::Backspace => {
                            state.filter.pop();
                        }
                        KeyCode::Char(c) => {
                            if k.modifiers.contains(KeyModifiers::CONTROL) && (c == 'c' || c == 'd')
                            {
                                return Ok(());
                            }
                            state.filter.push(c);
                        }
                        _ => {}
                    }
                    continue;
                }
                match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('c') | KeyCode::Char('d')
                        if k.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        return Ok(())
                    }
                    KeyCode::Down | KeyCode::Char('j') => state.next(),
                    KeyCode::Up | KeyCode::Char('k') => state.prev(),
                    KeyCode::PageDown => state.page_down(),
                    KeyCode::PageUp => state.page_up(),
                    KeyCode::Home | KeyCode::Char('g') => state.first(),
                    KeyCode::End | KeyCode::Char('G') => state.last(),
                    KeyCode::Char('/') => {
                        state.editing_filter = true;
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

    // Footer
    let footer_text = if state.editing_filter {
        format!(" filter: {}_   [Enter/Esc: done] ", state.filter)
    } else if state.filter.is_empty() {
        " [↑/↓ j/k navigate · PgUp/PgDn · g/G first/last · / filter · q quit] ".into()
    } else {
        format!(
            " filter: {}   [/: edit · q: quit] ",
            if state.filter.is_empty() {
                "(none)".into()
            } else {
                state.filter.clone()
            }
        )
    };
    let footer = Paragraph::new(footer_text).block(Block::default().borders(Borders::ALL));
    f.render_widget(footer, chunks[2]);
}
