//! `alacritty_terminal` implementation of `VtEngine`. Pure Rust — no Zig, no FFI.

use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color as VtColor, Processor};

use ratatui::style::{Color, Modifier};

use super::{CodexComposerRegion, Cursor, HistoryMetrics, RenderCell, RetainedRowLayout, VtEngine};
use crate::terminal::backend::{CaptureMode, CaptureResult};
use crate::terminal::pty::InputAction;

type TitleSlot = Arc<Mutex<Option<String>>>;

/// Receives terminal-generated responses (cursor reports, device attributes,
/// etc.) and forwards them back to the child via the shared write channel.
/// Also captures the window title (OSC 0/2) for agent detection.
#[derive(Clone)]
pub struct EventProxy {
    tx: Sender<InputAction>,
    title: TitleSlot,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => {
                let _ = self.tx.send(InputAction::Bytes(text.into_bytes()));
            }
            Event::Title(t) => {
                if let Ok(mut g) = self.title.lock() {
                    *g = Some(t);
                }
            }
            Event::ResetTitle => {
                if let Ok(mut g) = self.title.lock() {
                    *g = None;
                }
            }
            _ => {}
        }
    }
}

/// A size descriptor for `Term::new` / `Term::resize`.
#[derive(Clone, Copy)]
struct Dims {
    cols: usize,
    rows: usize,
}

impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

pub struct AlacrittyEngine {
    term: Term<EventProxy>,
    parser: Processor,
    title: TitleSlot,
    history_budget_bytes: usize,
    output_generation: u64,
}

impl AlacrittyEngine {
    pub fn new(
        cols: u16,
        rows: u16,
        resp_tx: Sender<InputAction>,
        history_budget_bytes: usize,
    ) -> Self {
        let dims = Dims {
            cols: cols.max(1) as usize,
            rows: rows.max(1) as usize,
        };
        let title: TitleSlot = Arc::new(Mutex::new(None));
        let proxy = EventProxy {
            tx: resp_tx,
            title: title.clone(),
        };
        // Alacritty retains history by rows, not bytes. Derive a conservative
        // capacity from Luvus's per-pane byte budget and current width. The
        // estimate deliberately overcharges each row; metrics identify it as an
        // estimate until an engine provides native byte accounting.
        let config = Config {
            scrolling_history: history_rows_for_budget(history_budget_bytes, cols),
            ..Config::default()
        };
        let term = Term::new(config, &dims, proxy);
        AlacrittyEngine {
            term,
            parser: Processor::new(),
            title,
            history_budget_bytes,
            output_generation: 0,
        }
    }

    fn apply_history_budget(&mut self) {
        self.term.set_options(Config {
            scrolling_history: history_rows_for_budget(
                self.history_budget_bytes,
                self.term.grid().columns() as u16,
            ),
            ..Config::default()
        });
    }

    fn write_retained_row(&self, index: usize, output: &mut String) -> bool {
        let Ok(index) = i32::try_from(index) else {
            return false;
        };
        let grid = self.term.grid();
        let top = grid.topmost_line().0;
        let bottom = grid.bottommost_line().0;
        let line = top.saturating_add(index);
        if line > bottom {
            return false;
        }

        output.clear();
        let row = &grid[Line(line)];
        for column in 0..grid.columns() {
            let cell = &row[Column(column)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            output.push(if cell.c == '\0' { ' ' } else { cell.c });
            if let Some(zerowidth) = cell.zerowidth() {
                output.extend(zerowidth);
            }
        }
        let trimmed = output.trim_end().len();
        output.truncate(trimmed);
        true
    }

    fn retained_row_wraps(&self, index: usize) -> bool {
        let Ok(index) = i32::try_from(index) else {
            return false;
        };
        let grid = self.term.grid();
        let line = grid.topmost_line().0.saturating_add(index);
        if line > grid.bottommost_line().0 || grid.columns() == 0 {
            return false;
        }
        grid[Line(line)][Column(grid.columns() - 1)]
            .flags
            .contains(Flags::WRAPLINE)
    }

    fn retained_line(&self, index: usize) -> Option<Line> {
        let index = i32::try_from(index).ok()?;
        let grid = self.term.grid();
        let line = grid.topmost_line().0.saturating_add(index);
        (line <= grid.bottommost_line().0).then_some(Line(line))
    }

    fn append_plain_grid_row(&self, line: Line, output: &mut String, max_bytes: usize) -> bool {
        let grid = self.term.grid();
        let row = &grid[line];
        let last = (0..grid.columns())
            .rfind(|column| {
                let cell = &row[Column(*column)];
                !cell.flags.contains(Flags::WIDE_CHAR_SPACER) && cell.c != '\0' && cell.c != ' '
            })
            .map_or(0, |column| column + 1);
        let mut encoded = [0_u8; 4];
        for column in 0..last {
            let cell = &row[Column(column)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let character = if cell.c == '\0' { ' ' } else { cell.c };
            if (!character.is_control() || character == '\t')
                && !append_utf8_bounded(output, character.encode_utf8(&mut encoded), max_bytes)
            {
                return false;
            }
            if let Some(zerowidth) = cell.zerowidth() {
                for character in zerowidth.iter().copied().filter(|c| !c.is_control()) {
                    if !append_utf8_bounded(output, character.encode_utf8(&mut encoded), max_bytes)
                    {
                        return false;
                    }
                }
            }
        }
        true
    }

    fn append_ansi_grid_row(&self, line: Line, output: &mut String, max_bytes: usize) -> bool {
        let grid = self.term.grid();
        let row = &grid[line];
        let last = (0..grid.columns())
            .rfind(|column| {
                let cell = &row[Column(*column)];
                !cell.flags.contains(Flags::WIDE_CHAR_SPACER) && cell.c != '\0' && cell.c != ' '
            })
            .map_or(0, |column| column + 1);
        let mut style = (Color::Reset, Color::Reset, Modifier::empty());
        // Always reserve room to reset a style we emit.
        let content_limit = max_bytes.saturating_sub(4);
        for column in 0..last {
            let cell = &row[Column(column)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let character = if cell.c == '\0' { ' ' } else { cell.c };
            if character.is_control() && character != '\t' {
                continue;
            }
            let next_style = (
                map_color(cell.fg),
                map_color(cell.bg),
                map_flags(cell.flags),
            );
            let style_code =
                (next_style != style).then(|| sgr(next_style.0, next_style.1, next_style.2));
            let mut symbol = character.to_string();
            if let Some(zerowidth) = cell.zerowidth() {
                symbol.extend(zerowidth.iter().copied().filter(|c| !c.is_control()));
            }
            let needed = style_code.as_ref().map_or(0, String::len) + symbol.len();
            if output.len().saturating_add(needed) > content_limit {
                if style != (Color::Reset, Color::Reset, Modifier::empty()) {
                    output.push_str("\x1b[0m");
                }
                return false;
            }
            if let Some(code) = style_code {
                output.push_str(&code);
                style = next_style;
            }
            output.push_str(&symbol);
        }
        if style != (Color::Reset, Color::Reset, Modifier::empty()) {
            output.push_str("\x1b[0m");
        }
        true
    }
}

fn append_utf8_bounded(output: &mut String, text: &str, max_bytes: usize) -> bool {
    if output.len().saturating_add(text.len()) <= max_bytes {
        output.push_str(text);
        return true;
    }
    let remaining = max_bytes.saturating_sub(output.len());
    let mut end = remaining.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    output.push_str(&text[..end]);
    false
}

/// Conservative upper estimate for a retained terminal row. It includes more
/// than the measured fixed cell footprint plus allocator/row overhead, so the
/// Alacritty adapter stays below the selected history budget in ordinary use.
const HISTORY_CELL_BYTES: usize = 32;
const HISTORY_ROW_OVERHEAD_BYTES: usize = 512;

fn estimated_row_bytes(cols: usize) -> usize {
    cols.max(1)
        .saturating_mul(HISTORY_CELL_BYTES)
        .saturating_add(HISTORY_ROW_OVERHEAD_BYTES)
}

fn history_rows_for_budget(bytes: usize, cols: u16) -> usize {
    bytes
        .saturating_div(estimated_row_bytes(cols.max(1) as usize))
        .max(1)
}

impl VtEngine for AlacrittyEngine {
    fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
        self.output_generation = self.output_generation.wrapping_add(1);
    }

    fn finish_output_batch(&mut self) {
        self.term.finish_output_batch();
    }

    fn output_generation(&self) -> u64 {
        self.output_generation
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.term.resize(Dims {
            cols: cols.max(1) as usize,
            rows: rows.max(1) as usize,
        });
        self.apply_history_budget();
    }

    fn cursor(&self) -> Cursor {
        let p = self.term.grid().cursor.point;
        Cursor {
            x: p.column.0 as u16,
            y: p.line.0.max(0) as u16,
            // Scrolled into history: the live cursor isn't in view, so hide it
            // rather than draw it over an old line.
            visible: self.term.mode().contains(TermMode::SHOW_CURSOR)
                && self.term.grid().display_offset() == 0,
        }
    }

    fn codex_composer_region(&self) -> Option<CodexComposerRegion> {
        let grid = self.term.grid();
        if grid.display_offset() != 0 {
            return None;
        }

        let rows = grid.screen_lines();
        let cols = grid.columns();
        if rows < 3 || cols < 4 {
            return None;
        }

        let cursor = grid.cursor.point.line.0.max(0) as usize;
        if cursor >= rows {
            return None;
        }
        let row_is_blank = |row: usize| {
            (0..cols).all(|col| {
                let c = grid[Line(row as i32)][Column(col)].c;
                c == '\0' || c == ' '
            })
        };
        let row_has_prompt =
            |row: usize| (0..cols.min(3)).any(|col| grid[Line(row as i32)][Column(col)].c == '›');

        let prompt = (cursor.saturating_sub(8)..=cursor)
            .rev()
            .find(|&row| row_has_prompt(row))?;
        let top = prompt.checked_sub(1)?;
        if !row_is_blank(top) || (prompt..=cursor).any(row_is_blank) {
            return None;
        }

        let bottom_limit = (cursor + 8).min(rows - 1);
        let bottom = ((cursor + 1)..=bottom_limit).find(|&row| row_is_blank(row))?;
        Some(CodexComposerRegion {
            top: top as u16,
            bottom: bottom as u16,
        })
    }

    fn for_each_cell(&self, f: &mut dyn FnMut(u16, u16, &str, RenderCell)) {
        // `display_iter` walks the *displayed* region, whose lines are *negative*
        // once scrolled into history (it starts at `Line(-display_offset)`).
        // Shift by the offset to get viewport rows `0..screen_lines`; dropping
        // the negative ones instead would blank the pane the further you scroll.
        let grid = self.term.grid();
        let offset = grid.display_offset() as i32;
        let rows = grid.screen_lines() as i32;
        // The symbol is `cell.c` plus any combining/VS16/ZWJ chars alacritty stores
        // as `zerowidth`. Emitting only `cell.c` dropped those, so `🖥️`/accents
        // rendered as a bare base glyph or a tofu box.
        //
        // Hot path (every visible cell, every frame): the overwhelmingly common
        // cell is a single char with no combining marks, so encode it straight into
        // a stack buffer and touch no heap at all — the same cost as the old
        // single-`char` path. Only a cell that actually carries `zerowidth` marks
        // spills into `combined`, a `String` allocated lazily (at most once, then
        // reused) and never touched otherwise. Zero allocation in the common case,
        // correctness in the rare one.
        let mut stack = [0u8; 4];
        let mut combined = String::new();
        for indexed in grid.display_iter() {
            let row = indexed.point.line.0 + offset;
            if !(0..rows).contains(&row) {
                continue;
            }
            let cell = indexed.cell;
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let sym: &str = match cell.zerowidth() {
                None => cell.c.encode_utf8(&mut stack),
                Some(zw) => {
                    combined.clear();
                    combined.push(cell.c);
                    combined.extend(zw.iter());
                    &combined
                }
            };
            f(
                row as u16,
                indexed.point.column.0 as u16,
                sym,
                RenderCell {
                    fg: map_color(cell.fg),
                    bg: map_color(cell.bg),
                    mods: map_flags(cell.flags),
                },
            );
        }
    }

    fn detection_text(&self, n: u16) -> String {
        // Index the grid by `Line` rather than using `display_iter()`: line
        // indexing is relative to the **live** screen (`Storage::compute_index`
        // ignores `display_offset`), while `display_iter` follows the user's
        // scrollback position. Agent state must describe what the agent is doing
        // *now*, not whatever the user happens to be looking at — scrollback
        // preserves the spinner/interrupt frames of earlier turns, so reading the
        // scrolled viewport made a quiet agent read as Working the moment you
        // scrolled up (docs/07).
        let grid = self.term.grid();
        let rows = grid.screen_lines();
        let cols = grid.columns();
        let start = rows.saturating_sub(n as usize);
        let mut out = String::new();
        for r in start..rows {
            let row = &grid[Line(r as i32)];
            let mut line = String::with_capacity(cols);
            for c in 0..cols {
                let cell = &row[Column(c)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                line.push(if cell.c == '\0' { ' ' } else { cell.c });
                if let Some(zerowidth) = cell.zerowidth() {
                    line.extend(zerowidth);
                }
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(line.trim_end());
        }
        out
    }

    fn visible_rows(&self) -> Vec<String> {
        // Same offset shift as `for_each_cell` — these are the rows the user can
        // see, so a selection made while scrolled back must copy the history
        // text, not come back empty.
        let grid = self.term.grid();
        let rows = grid.screen_lines();
        let offset = grid.display_offset() as i32;
        let mut lines = vec![String::new(); rows];
        for indexed in grid.display_iter() {
            let r = indexed.point.line.0 + offset;
            if r < 0 || r as usize >= rows {
                continue;
            }
            if indexed.cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let c = indexed.cell.c;
            lines[r as usize].push(if c == '\0' { ' ' } else { c });
        }
        lines
    }

    fn visible_rows_aligned(&self) -> Vec<String> {
        // Identical to `visible_rows`, except a wide-char spacer cell is kept (as
        // its blank) instead of skipped, so each terminal column contributes one
        // char and a string index equals a column. See the trait doc for why a
        // caller addressing a column needs this.
        let grid = self.term.grid();
        let rows = grid.screen_lines();
        let offset = grid.display_offset() as i32;
        let mut lines = vec![String::new(); rows];
        for indexed in grid.display_iter() {
            let r = indexed.point.line.0 + offset;
            if r < 0 || r as usize >= rows {
                continue;
            }
            let c = indexed.cell.c;
            lines[r as usize].push(if c == '\0' { ' ' } else { c });
        }
        lines
    }

    fn backend_capture(
        &self,
        mode: CaptureMode,
        lines: usize,
        ansi: bool,
        max_bytes: usize,
    ) -> CaptureResult {
        let lines = lines.max(1);
        if mode == CaptureMode::Detection {
            let grid = self.term.grid();
            let rows = grid.screen_lines();
            let start = rows.saturating_sub(lines);
            let mut text = String::new();
            let mut count = 0;
            let mut complete = true;
            for row in start..rows {
                if count > 0 && !append_utf8_bounded(&mut text, "\n", max_bytes) {
                    complete = false;
                    break;
                }
                count += 1;
                if !self.append_plain_grid_row(Line(row as i32), &mut text, max_bytes) {
                    complete = false;
                    break;
                }
            }
            return CaptureResult {
                text,
                lines: count,
                truncated: !complete,
            };
        }

        let grid = self.term.grid();
        let mut output = String::new();
        let mut returned = 0;
        let mut truncated = false;
        match mode {
            CaptureMode::Visible => {
                let rows = grid.screen_lines();
                let start = rows.saturating_sub(lines);
                for row in start..rows {
                    if returned > 0 && !append_utf8_bounded(&mut output, "\n", max_bytes) {
                        truncated = true;
                        break;
                    }
                    let complete = if ansi {
                        self.append_ansi_grid_row(Line(row as i32), &mut output, max_bytes)
                    } else {
                        self.append_plain_grid_row(Line(row as i32), &mut output, max_bytes)
                    };
                    returned += 1;
                    if !complete {
                        truncated = true;
                        break;
                    }
                }
            }
            CaptureMode::RecentUnwrapped => {
                let count = self.retained_row_count();
                let mut logical: Vec<Vec<usize>> = Vec::new();
                let mut current = Vec::new();
                let mut row = String::new();
                let mut inspected_bytes = 0usize;
                let max_inspected_rows = max_bytes
                    .saturating_div(std::mem::size_of::<usize>().max(1))
                    .max(1);
                let mut inspected_rows = 0usize;
                for index in (0..count).rev() {
                    let Some(line) = self.retained_line(index) else {
                        continue;
                    };
                    row.clear();
                    let remaining = max_bytes.saturating_sub(inspected_bytes);
                    let complete = self.append_plain_grid_row(line, &mut row, remaining);
                    if !complete || inspected_rows >= max_inspected_rows {
                        truncated = true;
                        if current.is_empty() {
                            current.push(index);
                        }
                        logical.push(std::mem::take(&mut current));
                        break;
                    }
                    inspected_rows += 1;
                    inspected_bytes = inspected_bytes.saturating_add(row.len());
                    current.push(index);
                    if index == 0 || !self.retained_row_wraps(index - 1) {
                        logical.push(std::mem::take(&mut current));
                        if logical.len() >= lines {
                            break;
                        }
                    }
                }
                logical.reverse();
                for mut physical_rows in logical {
                    if returned > 0 && !append_utf8_bounded(&mut output, "\n", max_bytes) {
                        truncated = true;
                        break;
                    }
                    physical_rows.reverse();
                    let mut complete = true;
                    for index in physical_rows {
                        let Some(line) = self.retained_line(index) else {
                            continue;
                        };
                        complete = if ansi {
                            self.append_ansi_grid_row(line, &mut output, max_bytes)
                        } else {
                            self.append_plain_grid_row(line, &mut output, max_bytes)
                        };
                        if !complete {
                            break;
                        }
                    }
                    returned += 1;
                    if !complete {
                        truncated = true;
                        break;
                    }
                }
            }
            CaptureMode::Detection => unreachable!(),
        }
        CaptureResult {
            text: output,
            lines: returned,
            truncated,
        }
    }

    fn title(&self) -> Option<String> {
        self.title.lock().ok().and_then(|g| g.clone())
    }

    fn set_history_budget(&mut self, bytes: usize) {
        // `set_options` funnels into `Grid::update_history`, which *shrinks* the
        // retained history when the limit drops — so lowering the setting frees
        // memory on existing panes instead of only applying to new ones.
        let shrinking = bytes < self.history_budget_bytes;
        self.history_budget_bytes = bytes;
        self.apply_history_budget();
        // A deliberate budget reduction is the right time to return spare rows
        // to the allocator. Normal PTY output keeps the small byte-capped cache.
        if shrinking {
            self.term.compact_history();
        }
    }

    fn scroll(&mut self, delta: i32) {
        if !self.term.mode().contains(TermMode::ALT_SCREEN) {
            self.term.scroll_display(Scroll::Delta(delta));
        }
    }

    fn scroll_to_top(&mut self) {
        if !self.term.mode().contains(TermMode::ALT_SCREEN) {
            self.term.scroll_display(Scroll::Top);
        }
    }

    fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    fn scroll_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    fn history_len(&self) -> usize {
        // `Dimensions::history_size` = total_lines − screen_lines (the scrollback).
        self.term.grid().history_size()
    }

    fn history_metrics(&self) -> HistoryMetrics {
        let retained_rows = self.history_len();
        let retained_bytes =
            retained_rows.saturating_mul(estimated_row_bytes(self.term.grid().columns()));
        HistoryMetrics {
            offset: self.scroll_offset(),
            retained_rows,
            budget_bytes: self.history_budget_bytes,
            retained_bytes,
            estimated_grid_bytes: self.term.estimated_grid_bytes(),
            cache_bytes: Some(self.term.history_cache_bytes()),
            compacted_rows: Some(self.term.compacted_history_rows()),
            allocated_cells: Some(self.term.allocated_cell_capacity()),
            exact_bytes: false,
        }
    }

    fn retained_row_count(&self) -> usize {
        self.term
            .grid()
            .history_size()
            .saturating_add(self.term.grid().screen_lines())
    }

    #[cfg(test)]
    fn retained_row_text(&self, index: usize) -> Option<String> {
        let mut output = String::with_capacity(self.term.grid().columns());
        self.write_retained_row(index, &mut output)
            .then_some(output)
    }

    fn for_each_retained_row(&self, f: &mut dyn FnMut(usize, &str)) {
        let mut output = String::with_capacity(self.term.grid().columns());
        for index in 0..self.retained_row_count() {
            if self.write_retained_row(index, &mut output) {
                f(index, &output);
            }
        }
    }

    fn retained_selection_text(
        &self,
        ((start_row, start_col), (end_row, end_col)): ((usize, usize), (usize, usize)),
    ) -> Option<String> {
        if start_row > end_row {
            return None;
        }
        let last_column = self.term.grid().columns().checked_sub(1)?;
        let middle_left = start_col.min(end_col);
        let mut output = String::new();
        let mut appended = false;

        for row_index in start_row..=end_row {
            let Some(line) = self.retained_line(row_index) else {
                continue;
            };
            let left = if row_index == start_row {
                start_col
            } else {
                middle_left
            }
            .min(last_column);
            let right = if row_index == end_row {
                end_col
            } else {
                last_column
            }
            .min(last_column);

            if appended {
                output.push('\n');
            }
            appended = true;
            if left <= right {
                let row = self.term.bounds_to_string(
                    Point::new(line, Column(left)),
                    Point::new(line, Column(right)),
                );
                output.push_str(row.trim_end_matches(' '));
            }
        }

        appended.then_some(output)
    }

    fn retained_row_layout(&self, index: usize) -> Option<RetainedRowLayout> {
        let line = self.retained_line(index)?;
        let grid = self.term.grid();
        let row = &grid[line];
        let mut whitespace = Vec::with_capacity(grid.columns());
        let mut previous_whitespace = true;
        let mut last_content = None;

        for column in 0..grid.columns() {
            let cell = &row[Column(column)];
            let wide_spacer = cell.flags.contains(Flags::WIDE_CHAR_SPACER);
            let leading_spacer = cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER);
            let cell_whitespace = if wide_spacer {
                previous_whitespace
            } else if leading_spacer {
                false
            } else {
                cell.c == '\0' || cell.c.is_whitespace()
            };
            whitespace.push(cell_whitespace);

            let has_content = leading_spacer
                || (!wide_spacer && cell.c != '\0' && cell.c != ' ')
                || (wide_spacer && last_content == column.checked_sub(1));
            if has_content {
                last_content = Some(column);
            }
            if !wide_spacer && !leading_spacer {
                previous_whitespace = cell_whitespace;
            }
        }

        let has_text = last_content.is_some();
        whitespace.truncate(last_content.map_or(1, |column| column + 1));
        Some(RetainedRowLayout::new(whitespace, has_text))
    }

    fn scroll_to(&mut self, offset: usize) {
        if self.term.mode().contains(TermMode::ALT_SCREEN) {
            return;
        }
        let max = self.term.grid().history_size();
        let target = offset.min(max) as i32;
        let current = self.term.grid().display_offset() as i32;
        // `Scroll::Delta` is positive-scrolls-up (into history), matching `scroll`.
        self.term.scroll_display(Scroll::Delta(target - current));
    }

    fn alt_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    fn mouse_report(&self) -> bool {
        // MOUSE_MODE = REPORT_CLICK | MOUSE_MOTION | MOUSE_DRAG.
        self.term.mode().intersects(TermMode::MOUSE_MODE)
    }

    fn alternate_scroll(&self) -> bool {
        self.term.mode().contains(TermMode::ALTERNATE_SCROLL)
    }

    fn application_cursor(&self) -> bool {
        self.term.mode().contains(TermMode::APP_CURSOR)
    }

    fn mouse_drag(&self) -> bool {
        self.term
            .mode()
            .intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION)
    }

    fn mouse_motion(&self) -> bool {
        self.term.mode().contains(TermMode::MOUSE_MOTION)
    }

    fn sgr_mouse(&self) -> bool {
        self.term.mode().contains(TermMode::SGR_MOUSE)
    }

    fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    fn snapshot_ansi(&self) -> String {
        let grid = self.term.grid();
        let rows = grid.screen_lines();
        let cols = grid.columns();
        if rows == 0 || cols == 0 {
            return String::new();
        }
        let default = (' ', Color::Reset, Color::Reset, Modifier::empty());
        let mut cells = vec![vec![default; cols]; rows];
        for indexed in grid.display_iter() {
            let r = indexed.point.line.0;
            let c = indexed.point.column.0;
            if r < 0 || r as usize >= rows || c >= cols {
                continue;
            }
            let cell = indexed.cell;
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let ch = if cell.c == '\0' { ' ' } else { cell.c };
            cells[r as usize][c] = (
                ch,
                map_color(cell.fg),
                map_color(cell.bg),
                map_flags(cell.flags),
            );
        }

        // Trim trailing blank rows so replaying into any-size engine doesn't
        // scroll the content off-screen.
        let last_row = match cells
            .iter()
            .rposition(|row| row.iter().any(|c| *c != default))
        {
            Some(r) => r,
            None => return String::from("\x1b[2J\x1b[H"),
        };
        let mut out = String::from("\x1b[2J\x1b[H");
        for (ri, row) in cells.iter().take(last_row + 1).enumerate() {
            let last = row.iter().rposition(|c| *c != default).map_or(0, |i| i + 1);
            let mut cur = (Color::Reset, Color::Reset, Modifier::empty());
            for (ch, fg, bg, m) in &row[..last] {
                if (*fg, *bg, *m) != cur {
                    out.push_str(&sgr(*fg, *bg, *m));
                    cur = (*fg, *bg, *m);
                }
                out.push(*ch);
            }
            out.push_str("\x1b[0m");
            if ri < last_row {
                out.push_str("\r\n");
            }
        }
        out
    }
}

fn sgr(fg: Color, bg: Color, m: Modifier) -> String {
    let mut s = String::from("\x1b[0");
    if m.contains(Modifier::BOLD) {
        s.push_str(";1");
    }
    if m.contains(Modifier::DIM) {
        s.push_str(";2");
    }
    if m.contains(Modifier::ITALIC) {
        s.push_str(";3");
    }
    if m.contains(Modifier::UNDERLINED) {
        s.push_str(";4");
    }
    if m.contains(Modifier::REVERSED) {
        s.push_str(";7");
    }
    push_color(&mut s, fg, 38);
    push_color(&mut s, bg, 48);
    s.push('m');
    s
}

fn push_color(s: &mut String, c: Color, base: u8) {
    match c {
        Color::Indexed(i) => s.push_str(&format!(";{base};5;{i}")),
        Color::Rgb(r, g, b) => s.push_str(&format!(";{base};2;{r};{g};{b}")),
        _ => {}
    }
}

fn map_color(c: VtColor) -> Color {
    match c {
        VtColor::Spec(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
        VtColor::Indexed(i) => Color::Indexed(i),
        VtColor::Named(n) => {
            // The first 16 named colors map to the ANSI palette; everything
            // else (Foreground/Background/Cursor/Dim*) resolves to the host
            // terminal's default so its real background shows through.
            let idx = n as usize;
            if idx < 16 {
                Color::Indexed(idx as u8)
            } else {
                Color::Reset
            }
        }
    }
}

fn map_flags(fl: Flags) -> Modifier {
    let mut m = Modifier::empty();
    if fl.contains(Flags::BOLD) {
        m |= Modifier::BOLD;
    }
    if fl.contains(Flags::ITALIC) {
        m |= Modifier::ITALIC;
    }
    if fl.contains(Flags::UNDERLINE) {
        m |= Modifier::UNDERLINED;
    }
    if fl.contains(Flags::DIM) {
        m |= Modifier::DIM;
    }
    if fl.contains(Flags::INVERSE) {
        m |= Modifier::REVERSED;
    }
    if fl.contains(Flags::HIDDEN) {
        m |= Modifier::HIDDEN;
    }
    if fl.contains(Flags::STRIKEOUT) {
        m |= Modifier::CROSSED_OUT;
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    fn feed_lines(e: &mut AlacrittyEngine, n: usize) {
        for i in 0..n {
            e.advance(format!("line{i}\r\n").as_bytes());
        }
    }

    fn budget_for_rows(cols: usize, rows: usize) -> usize {
        estimated_row_bytes(cols).saturating_mul(rows)
    }

    // docs/07: agent detection must read the **live** screen, never the
    // scrolled-back viewport. Scrollback preserves the spinner/interrupt frames
    // an agent printed earlier, so a user scrolling up would otherwise drag a
    // stale "working" marker into the detection window and the pane would read
    // as Working while the agent sits idle.
    // Regression: `display_iter` yields *negative* lines once scrolled into
    // history, so skipping `r < 0` progressively blanked the pane — at the top of
    // history it drew nothing at all, and a selection there copied nothing.
    // Scrollback is the dominant per-pane memory cost, so it is user-set
    // (Settings → Layout). Lowering the limit must *drop* the excess history
    // immediately, not just apply to new panes.
    #[test]
    fn scrollback_limit_is_honored_and_shrinks_live() {
        let (tx, _rx) = channel();
        let mut e = AlacrittyEngine::new(20, 5, tx, budget_for_rows(20, 100));
        feed_lines(&mut e, 400);
        assert_eq!(
            e.history_len(),
            100,
            "history is capped at the configured limit"
        );

        // Lowering it reclaims immediately…
        e.set_history_budget(budget_for_rows(20, 20));
        assert_eq!(e.history_len(), 20, "excess history is dropped on the spot");
        let compacted = e.history_metrics();
        assert_eq!(
            compacted.cache_bytes,
            Some(0),
            "budget shrink releases cached rows"
        );
        assert!(
            !compacted.exact_bytes,
            "dynamic cell allocations remain estimated"
        );
        assert!(compacted.estimated_grid_bytes > 0);
        // …and the viewport can't be left scrolled past the new end.
        e.scroll_to_top();
        assert!(e.scroll_offset() <= 20);

        // Raising it takes effect as new output accumulates.
        e.set_history_budget(budget_for_rows(20, 200));
        feed_lines(&mut e, 400);
        assert_eq!(e.history_len(), 200, "the raised limit is used");
    }

    #[test]
    fn cold_history_compacts_losslessly_and_survives_reflow() {
        let (tx, _rx) = channel();
        let mut e = AlacrittyEngine::new(40, 5, tx, budget_for_rows(40, 500));
        e.advance(b"\x1b[38;2;12;200;155mCOLOR\x1b[0m cafe\xcc\x81 \x1b]8;;https://luvus.dev\x1b\\LINK\x1b]8;;\x1b\\\r\n");
        assert!(e.detection_text(100).contains("cafe\u{301}"));
        feed_lines(&mut e, 80);

        let before = e.history_metrics();
        assert!(before.compacted_rows.unwrap_or(0) > 0);
        assert!(before.allocated_cells.unwrap_or(0) > 0);

        e.scroll_to_top();
        let mut rendered = Vec::new();
        e.for_each_cell(&mut |_row, _column, symbol, style| {
            if symbol != " " {
                rendered.push((symbol.to_string(), style.fg));
            }
        });
        assert!(rendered.iter().any(|(symbol, _)| symbol == "e\u{301}"));
        assert!(rendered
            .iter()
            .any(|(symbol, color)| { symbol == "C" && *color == Color::Rgb(12, 200, 155) }));
        assert!(e.visible_rows().join("\n").contains("LINK"));
        let retained = e
            .retained_row_text(0)
            .expect("oldest feature row remains readable");
        assert!(retained.contains("cafe\u{301}"));

        e.resize(80, 8);
        e.scroll_to_top();
        let after = e.history_metrics();
        assert!(after.compacted_rows.unwrap_or(0) > 0);
        assert!(e.visible_rows().join("\n").contains("COLOR"));
        assert!(e.visible_rows().join("\n").contains("LINK"));
    }

    #[test]
    fn output_generation_advances_only_with_parser_input() {
        let (tx, _rx) = channel();
        let mut e = AlacrittyEngine::new(20, 5, tx, budget_for_rows(20, 20));
        assert_eq!(e.output_generation(), 0);
        e.advance(b"hello");
        assert_eq!(e.output_generation(), 1);
        e.resize(40, 10);
        assert_eq!(
            e.output_generation(),
            1,
            "resize uses the explicit force path"
        );
        e.advance(b" world");
        assert_eq!(e.output_generation(), 2);
    }

    #[test]
    fn retained_selection_preserves_unicode_scripts_and_clusters() {
        let samples = [
            "你好，世界",
            "こんにちは",
            "안녕하세요",
            "مرحبا",
            "שלום",
            "नमस्ते",
            "สวัสดี",
            "cafe\u{301}",
            "🖥️ coding",
            "👩‍💻 pair",
        ];

        for sample in samples {
            let (tx, _rx) = channel();
            let mut engine = AlacrittyEngine::new(80, 3, tx, budget_for_rows(80, 20));
            engine.advance(format!("\x1b[H\x1b[2J{sample}").as_bytes());
            let row = (0..engine.retained_row_count())
                .find(|row| engine.retained_row_text(*row).as_deref() == Some(sample))
                .expect("sample retained row");
            let layout = engine
                .retained_row_layout(row)
                .expect("retained row layout");
            let selected = engine
                .retained_selection_text(((row, 0), (row, layout.last_column())))
                .expect("selected row");
            assert_eq!(selected, sample, "Unicode selection changed {sample:?}");
        }
    }

    #[test]
    fn retained_selection_uses_visual_columns_for_mixed_cjk_text() {
        let (tx, _rx) = channel();
        let mut engine = AlacrittyEngine::new(40, 4, tx, budget_for_rows(40, 20));
        engine.advance("\x1b[H\x1b[2J你好，hello.\r\nمرحبا world".as_bytes());
        let first = (0..engine.retained_row_count())
            .find(|row| engine.retained_row_text(*row).as_deref() == Some("你好，hello."))
            .expect("first retained row");
        let second = (0..engine.retained_row_count())
            .find(|row| engine.retained_row_text(*row).as_deref() == Some("مرحبا world"))
            .expect("second retained row");

        assert_eq!(
            engine
                .retained_selection_text(((first, 0), (first, 3)))
                .as_deref(),
            Some("你好")
        );
        assert_eq!(
            engine
                .retained_selection_text(((first, 1), (first, 2)))
                .as_deref(),
            Some("你好"),
            "starting on a wide spacer still includes its complete glyph"
        );
        let second_layout = engine
            .retained_row_layout(second)
            .expect("second row layout");
        assert_eq!(
            engine
                .retained_selection_text(((first, 0), (second, second_layout.last_column())))
                .as_deref(),
            Some("你好，hello.\nمرحبا world")
        );
    }

    #[test]
    fn retained_selection_keeps_the_drag_left_edge_on_middle_rows() {
        let (tx, _rx) = channel();
        let mut engine = AlacrittyEngine::new(40, 5, tx, budget_for_rows(40, 20));
        engine.advance(b"\x1b[H\x1b[2J - first\r\n - second\r\n - third");
        let mut rows = Vec::new();
        engine.for_each_retained_row(&mut |row, text| {
            if text.starts_with(" - ") {
                rows.push(row);
            }
        });
        assert_eq!(rows.len(), 3);

        assert_eq!(
            engine
                .retained_selection_text(((rows[0], 1), (rows[2], 7)))
                .as_deref(),
            Some("- first\n- second\n- third")
        );
    }

    #[test]
    fn scrolled_back_still_renders_and_copies_history() {
        let (tx, _rx) = channel();
        let mut e = AlacrittyEngine::new(40, 6, tx, budget_for_rows(40, 2_000));
        e.advance(b"OLDEST\r\n");
        feed_lines(&mut e, 40);

        let cells = |e: &AlacrittyEngine| {
            let mut n = 0usize;
            e.for_each_cell(&mut |_r, _c, sym, _cell| {
                if sym != " " {
                    n += 1
                }
            });
            n
        };
        assert!(cells(&e) > 0, "live screen draws");

        e.scroll_to_top();
        assert!(e.scroll_offset() > 0, "we are in history");
        assert!(
            cells(&e) > 0,
            "the top of history must still draw — this rendered blank before"
        );
        let visible = e.visible_rows().join("\n");
        assert!(
            visible.contains("OLDEST"),
            "history text is selectable/copyable: {visible:?}"
        );
    }

    #[test]
    fn rows_text_dumps_full_history_oldest_first() {
        let (tx, _rx) = channel();
        let mut e = AlacrittyEngine::new(40, 6, tx, budget_for_rows(40, 2_000));
        feed_lines(&mut e, 40); // line0..line39; only ~6 fit the live screen
        let mut rows = Vec::new();
        e.for_each_retained_row(&mut |_index, line| rows.push(line.to_string()));
        assert_eq!(e.retained_row_count(), rows.len());
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(e.retained_row_text(index).as_ref(), Some(row));
        }
        assert_eq!(e.retained_row_text(rows.len()), None);
        let i0 = rows
            .iter()
            .position(|r| r.contains("line0"))
            .expect("oldest history line present");
        let i39 = rows
            .iter()
            .position(|r| r.contains("line39"))
            .expect("newest live line present");
        assert!(i0 < i39, "oldest first: {i0} < {i39}");
        assert_eq!(e.scroll_offset(), 0, "reading rows_text is read-only");
    }

    #[test]
    fn scroll_to_lands_and_clamps() {
        let (tx, _rx) = channel();
        let mut e = AlacrittyEngine::new(40, 6, tx, budget_for_rows(40, 2_000));
        feed_lines(&mut e, 40);
        let hist = e.history_len();
        assert!(hist > 0, "there is history to land in");
        e.scroll_to(hist);
        assert_eq!(e.scroll_offset(), hist, "landed at the requested offset");
        e.scroll_to(hist + 100);
        assert_eq!(e.scroll_offset(), hist, "clamped to the history length");
        e.scroll_to(0);
        assert_eq!(e.scroll_offset(), 0, "offset 0 returns to the live bottom");
    }

    #[test]
    fn for_each_cell_emits_the_whole_grapheme_cluster() {
        let (tx, _rx) = channel();
        let mut e = AlacrittyEngine::new(40, 3, tx, budget_for_rows(40, 200));
        // 🖥️ = U+1F5A5 (desktop computer) + U+FE0F (VS16). Alacritty stores the
        // VS16 as a `zerowidth` attachment on the base cell; emitting only the
        // base char rendered a bare monochrome glyph or a tofu box.
        e.advance("🖥️A".as_bytes());

        let mut syms: Vec<(u16, String)> = Vec::new();
        e.for_each_cell(&mut |_r, c, sym, _cell| {
            if sym != " " {
                syms.push((c, sym.to_string()));
            }
        });

        let emoji = syms.iter().find(|(c, _)| *c == 0).map(|(_, s)| s.as_str());
        assert_eq!(
            emoji,
            Some("🖥\u{fe0f}"),
            "the base char and its VS16 must arrive together as one symbol"
        );
        assert!(
            syms.iter().any(|(_, s)| s == "A"),
            "the following glyph still renders: {syms:?}"
        );
    }

    #[test]
    fn detection_text_ignores_scrollback_offset() {
        let (tx, _rx) = channel();
        let mut e = AlacrittyEngine::new(40, 5, tx, budget_for_rows(40, 2_000));
        // An old turn that was working, now scrolled far above the live screen.
        e.advance(b"\xE2\xA0\xB9 Thinking... (esc to interrupt)\r\n");
        feed_lines(&mut e, 40);
        // The live bottom is quiet.
        e.advance(b"$ \r\n");

        let live = e.detection_text(14);
        assert!(
            !live.contains("esc to interrupt"),
            "live screen has no stale marker: {live:?}"
        );

        // Walk the whole history: at *every* offset the detection window must
        // still describe the live screen, so no scroll position can fabricate a
        // working marker.
        e.scroll_to_top();
        let top = e.scroll_offset();
        assert!(top > 0, "there is history to scroll through");
        e.scroll_to_bottom();
        for _ in 0..top {
            e.scroll(1);
            let at = e.detection_text(14);
            assert_eq!(
                at,
                live,
                "detection text changed at scroll offset {}",
                e.scroll_offset()
            );
            assert!(
                !at.contains("esc to interrupt"),
                "scrolling resurrected an old working marker at offset {}",
                e.scroll_offset()
            );
        }
    }

    #[test]
    fn scrollback_offset_moves_clamps_and_resets() {
        let (tx, _rx) = channel();
        let mut e = AlacrittyEngine::new(20, 5, tx, budget_for_rows(20, 2_000)); // 5 visible rows
        feed_lines(&mut e, 50); // 50 lines → ~45 in scrollback

        assert_eq!(e.scroll_offset(), 0, "starts live at the bottom");

        e.scroll(10);
        assert_eq!(e.scroll_offset(), 10, "scrolls up 10 lines into history");
        assert!(!e.cursor().visible, "cursor hidden while scrolled back");

        e.scroll_to_top();
        let top = e.scroll_offset();
        assert!(top > 10, "top of history is well above the live bottom");
        e.scroll(1000);
        assert_eq!(
            e.scroll_offset(),
            top,
            "cannot scroll past the top of history"
        );

        e.scroll(-1000);
        assert_eq!(e.scroll_offset(), 0, "cannot scroll below the live bottom");
        e.scroll(5);
        e.scroll_to_bottom();
        assert_eq!(e.scroll_offset(), 0, "snaps back to live");
        assert!(e.cursor().visible, "cursor returns once live");
    }

    #[test]
    fn alt_screen_retains_bounded_capture_history_without_host_scrolling() {
        let (tx, _rx) = channel();
        let mut e = AlacrittyEngine::new(20, 5, tx, budget_for_rows(20, 20));
        e.advance(b"\x1b[?1049h"); // enter the alternate screen
        assert!(e.alt_screen());
        for i in 0..20 {
            e.advance(format!("alternate {i}\r\n").as_bytes());
        }
        assert!(
            e.history_len() > 0,
            "scrolled-off alternate rows are retained"
        );
        let capture = e.backend_capture(CaptureMode::RecentUnwrapped, 20, false, 4096);
        assert!(
            capture.text.contains("alternate 1"),
            "capture reaches rows no longer visible: {:?}",
            capture.text
        );
        e.scroll(5);
        assert_eq!(
            e.scroll_offset(),
            0,
            "the host viewport still never scrolls an alternate-screen app"
        );

        e.advance(b"\x1b[?1049l");
        assert!(!e.alt_screen());
        assert_eq!(
            e.history_len(),
            0,
            "alternate history is reclaimed instead of leaking into the primary screen"
        );
    }

    #[test]
    fn alternate_history_displaces_primary_rows_only_as_it_grows() {
        let (tx, _rx) = channel();
        let mut engine = AlacrittyEngine::new(20, 5, tx, budget_for_rows(20, 20));
        feed_lines(&mut engine, 30);
        let primary_before = engine.history_len();
        assert_eq!(primary_before, 20);

        engine.advance(b"\x1b[?1049h");
        for i in 0..8 {
            engine.advance(format!("alternate {i}\r\n").as_bytes());
        }
        let alternate_rows = engine.history_len();
        assert!(alternate_rows > 0);
        engine.advance(b"\x1b[?1049l");

        let primary_after = engine.history_len();
        assert_eq!(
            primary_after + alternate_rows,
            primary_before,
            "alternate rows consume the shared budget one for one"
        );
        assert!(
            primary_after > primary_before / 2,
            "entering alternate mode alone does not discard half the primary transcript"
        );
    }

    #[test]
    fn alternate_scroll_mode_is_reported() {
        let (tx, _rx) = channel();
        let mut e = AlacrittyEngine::new(20, 5, tx, budget_for_rows(20, 2_000));
        // Alacritty follows the terminal default: alternate scrolling starts
        // enabled, and an application can explicitly turn it off.
        assert!(e.alternate_scroll());
        e.advance(b"\x1b[?1007l");
        assert!(!e.alternate_scroll());
        e.advance(b"\x1b[?1007h");
        assert!(e.alternate_scroll());
    }

    #[test]
    fn mouse_tracking_modes_are_detected() {
        let (tx, _rx) = channel();
        let mut e = AlacrittyEngine::new(20, 5, tx, budget_for_rows(20, 2_000));
        assert!(!e.mouse_report(), "no tracking by default");
        assert!(!e.sgr_mouse());
        // A TUI agent enabling normal + SGR mouse reporting (DECSET 1000, 1006).
        e.advance(b"\x1b[?1000h\x1b[?1006h");
        assert!(e.mouse_report(), "wheel should be forwarded to the app");
        assert!(e.sgr_mouse(), "reports use the SGR encoding");
        assert!(!e.mouse_drag(), "click-only tracking: no drag reports");
        // Button-event tracking (1002) adds press-and-move reporting.
        e.advance(b"\x1b[?1002h");
        assert!(e.mouse_drag(), "drag tracking requested");
        assert!(!e.mouse_motion(), "1002 is not any-motion hover tracking");
        // Any-motion tracking (1003) adds hover reporting too.
        e.advance(b"\x1b[?1003h");
        assert!(e.mouse_motion());
        // Disabling it hands the wheel back to luvus's scrollback.
        e.advance(b"\x1b[?1003l\x1b[?1002l\x1b[?1000l");
        assert!(!e.mouse_report());
        assert!(!e.mouse_drag());
        assert!(!e.mouse_motion());
    }

    #[test]
    fn pi_fullscreen_mouse_modes_are_detected() {
        let (tx, _rx) = channel();
        let mut e = AlacrittyEngine::new(20, 5, tx, budget_for_rows(20, 2_000));

        // Pi fullscreen enters the alternate screen, disables autowrap, and
        // enables normal, button-motion, focus, and SGR mouse reporting.
        e.advance(b"\x1b[?1049h\x1b[?7l\x1b[?1000h\x1b[?1002h\x1b[?1004h\x1b[?1006h");

        assert!(e.alt_screen());
        assert!(e.mouse_report());
        assert!(e.sgr_mouse());
    }

    #[test]
    fn codex_composer_region_finds_the_real_default_background_layout() {
        let (tx, _rx) = channel();
        let mut e = AlacrittyEngine::new(40, 8, tx, budget_for_rows(40, 2_000));
        // Codex leaves one blank padding row above and below its `›` prompt.
        e.advance("\x1b[2;1H› Write tests".as_bytes());

        assert_eq!(
            e.codex_composer_region(),
            Some(CodexComposerRegion { top: 0, bottom: 2 })
        );

        // A prompt-looking transcript line without the padding geometry must
        // not be restyled as the active composer.
        e.advance(b"\x1b[1;1Htranscript\x1b[2;1H");
        assert_eq!(e.codex_composer_region(), None);
    }

    #[test]
    fn backend_capture_is_bounded_and_never_replays_unsafe_controls() {
        let (tx, _rx) = channel();
        let mut engine = AlacrittyEngine::new(30, 4, tx, budget_for_rows(30, 200));
        engine.advance(b"\x1b[31mred\x1b[0m\r\n\x1b]52;c;SECRET\x1b\\safe\r\n");

        let ansi = engine.backend_capture(CaptureMode::Visible, 4, true, 512);
        assert!(ansi.text.contains("\x1b["), "safe SGR styling is retained");
        assert!(!ansi.text.contains("\x1b]"), "OSC is never replayed");
        assert!(
            !ansi.text.contains("SECRET"),
            "OSC payload is not terminal text"
        );
        assert!(ansi.text.contains("safe"));

        let bounded = engine.backend_capture(CaptureMode::Visible, 4, false, 5);
        assert!(bounded.text.len() <= 5);
        assert!(bounded.truncated);
        assert!(std::str::from_utf8(bounded.text.as_bytes()).is_ok());
    }

    #[test]
    fn recent_capture_joins_soft_wrapped_rows() {
        let (tx, _rx) = channel();
        let mut engine = AlacrittyEngine::new(5, 3, tx, budget_for_rows(5, 200));
        engine.advance(b"abcdefghij\r\nnext\r\n");
        let capture = engine.backend_capture(CaptureMode::RecentUnwrapped, 3, false, 512);
        assert!(capture.text.contains("abcdefghij"), "{:?}", capture.text);
        assert!(capture.text.contains("next"), "{:?}", capture.text);
        assert!(capture.lines <= 3);
    }
}
