use std::io;
use anyhow::Result;
use clap::Parser;
use crossterm::event::{self, KeyEvent, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    prelude::*,
    buffer::Buffer,
    buffer::Cell,
    layout::{Alignment, Rect},
    style::Stylize,
    text::Line,
    widgets::{Paragraph, Widget, Block, Clear, Padding},
    DefaultTerminal, Frame,
};
use std::collections::HashMap;
use clog::prelude::*;
use std::panic;
use std::process;
use std::io::Write;
use std::ffi::OsString;
use std::sync::Arc;
use std::sync::Mutex;
use std::fmt::{self, Debug, Formatter};

use crate::log::LogKeys::MA;
use crate::lines::*;
use crate::pattern::*;
use crate::cache::SearchType;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MarkType {
    None = 0,
    Mark = 1,
    Tag = 2,
    Hide = 3,
    Search = 4,
}

#[derive(Debug)]
struct MarkStyleSet {
    styles: Vec<Style>,
}

#[derive(Clone)]
pub struct MarkStyle {
    variant: MarkType,
    index: isize,
    styles: Arc<Vec<MarkStyleSet>>,
}

impl Debug for MarkStyle {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "MarkStyle {{ variant: {:?}, index: {} }}", self.variant, self.index)
    }
}

impl MarkStyle {
    pub fn cycle_forward(&mut self) {
        self.index += 1;
    }

    pub fn cycle_backward(&mut self) {
        self.index -= 1;
    }

    pub fn style(&self) -> Style {
        let len = self.styles[self.variant as usize].styles.len() as isize;
        self.styles[self.variant as usize].styles[(self.index % len) as usize]
    }

    pub fn get(&self, variant: MarkType) -> Self {
        let mut m = self.clone();
        m.variant = variant;
        m
    }

    pub fn new() -> Self {
        let mark_styles = vec![
            // None
            MarkStyleSet { styles: vec![Style::default()] },
            // Mark
            MarkStyleSet { styles: vec![
                Style::default().fg(Color::Gray).bg(Color::Blue),
                Style::default().fg(Color::Green).bg(Color::Red),
                Style::default().fg(Color::Black).bg(Color::Green),
                Style::default().fg(Color::Red).bg(Color::Yellow),
                Style::default().fg(Color::Cyan).bg(Color::Gray),
                Style::default().fg(Color::Gray).bg(Color::Magenta),
                Style::default().fg(Color::Red).bg(Color::Black),
            ] },
            // Tag
            MarkStyleSet { styles: vec![
                Style::default().fg(Color::Blue),
                Style::default().fg(Color::Red),
                Style::default().fg(Color::Green),
                Style::default().fg(Color::Yellow),
                Style::default().fg(Color::Gray),
                Style::default().fg(Color::Magenta),
                Style::default().fg(Color::DarkGray),
            ] },
            // Hide
            MarkStyleSet { styles: vec![
                Style::default().fg(Color::Blue).bg(Color::Black),
                Style::default().fg(Color::Red).bg(Color::Black),
                Style::default().fg(Color::Green).bg(Color::Black),
                Style::default().fg(Color::Yellow).bg(Color::Black),
                Style::default().fg(Color::Black).bg(Color::Black),
                Style::default().fg(Color::Magenta).bg(Color::Black),
                Style::default().fg(Color::Gray).bg(Color::Black),
            ] },
            // Search
            MarkStyleSet { styles: vec![Style::default().bold()] },
        ];
        MarkStyle {
            index: 0,
            variant: MarkType::None,
            styles: Arc::new(mark_styles),
        }
    }
}

#[macro_use]
extern crate clog;

mod log;
mod search;
mod pattern;
mod cache;
mod lines;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Direction {
    Forward,
    Backward,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Focus {
    Main,
    Search,
    Help,
}

#[derive(Debug)]
enum Undo {
    Pattern((PatternMode, PatternSet)),
    TagHide((LineId, PatternMode)),
}

#[derive(Debug)]
struct LogrokInner {
    cursor_x: i16,
    cursor_y: i16,
    // kept for reference as how the cursor is calculated, needed for resize
    area_width: u16,
    area_height: u16,
    first_line: LineId,
    line_offset: usize,
    exit: bool,
    patterns: PatternSet,
    lines: Lines,
    display_mode: DisplayMode,
    focus: Focus,
    current_search: String,
    last_search: Option<PatternId>,
    search_direction: Direction,
    search_match_type: MatchType,
    mark_style: MarkStyle,
    display_offset: bool,
    display_offset_len: usize,
    before_filter_pos: HashMap<usize, (LineId, usize, i16)>,
    status_message: Option<String>,
    overlong_fold: HashMap<LineId, (usize, usize)>,       // crop lines to this many display lines
    render_cursor: (u16, u16),
    indent: String,
    indent_chars: u16,
    help_first_line: usize,
    help: Help,
    undo_stack: Vec<Undo>,
    // the fields below are rebuilt on each render
    plines: Vec<ProcessedLine>,
    line_indexes: Vec<LineIndex>,
    // progress hack
    input_area: Rect,
    input_content: Vec<Cell>,
}

#[derive(Debug, Clone)]
struct Logrok {
    inner: Arc<Mutex<LogrokInner>>,
}

#[derive(Debug)]
struct LineIndex {
    line_ix: usize,  // index into the lines vector
    char_index: usize,
    line_part: usize,
}

impl LogrokInner {
    fn undo_push_pattern(&mut self, mode: PatternMode) {
        let p = self.patterns.clone();
        lD3!(MA, "push pattern to undo stack: {:?}", p);
        self.undo_stack.push(Undo::Pattern((mode, p)));
    }

    fn update_patterns(&mut self, mode: PatternMode) {
        match mode {
            PatternMode::Tagging => self.lines.update_patterns(SearchType::Tag, &self.patterns),
            PatternMode::Search => self.lines.update_patterns(SearchType::Search, &self.patterns),
            _ => (),
        }
    }

    fn add_pattern(&mut self, pattern: &str, match_type: MatchType, style: MarkStyle,
        mode: PatternMode) -> PatternId
    {
        let id = self.patterns.add(&pattern, match_type, style, mode);
        self.update_patterns(mode);

        id
    }

    fn remove_pattern(&mut self, id: PatternId) {
        let mode = self.patterns.get(id).mode;
        lD1!(MA, "mark: removing pattern: {} mode {:?}", id, mode);
        self.patterns.remove(id);
        self.update_patterns(mode);
    }

    // events that don't need the layout or may change the layout
    fn handle_event_before_layout(&mut self, key_event: &KeyEvent) -> bool {
        if key_event.modifiers.contains(KeyModifiers::CONTROL) {
            match key_event.code {
                KeyCode::Char('h') => self.help(),
                _ => false,
            }
        } else if key_event.modifiers.contains(KeyModifiers::ALT) {
            false
        } else {
            match key_event.code {
                KeyCode::Char('@') => self.offsets(),
                KeyCode::Char('q') => self.exit(),
                _ => false,
            }
        }
    }

    // events that need the layout. this must not change the layout. It is possible
    // to split an event in both before and after.
    fn handle_event_after_layout(&mut self, key_event: &KeyEvent) -> bool {
        if key_event.modifiers.contains(KeyModifiers::CONTROL) {
            let area_height = self.area_height;
            let cnt = match key_event.code {
                KeyCode::Char('e') => 1,
                KeyCode::Char('d') => area_height / 2,
                KeyCode::Char('f') => area_height,
                _ => 0,
            };
            for _ in 0..cnt {
                let scrolled = self.scroll_down();
                if cnt == 1 && scrolled && self.cursor_y > 0 {
                    self.move_cursor(0, -1);
                }
            }
            if cnt > 0 {
                return true;
            }
            let cnt = match key_event.code {
                KeyCode::Char('y') => 1,
                KeyCode::Char('u') => area_height / 2,
                KeyCode::Char('b') => area_height,
                _ => 0,
            };
            for _ in 0..cnt {
                let scrolled = self.scroll_up();
                if cnt == 1 && scrolled &&
                    self.cursor_y < (area_height - 1) as i16
                {
                    self.move_cursor(0, 1);
                }
            }
            if cnt > 0 {
                return true;
            }
            match key_event.code {
                KeyCode::Char('r') => self.redo(),
                _ => false,
            }
        } else if key_event.modifiers.contains(KeyModifiers::ALT) {
            let area_height = self.area_height;
            let cnt = match key_event.code {
                KeyCode::Char('e') => 1,
                KeyCode::Char('d') => area_height as usize / 2,
                KeyCode::Char('f') => area_height as usize,
                _ => 0,
            };
            if cnt > 0 {
                self.scroll_fold_up_down(cnt, Direction::Forward);
            }
            let cnt = match key_event.code {
                KeyCode::Char('y') => 1,
                KeyCode::Char('u') => area_height as usize / 2,
                KeyCode::Char('b') => area_height as usize,
                _ => 0,
            };
            if cnt > 0 {
                self.scroll_fold_up_down(cnt, Direction::Backward);
            }
            true
        } else {
            match key_event.code {
                KeyCode::Char('j') => self.move_cursor(0, 1),
                KeyCode::Char('k') => self.move_cursor(0, -1),
                KeyCode::Char('h') => self.move_cursor(-1, 0),
                KeyCode::Char('l') => self.move_cursor(1, 0),
                KeyCode::Char('J') => self.move_cursor(0, 2),
                KeyCode::Char('K') => self.move_cursor(0, -2),
                KeyCode::Char('H') => self.move_cursor(-5, 0),
                KeyCode::Char('L') => self.move_cursor(5, 0),
                KeyCode::Char('w') => self.move_word(MatchType::SmallWord, Direction::Forward),
                KeyCode::Char('W') => self.move_word(MatchType::BigWord, Direction::Forward),
                KeyCode::Char('b') => self.move_word(MatchType::SmallWord, Direction::Backward),
                KeyCode::Char('B') => self.move_word(MatchType::BigWord, Direction::Backward),
                KeyCode::Char('g') => self.move_start(),
                KeyCode::Char('G') => self.move_end(),
                KeyCode::Char('0') => self.start_of_line(),
                KeyCode::Char('$') => self.end_of_line(),
                KeyCode::Char('F') => self.fold_line(),
                KeyCode::Char('+') => self.fold_more_less(true),
                KeyCode::Char('-') => self.fold_more_less(false),
                KeyCode::Char('i') => self.set_indent(),
                KeyCode::Char('t') => self.tag_hide(true, PatternMode::Tagging),
                KeyCode::Char('T') => self.tag_hide(false, PatternMode::Tagging),
                KeyCode::Char('f') => self.display(Direction::Forward),
                KeyCode::Char('d') => self.display(Direction::Backward),
                KeyCode::Char('m') => self.mark(MatchType::SmallWord),
                KeyCode::Char('M') => self.mark(MatchType::BigWord),
                KeyCode::Char('c') => self.cycle_color(Direction::Forward),
                KeyCode::Char('C') => self.cycle_color(Direction::Backward),
                KeyCode::Char('/') => self.search(Direction::Forward, MatchType::Text),
                KeyCode::Char('&') => self.search(Direction::Forward, MatchType::Regex),
                KeyCode::Char('?') => self.search(Direction::Backward, MatchType::Text),
                KeyCode::Char('n') => self.search_cont(Direction::Forward),
                KeyCode::Char('N') => self.search_cont(Direction::Backward),
                KeyCode::Char('.') => self.mark_extend(true, Direction::Forward),
                KeyCode::Char(',') => self.mark_extend(false, Direction::Forward),
                KeyCode::Char('<') => self.mark_extend(true, Direction::Backward),
                KeyCode::Char('>') => self.mark_extend(false, Direction::Backward),
                KeyCode::Char('x') => self.tag_hide(true, PatternMode::Hiding),
                KeyCode::Char('X') => self.tag_hide(false, PatternMode::Hiding),
                KeyCode::Char('u') => self.undo(),
                // todo: fast movement with shift
                KeyCode::Left => self.move_cursor(-1, 0),
                KeyCode::Right => self.move_cursor(1, 0),
                KeyCode::Up => self.move_cursor(0, -1),
                KeyCode::Down => self.move_cursor(0, 1),
                _ => false,
            }
        }
    }

    fn handle_search_event_before_layout(&mut self, _key_event: &KeyEvent) -> bool {
        return false;
    }

    fn handle_search_event_after_layout(&mut self, key_event: &KeyEvent) -> bool {
        lD3!(MA, "search event: {:?}", key_event);
        if key_event.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }

        match key_event.code {
            KeyCode::Char(c) => {
                self.current_search.push(c);
                false
            }
            KeyCode::Backspace => {
                if self.current_search.is_empty() {
                    self.focus = Focus::Main;
                    return true;
                }
                self.current_search.pop();
                false
            }
            KeyCode::Enter => {
                self.focus = Focus::Main;
                let input = self.current_search.clone();
                self.current_search.clear();
                self.do_search(input);
                true
            }
            _ => false,
        }
    }

    fn handle_help_event_before_layout(&mut self, _key_event: &KeyEvent) -> bool {
        return false;
    }

    fn handle_help_event_after_layout(&mut self, key_event: &KeyEvent) -> bool {
       lD3!(MA, "help event: {:?}", key_event);

        if key_event.modifiers.contains(KeyModifiers::CONTROL) {
            match key_event.code {
                KeyCode::Char('h') => {
                    self.focus = Focus::Main;
                    true
                }
                _ => false,
            }
        } else {
            match key_event.code {
                KeyCode::Char('q') |
                KeyCode::Char(' ') |
                KeyCode::Enter => {
                    self.focus = Focus::Main;
                    true
                }
                KeyCode::Char('j') => {
                    self.help_first_line += 1;
                    true
                }
                KeyCode::Char('k') => {
                    if self.help_first_line > 0 {
                        self.help_first_line -= 1;
                    }
                    true
                }
                _ => false,
            }
        }
    }

    fn move_cursor(&mut self, dx: i16, mut dy: i16) -> bool {
        self.cursor_x = (self.cursor_x + dx).max(0).min(self.area_width as i16 - 1);
        let mut cursor_y = self.cursor_y;
        let area_height = self.area_height as i16;

        let mut moved = false;
        while dy > 0 {
            if cursor_y == area_height as i16 - 1 {
                moved |= self.scroll_down();
            } else {
                cursor_y += 1;
            }
            dy -= 1;
        }
        while dy < 0 {
            if cursor_y == 0 {
                moved |= self.scroll_up();
            } else {
                cursor_y -= 1;
            }
            dy += 1;
        }
        self.cursor_y = cursor_y;
        self.before_filter_pos.clear();

        moved
    }

    fn move_start(&mut self) -> bool {
        self.cursor_x = 0;
        self.cursor_y = 0;
        self.first_line = 0;
        self.line_offset = 0;
        self.lines.set_current_line(self.first_line);
        if let Some(id) = self.adjust_to_unfiltered_line(0) {
            self.first_line = id;
        }

        true
    }

    fn move_end(&mut self) -> bool {
        let mut last_line_id = self.lines.last_line_id();
        self.lines.set_current_line(self.first_line);
        if let Some(id) = self.adjust_to_unfiltered_line(last_line_id) {
            last_line_id = id;
        }

        self.cursor_x = 0;
        self.cursor_y = self.area_height as i16 - 1;

        lD3!(MA, "move_end: last_line_id: {}", last_line_id);
        self.first_line = last_line_id;

        let pline = self.get_line(last_line_id).unwrap();
        let parts = self.line_parts(&pline, self.area_width) as usize;
        self.move_line_under_cursor(last_line_id, parts - 1);

        true
    }

    fn move_word(&mut self, match_type: MatchType, direction: Direction) -> bool {
        let Some((pos, line_ix, line_part)) = self.resolve_cursor_position() else {
            return false;
        };
        let pline = &self.plines[line_ix];
        let linelen = pline.chars.len();
        let mut pos = if let Some(pos) = pos {
            pos
        } else {
            let parts = self.line_parts(pline, self.area_width) as usize;
            // set pos to non-indent part of the line
            if line_part == parts - 1 && self.cursor_x >= self.indent_chars as i16 {
                if direction == Direction::Forward {
                    return false;
                }
                linelen - 1
            } else {
                assert!(self.cursor_x < self.indent_chars as i16);
                self.area_width as usize +
                    (line_part - 1) * (self.area_width as usize - self.indent_chars as usize)
            }
        };
        lD5!(MA, "move_word: pos: {} line_ix: {} line_part: {}", pos, line_ix, line_part);

        let deliminator = match_type.delimiter();

        if direction == Direction::Forward {
            let invert = !deliminator.contains(pline.chars[pos].c);
            while pos < linelen - 1 && (invert ^ deliminator.contains(pline.chars[pos].c)) {
                pos += 1;
            }
        } else {
            if pos == 0 {
                return false;
            }
            let invert = !deliminator.contains(pline.chars[pos - 1].c);
            while pos > 0 && (invert ^ deliminator.contains(pline.chars[pos - 1].c)) {
                pos -= 1;
            }
        }
        lD5!(MA, "move_word: new pos: {}", pos);

        let (x, y) = self.cursor_from_pos_ix(pos, line_ix, self.area_width);
        self.cursor_x = x as i16;
        self.cursor_y = y as i16;

        false
    }

    fn exit(&mut self) -> bool {
        self.exit = true;
        false
    }

    fn scroll_down(&mut self) -> bool {
        lD4!(MA, "scroll_down: self.line_offset: {} indexes {:?}",
            self.line_offset, self.line_indexes);

        /*
         * don't scroll down if the bottom line is the last line
         */
        let mode = self.display_mode;
        let last_line_index = self.line_indexes.last().unwrap();
        let last_pline = &self.plines[last_line_index.line_ix];
        let last_parts = self.line_parts(last_pline, self.area_width);
        lD5!(MA, "scroll_down: last_line_index: {:?} last_parts: {}", last_line_index, last_parts);
        if last_line_index.line_part == last_parts - 1 && self.lines.next_line(SearchType::Tag,
            last_pline.line_id, &self.patterns, mode, false).is_none()
        {
            return false;
        }

        let Some(index1) = self.line_indexes.get(1) else {
            return false;
        };
        if index1.line_part > 0 && self.line_offset < index1.line_part {
            self.line_offset += 1;
            return true;
        }
        self.line_offset = 0;

        let first_line = self.first_line;
        let Some(next_line_id) = self.lines.next_line(SearchType::Tag, first_line,
            &self.patterns, mode, false) else
        {
            return false;
        };

        self.first_line = next_line_id;
        self.lines.set_current_line(self.first_line);
        return true;
    }

    fn scroll_up(&mut self) -> bool {
        lD4!(MA, "scroll_up: self.line_offset: {} indexes {:?}",
            self.line_offset, self.line_indexes);

        if self.line_offset > 0 {
            self.line_offset -= 1;
            return true;
        }

        let mode = self.display_mode;
        let first_line = self.first_line;
        let Some(line_id) = self.lines.prev_line(SearchType::Tag, first_line,
            &self.patterns, mode, false) else
        {
            return false;
        };
        self.first_line = line_id;
        self.lines.set_current_line(self.first_line);

        let pline = self.get_line(line_id).unwrap();
        let linelen = pline.chars.len() as u16;
        if linelen > self.area_width {
            self.line_offset = (linelen - self.area_width) as usize /
                (self.area_width as usize - self.indent_chars as usize) + 1;
        }
        return true;
    }

    fn start_of_line(&mut self) -> bool {
        let Some((_, line_ix, _)) = self.resolve_cursor_position() else {
            return false;
        };
        let (x, y) = self.cursor_from_pos_ix(0, line_ix, self.area_width);
        self.cursor_x = x as i16;
        self.cursor_y = y as i16;

        false
    }

    fn end_of_line(&mut self) -> bool {
        let Some((_, line_ix, _)) = self.resolve_cursor_position() else {
            return false;
        };
        let (x, y) = self.cursor_from_pos_ix(self.plines[line_ix].chars.len() - 1,
            line_ix, self.area_width);
        self.cursor_x = x as i16;
        self.cursor_y = y as i16;

        false
    }

    fn fold_line(&mut self) -> bool {
        let Some((_, line_ix, _)) = self.resolve_cursor_position() else {
            return false;
        };
        let line_id = self.plines[line_ix].line_id;
        if self.overlong_fold.get(&line_id).is_some() {
            self.overlong_fold.remove(&line_id);
        } else {
            let parts = self.line_parts(&self.plines[line_ix], self.area_width) as usize;
            self.overlong_fold.insert(line_id, (parts.min(self.area_height as usize / 2), 0));
        }

        true
    }

    fn fold_more_less(&mut self, more: bool) -> bool {
        let Some((_, line_ix, _)) = self.resolve_cursor_position() else {
            return false;
        };
        let line_id = self.plines[line_ix].line_id;
        let full_line = self.lines.get(line_id, &self.patterns, None).unwrap();
        let parts = self.line_parts(&full_line, self.area_width) as usize;
        if let Some((lines, _)) = self.overlong_fold.get_mut(&line_id) {
            if more && *lines < parts {
                *lines += 1;
            } else if !more && *lines > 2 {
                *lines -= 1;
            }
            return true;
        }

        false
    }

    fn scroll_fold_up_down(&mut self, cnt: usize, direction: Direction) -> bool {
        let Some((_, line_ix, _)) = self.resolve_cursor_position() else {
            return false;
        };
        let line_id = self.plines[line_ix].line_id;
        let full_line = self.lines.get(line_id, &self.patterns, None).unwrap();
        let parts = self.line_parts(&full_line, self.area_width) as usize;
        if parts == 1 {
            return false;
        }
        if let Some((lines, first)) = self.overlong_fold.get_mut(&line_id) {
            if direction == Direction::Forward {
                for _ in 0..cnt {
                    if *first + *lines >= parts {
                        break;
                    }
                    *first += 1;
                }
            } else {
                *first = first.saturating_sub(cnt);
            }
            return true;
        }

        false
    }

    fn set_indent(&mut self) -> bool {
        self.indent_chars = self.cursor_x as u16;
        self.indent = vec![" "; self.indent_chars as usize].join("");

        true
    }

    // return : None if cursor is not on a line
    // return : Some(None, index, part) if cursor is on the whitespace part of the line
    // return : Some(Some(position), index, part) if cursor is on the text part of the line
    fn resolve_cursor_position(&self) -> Option<(Option<usize>, usize, usize)> {
        let Some(index) = self.line_indexes.get(self.cursor_y as usize) else {
            return None;
        };
        lD5!(MA, "cursor_y {} index {:?}", self.cursor_y, index);
        let pos = if index.line_part > 0 {
            if self.cursor_x < self.indent_chars as i16 {
                lD5!(MA, "returns None, line_ix: {} line_part: {}", index.line_ix, index.line_part);
                return Some((None, index.line_ix, index.line_part));
            }
            index.char_index + self.cursor_x as usize - self.indent_chars as usize
        } else {
            index.char_index + self.cursor_x as usize
        };
        if pos >= self.plines[index.line_ix].chars.len() {
            lD5!(MA, "returns None, line_ix: {} line_part: {}", index.line_ix, index.line_part);
            return Some((None, index.line_ix, index.line_part));
        }

        lD5!(MA, "returns Some({}), line_ix: {} line_part: {}", pos, index.line_ix,
            index.line_part);
        Some((Some(pos), index.line_ix, index.line_part))
    }

    fn cursor_from_pos_ix(&self, pos: usize, ix: usize, width: u16) -> (u16, u16) {
        // find new y position
        lD5!(MA, "cursor_from_pos_ix: pos: {} ix: {} width: {}", pos, ix, width);
        let mut y = 0;
        for i in 0..ix {
            let mut parts = self.line_parts(&self.plines[i], width);
            if i == 0 {
                parts -= self.line_offset;
            }
            y += parts;
            lD5!(MA, "line {} has {} parts new y {}", i, parts, y);
        }
        let mut chars_per_line = width as usize;
        let mut pos = pos;
        let mut x_off = 0;
        while pos >= chars_per_line {
            pos -= chars_per_line;
            y += 1;
            chars_per_line = width as usize - self.indent_chars as usize;
            x_off = self.indent_chars as usize;
        }

        lD5!(MA, "returns x: {} y: {}", pos + x_off, y);
        ((pos + x_off) as u16, y as u16)
    }

    fn cursor_from_pos_len(&self, pos: usize, width: u16) -> (u16, u16) {
        if pos < width as usize {
            return (pos as u16, 0);
        }
        let y = (pos - width as usize) / (width - self.indent_chars) as usize;
        let x = (pos - width as usize) % (width - self.indent_chars) as usize;

        return (x as u16 + self.indent_chars, (y + 1) as u16);
    }

    fn line_parts(&self, pline: &ProcessedLine, width: u16) -> usize {
        if pline.chars.len() <= width as usize {
            1
        } else {
            (pline.chars.len() - width as usize - 1) /
                ((width - self.indent_chars) as usize) + 2
        }
    }

    fn tag_hide(&mut self, all: bool, patmode: PatternMode) -> bool {
        let Some((pos, line_ix, line_part)) = self.resolve_cursor_position() else {
            return false;
        };

        let line = &self.plines[line_ix];
        let line_id = line.line_id;

        if all {
            if let Some(pos) = pos {
                if let Some(ref matches) = line.chars[pos].matches {
                    if matches.len() != 1 {
                        // don't do anything on more that one match, it might be confusing
                        self.status_message = Some("ambiguous selection".to_string());
                        return false;
                    }
                    let (id, _) = matches[0];
                    let mode = self.patterns.get(id).mode;
                    self.undo_push_pattern(mode);
                    let (new_mode, new_variant) = if mode == PatternMode::Marking {
                        if patmode == PatternMode::Tagging {
                            (PatternMode::Tagging, MarkType::Tag)
                        } else {
                            (PatternMode::Hiding, MarkType::Hide)
                        }
                    } else if mode == patmode {
                        (PatternMode::Marking, MarkType::Mark)
                    } else if mode == PatternMode::Search {
                        // convert search to tag
                        self.last_search = None;
                        (PatternMode::Tagging, MarkType::Tag)
                    } else {
                        return false;
                    };

                    lD1!(MA, "mark/hide: set pattern {} tagging to {:?}", id, new_mode);
                    let mode = self.patterns.get(id).mode;
                    self.patterns.with(id, |p| {
                        p.mode = new_mode;
                        p.style.variant = new_variant;
                    });
                    self.update_patterns(mode);
                    self.update_patterns(new_mode);

                    self.move_line_under_cursor(line_id, line_part);

                    return true;
                }
            }
        }

        lD3!(MA, "tag: line_ix: {} pos: {:?} id {}", line_ix, pos, line.line_id);
        lD5!(MA, "line_indexes: {:?}", self.line_indexes);
        let line_id = line.line_id;
        match patmode {
            PatternMode::Tagging => self.lines.toggle_tag(line_id),
            PatternMode::Hiding => self.lines.toggle_hide(line_id),
            _ => panic!("unexpected pattern mode {:?}", patmode),
        }
        self.undo_stack.push(Undo::TagHide((line_id, patmode)));

        self.move_line_under_cursor(line_id, line_part);

        true
    }

    fn undo(&mut self) -> bool {
        let Some(undo) = self.undo_stack.pop() else {
            lD3!(MA, "undo stack empty");
            return false;
        };

        match undo {
            Undo::Pattern((mode, p)) => {
                lD3!(MA, "undo pattern: {:?}", p);
                self.patterns = p;
                self.update_patterns(mode);
            }
            Undo::TagHide((line_id, mode)) => {
                lD3!(MA, "undo tag/hide: line_id: {} mode: {:?}", line_id, mode);
                match mode {
                    PatternMode::Tagging => self.lines.toggle_tag(line_id),
                    PatternMode::Hiding => self.lines.toggle_hide(line_id),
                    _ => panic!("unexpected pattern mode {:?}", mode),
                }
            }
        }

        true
    }

    fn redo(&mut self) -> bool {
        false
    }

    fn mark(&mut self, match_type: MatchType) -> bool {
        let Some((pos, line_ix, line_part)) = self.resolve_cursor_position() else {
            return false;
        };
        let Some(mut pos) = pos else {
            return false;
        };
        lD1!(MA, "mark: line: {} pos: {} char: {}", line_ix, pos,
            self.plines[line_ix].chars[pos].c);
        if let Some(ref matches) = self.plines[line_ix].chars[pos].matches {
            let matches = matches.clone();
            // only act on last match
            if let Some(&(id, _)) = matches.last() {
                // convert search to mark
                if self.patterns.get(id).mode == PatternMode::Search {
                    self.undo_push_pattern(PatternMode::Search);
                    // give it a new color
                    let match_index = self.mark_style.index;
                    self.mark_style.cycle_forward();
                    self.patterns.with(id, |p| {
                        p.mode = PatternMode::Marking;
                        p.style.variant = MarkType::Mark;
                        p.style.index = match_index;
                    });
                    self.update_patterns(PatternMode::Search);
                    if Some(id) == self.last_search {
                        self.last_search = None;
                    }
                    return true;
                }
                self.undo_push_pattern(self.patterns.get(id).mode);
                self.remove_pattern(id);
                let line_id = self.plines[line_ix].line_id;
                self.move_line_under_cursor(line_id, line_part);

                return true;
            }

            return false;
        }

        let deliminator = match_type.delimiter();

        let line = &self.plines[line_ix];
        if deliminator.contains(line.chars[pos].c) {
            return false;
        }
        while pos > 0 && !deliminator.contains(line.chars[pos - 1].c) {
            pos -= 1;
        }
        let mut pattern = String::new();
        for i in pos..line.chars.len() {
            if deliminator.contains(line.chars[i].c) {
                break;
            }
            pattern.push(line.chars[i].c);
        }

        lD1!(MA, "mark: pattern: {}", pattern);
        let style = self.mark_style.get(MarkType::Mark);
        self.mark_style.cycle_forward();
        self.undo_push_pattern(PatternMode::Marking);
        self.add_pattern(&pattern, match_type, style, PatternMode::Marking);

        true
    }

    fn cycle_color(&mut self, direction: Direction) -> bool {
        let Some((pos, line_ix, _)) = self.resolve_cursor_position() else {
            return false;
        };
        let Some(pos) = pos else {
            return false;
        };
        let line = &self.plines[line_ix];
        lD1!(MA, "mark: line: {} pos: {} char: {}", line_ix, pos, line.chars[pos].c);
        if let Some(ref matches) = line.chars[pos].matches {
            if matches.len() != 1 {
                // we don't know what to do when the char has multiple matches
                return false;
            }
            let (id, _) = matches[0];
            self.patterns.with(id, |p| {
                if direction == Direction::Forward {
                    p.style.cycle_forward();
                } else {
                    p.style.cycle_backward();
                }
            });

            return true;
        }

        return false;
    }

    fn mark_extend(&mut self, extend: bool, direction: Direction) -> bool {
        let Some((pos, line_ix, _)) = self.resolve_cursor_position() else {
            return false;
        };
        let pline = &self.plines[line_ix];
        let Some(mut pos) = pos else {
            return false;
        };
        lD1!(MA, "line: {} pos: {} char: {}", line_ix, pos, pline.chars[pos].c);
        if let Some(ref matches) = pline.chars[pos].matches {
            if matches.len() != 1 {
                // we don't know what to do when the char has multiple matches
                return false;
            }
            let idm = matches[0];
            let (id, _) = idm;
            loop {
                if extend {
                    if pline.chars[pos].matches.is_some() &&
                       pline.chars[pos].matches.as_ref().unwrap().contains(&idm)
                    {
                        if direction == Direction::Forward {
                            if pos == pline.chars.len() - 1 {
                                break;
                            }
                            pos += 1;
                        } else {
                            if pos == 0 {
                                break;
                            }
                            pos -= 1;
                        }
                        continue;
                    }
                    // extend the match
                    let c = pline.chars[pos].c;
                    self.patterns.with(id, |p| {
                        if direction == Direction::Forward {
                            p.pattern.push(c);
                        } else {
                            p.pattern.insert(0, c);
                        }
                        p.match_type = MatchType::Text;
                        lD1!(MA, "mark: pattern: {}", p.pattern);
                    });
                    break;
                } else {
                    self.patterns.with(id, |p| {
                        if p.pattern.len() > 1 {
                            if direction == Direction::Forward {
                                p.pattern.pop();
                            } else {
                                p.pattern.remove(0);
                            }
                        }
                        p.match_type = MatchType::Text;
                        lD1!(MA, "mark: pattern: {}", p.pattern);
                    });
                }
                break;
            }
            let mode = self.patterns.get(id).mode;
            self.update_patterns(mode);
        } else if extend {
            let c = pline.chars[pos].c;
            let style = self.mark_style.get(MarkType::Mark);
            self.mark_style.cycle_forward();
            self.add_pattern(&c.to_string(), MatchType::Text, style, PatternMode::Marking);
        }

        return true;
    }

    fn offsets(&mut self) -> bool {
        let line_id_len = self.lines.last_line_id().to_string().len();
        self.display_offset_len = line_id_len;
        self.display_offset = !self.display_offset;

        return true;
    }

    fn adjust_to_unfiltered_line(&mut self, line_id: LineId) -> Option<LineId> {
        lD2!(MA, "filter: current line {} is filtered", line_id);
        let mut res = self.lines.next_line(SearchType::Tag, line_id, &self.patterns,
            self.display_mode, true);
        if res.is_none() {
            lD2!(MA, "filter: trying next line backwards");
            res = self.lines.prev_line(SearchType::Tag, line_id, &self.patterns,
                self.display_mode, true);
        }
        let Some(id) = res else {
            lD2!(MA, "filter: nothing found, staying in normal mode");
            self.status_message = Some("nothing to display".to_string());
            self.display_mode = DisplayMode::Normal;
            return None;
        };

        Some(id)
    }

    fn get_line(&self, line_id: LineId) -> Option<ProcessedLine> {
        let Some(&(lines, mut first)) = self.overlong_fold.get(&line_id) else {
            return self.lines.get(line_id, &self.patterns, None);
        };
        assert!(lines >= 1);
        let width = self.area_width as usize;
        let indented = self.area_width as usize - self.indent_chars as usize;

        let crop_chars = Some(width + (lines + first - 1) * indented);
        let mut line = self.lines.get(line_id, &self.patterns, crop_chars)?;
        if first == 0 {
            return Some(line);
        }
        // lines - total number of lines to show, including the first line
        // first - number of lines to skip after the first line
        let parts = self.line_parts(&line, self.area_width) as usize;
        if first + lines > parts {
            first = parts - lines;
        }
        // cut out the first /first/ indented lines
        let cut_size = first * indented;
        line.chars.drain(width .. width + cut_size);

        Some(line)
    }

    fn move_line_under_cursor(&mut self, line_id: LineId, line_part: usize) {
        // we want line_id in display line line_ix. find lines backwards to find a suitable
        // first_line and offset
        let mut first_line = line_id;
        let mut lines_to_go_back = self.cursor_y as isize - line_part as isize;
        lD2!(MA, "move_under_cursor: line_id {} line_part: {} lines_to_go_back: {} first_line {}",
            line_id, line_part, lines_to_go_back, first_line);
        lD2!(MA, "cursor_x {} cursor_y: {}", self.cursor_x, self.cursor_y);
        let mut first_parts = None;
        while lines_to_go_back > 0 {
            let Some(prev_line_id) = self.lines.prev_line(SearchType::Tag, first_line,
                &self.patterns, self.display_mode, false) else
            {
                lD2!(MA, "can't get back any further");
                break;
            };
            let pline = self.get_line(prev_line_id).unwrap();
            first_line = prev_line_id;
            let parts = self.line_parts(&pline, self.area_width) as isize;
            lines_to_go_back -= parts;
            lD2!(MA, "line {} has {} parts, lines_to_go_back {}", prev_line_id, parts,
                lines_to_go_back);
            first_parts = Some(parts);
        }
        lD2!(MA, "first_line: {} lines_to_go_back: {} first_parts {:?}", first_line,
            lines_to_go_back, first_parts);
        self.first_line = first_line;
        self.lines.set_current_line(self.first_line);
        if lines_to_go_back > 0 {
            self.cursor_y -= lines_to_go_back as i16;
        }
        self.line_offset = 0;
        if let Some(first_parts) = first_parts {
            if lines_to_go_back < 0 && -lines_to_go_back < first_parts {
                self.line_offset = -lines_to_go_back as usize;
                lD2!(MA, "filter: line_offset: {}", self.line_offset);
            }
        }
    }

    fn display(&mut self, direction: Direction) -> bool {
        let old_mode = self.display_mode;

        if direction == Direction::Forward {
            match self.display_mode {
                DisplayMode::All => self.display_mode = DisplayMode::Normal,
                DisplayMode::Normal => self.display_mode = DisplayMode::Tagged,
                DisplayMode::Tagged => self.display_mode = DisplayMode::Manual,
                DisplayMode::Manual => return false,
            }
        } else {
            match self.display_mode {
                DisplayMode::Manual => self.display_mode = DisplayMode::Tagged,
                DisplayMode::Tagged => self.display_mode = DisplayMode::Normal,
                DisplayMode::Normal => self.display_mode = DisplayMode::All,
                DisplayMode::All => return false,
            }
        }

        let line_index = &self.line_indexes[self.cursor_y as usize];
        let line_ix = line_index.line_ix;
        let line_part = line_index.line_part;
        let line_id = self.plines[line_ix].line_id;
        let y = self.cursor_y;
        self.before_filter_pos.insert(old_mode as usize, (line_id, line_part, y));

        // move cursor to next unfiltered line
        let (line_id, line_part) = if let Some(&(id, part, y)) =
            self.before_filter_pos.get(&(self.display_mode as usize))
        {
            // cursor hasn't moved sind last mode change, try to restore the old position
            self.cursor_y = y as i16;
            (id, part)
        } else if let Some(new) = self.adjust_to_unfiltered_line(line_id) {
            if new != line_id {
                (new, 0)
            } else {
                (line_id, line_part)
            }
        } else {
            // if we can't find a line, stay in previous mode
            self.display_mode = old_mode;
            self.undo_stack.pop();
            return true;
        };

        self.move_line_under_cursor(line_id, line_part);

        true
    }

    fn search(&mut self, direction: Direction, match_type: MatchType) -> bool {
        self.focus = Focus::Search;
        self.current_search = String::new();
        self.search_direction = direction;
        self.search_match_type = match_type;

        false
    }

    // search string is collected, do the actual search
    fn do_search(&mut self, search: String) {
        lD5!(MA, "do_search: search: {}", search);
        if let Some(id) = self.last_search {
            self.remove_pattern(id);
            self.last_search = None;
        }
        if search.is_empty() {
            return;
        }
        // TODO: check if the pattern is valid
        let style = self.mark_style.get(MarkType::Search);
        let match_type = self.search_match_type;
        let id = self.add_pattern(&search, match_type, style, PatternMode::Search);
        self.last_search = Some(id);

        self.search_cont(Direction::Forward);
    }

    fn match_has_mode(&self, pline: &ProcessedLine, pos: usize, mode: PatternMode) -> bool
    {
        if let Some(ref matches) = pline.chars[pos].matches {
            for &(id, _) in matches {
                if self.patterns.get(id).mode == mode {
                    return true;
                }
            }
        }

        false
    }

    fn match_get_search_ix(&self, pline: &ProcessedLine, pos: usize) -> Option<usize>
    {
        if let Some(ref matches) = pline.chars[pos].matches {
            for &(id, ix) in matches {
                if self.patterns.get(id).mode == PatternMode::Search {
                    return Some(ix);
                }
            }
        }

        None
    }

    fn match_has_search_ix(&self, pline: &ProcessedLine, pos: usize, wanted_ix: usize) -> bool {
        if let Some(ref matches) = pline.chars[pos].matches {
            for &(_, ix) in matches {
                if ix == wanted_ix {
                    return true;
                }
            }
        }

        false
    }

    // return the start position of the search match, if any
    fn get_search_match_forward(&self, pline: &ProcessedLine, pos: usize, skip_current: bool)
        -> Option<usize>
    {
        let mut pos = pos;
        if skip_current {
            let ix = self.match_get_search_ix(pline, pos);
            if let Some(ix) = ix {
                while pos < pline.chars.len() {
                    if !self.match_has_search_ix(pline, pos, ix) {
                        break;
                    }
                    pos += 1;
                }
            }
        };
        while pos < pline.chars.len() {
            if self.match_has_mode(pline, pos, PatternMode::Search) {
                return Some(pos);
            }
            pos += 1;
        }
        None
    }

    // return the start position of the search match, if any
    fn get_search_match_backward(&mut self, pline: &ProcessedLine, pos: usize, skip_current: bool)
        -> Option<usize>
    {
        let mut pos = pos as isize;
        if skip_current {
            let ix = self.match_get_search_ix(pline, pos as usize);
            if let Some(ix) = ix {
                while pos >= 0 {
                    if !self.match_has_search_ix(pline, pos as usize, ix) {
                        break;
                    }
                    pos -= 1;
                }
            }
        };
        while pos >= 0 {
            if self.match_has_mode(pline, pos as usize, PatternMode::Search) {
                // found a match, now find the start of the match
                let Some(ix) = self.match_get_search_ix(pline, pos as usize) else {
                    return None;
                };
                while pos > 0 && self.match_has_search_ix(pline, pos as usize - 1, ix) {
                    pos -= 1;
                }
                return Some(pos as usize);
            }
            pos -= 1;
        }
        None
    }

    fn search_cont(&mut self, direction: Direction) -> bool {
        let search_dir = self.search_direction;
        if search_dir == direction {
            self.search_next()
        } else {
            self.search_prev()
        }
    }

    fn search_next(&mut self) -> bool {
        let (pos, ix, part) = match self.resolve_cursor_position() {
            Some(x) => x,
            None => (None, 0, 0),
        };
        let pos = if let Some(pos) = pos {
            pos
        } else if part == 0 {
            0
        } else {
            self.area_width as usize +
                (part - 1) * (self.area_width as usize - self.indent_chars as usize)
        };
        let line_id = self.plines[ix].line_id;
        // don't take line from cache, as the matches aren't up-to-date here
        let pline = self.get_line(line_id).unwrap();
        lD2!(MA, "search_next: pos: {} ix: {} part: {} line: {}", pos, ix, part, pline.line_id);
        if let Some(match_pos) = self.get_search_match_forward(&pline, pos, true) {
            lD2!(MA, "do_search: found match at {}", match_pos);
            let (x, y) = self.cursor_from_pos_ix(match_pos, ix, self.area_width);
            self.cursor_x = x as i16;
            self.cursor_y = y as i16;
            return true;
        }
        let mut res = self.lines.next_line(SearchType::Search, pline.line_id, &self.patterns,
            DisplayMode::Normal, false);
        lD2!(MA, "do_search: next_line: {:?}", res);
        if res.is_none() {
            self.lines.set_current_line(0);  // hint for FileSearch
            res = self.lines.next_line(SearchType::Search, 0, &self.patterns,
                DisplayMode::Normal, true);
            lD2!(MA, "do_search: next_line from 0: {:?}", res);
            if res.is_some() {
                self.status_message = Some("Search wrapped".to_string());
            }
        }
        let Some(line_id) = res else {
            lD2!(MA, "do_search: nothing found");
            self.status_message = Some("No matches".to_string());
            return false;
        };

        let pline = self.get_line(line_id).unwrap();
        lD10!(MA, "current line: {} {:?}", line_id, pline);
        let match_pos = self.get_search_match_forward(&pline, 0, false).unwrap();

        // if line is on screen, do not scroll
        let ix = self.line_indexes.iter().position(|x| self.plines[x.line_ix].line_id == line_id);
        if let Some(ix) = ix {
            let (x, y) = self.cursor_from_pos_len(match_pos, self.area_width);
            let y = y + ix as u16;
            if y < self.area_height {
                self.cursor_x = x as i16;
                self.cursor_y = y as i16;
                return true;
            }
        }

        lD2!(MA, "do_search: found match at {}", match_pos);
        let (x, y) = self.cursor_from_pos_len(match_pos, self.area_width);
        self.cursor_x = x as i16;
        self.cursor_y = y as i16;

        self.first_line = line_id;
        self.lines.set_current_line(self.first_line);
        self.line_offset = 0;

        true
    }

    fn search_prev(&mut self) -> bool {
        let (pos, ix, part) = match self.resolve_cursor_position() {
            Some(x) => x,
            None => (None, 0, 0),
        };
        let pos = if let Some(pos) = pos {
            pos
        } else if part == 0 {
            0
        } else {
            self.area_width as usize +
                (part - 1) * (self.area_width as usize - self.indent_chars as usize)
        };
        let line_id = self.plines[ix].line_id;
        // don't take line from cache, as the matches aren't up-to-date here
        let pline = self.get_line(line_id).unwrap();
        lD2!(MA, "search_prev: pos: {} ix: {} part: {} line: {}", pos, ix, part, pline.line_id);
        if let Some(match_pos) = self.get_search_match_backward(&pline, pos, true) {
            lD2!(MA, "do_search: found match at {}", match_pos);
            let (x, y) = self.cursor_from_pos_ix(match_pos, ix, self.area_width);
            self.cursor_x = x as i16;
            self.cursor_y = y as i16;
            return true;
        }
        let mut res = self.lines.prev_line(SearchType::Search, pline.line_id, &self.patterns,
            DisplayMode::Normal, false);
        lD2!(MA, "do_search: next_line: {:?}", res);
        if res.is_none() {
            let last_line_id = self.lines.last_line_id();
            self.lines.set_current_line(last_line_id); // hint for FileSearch
            res = self.lines.prev_line(SearchType::Search, last_line_id, &self.patterns,
                DisplayMode::Normal, true);
            lD2!(MA, "do_search: next_line from 0: {:?}", res);
        }
        let Some(line_id) = res else {
            lD2!(MA, "do_search: nothing found");
            self.status_message = Some("No matches".to_string());
            return false;
        };

        let pline = self.get_line(line_id).unwrap();
        lD10!(MA, "current line: {} {:?}", line_id, pline);
        let len = pline.chars.len();
        let match_pos = self.get_search_match_backward(&pline, len - 1, false).unwrap();

        // if line is on screen, do not scroll
        let ix = self.line_indexes.iter().position(|x| self.plines[x.line_ix].line_id == line_id);
        if let Some(ix) = ix {
            let (x, y) = self.cursor_from_pos_len(match_pos, self.area_width);
            let y = y + ix as u16;
            if y < self.area_height {
                self.cursor_x = x as i16;
                self.cursor_y = y as i16;
                return true;
            }
        }

        lD2!(MA, "do_search: found match at {}", match_pos);
        let (x, y) = self.cursor_from_pos_len(match_pos, self.area_width);
        self.cursor_x = x as i16;
        self.cursor_y = y as i16;

        self.first_line = line_id;
        self.lines.set_current_line(self.first_line);
        self.line_offset = 0;

        true
    }

    fn help(&mut self) -> bool {
        self.focus = Focus::Help;
        true
    }

    fn calculate_layout(&self, area: Rect) -> [Rect; 5] {
        /*
         * calculate layout
         */
        let [main_area, bottom_area] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)])
                .spacing(0)
                .areas(area);

        let [input_area, status_area] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(50)])
                .spacing(0)
                .areas(bottom_area);

        let marker_len = if self.display_offset {
            2 + self.display_offset_len as usize + 1
        } else {
            2
        };
        let [marker_area, log_area] =
            Layout::horizontal([Constraint::Length(marker_len as u16), Constraint::Fill(1)])
                .spacing(0)
                .areas(main_area);

        [main_area, log_area, marker_area, input_area, status_area]
    }

    fn process_event(&mut self, area: Rect, event: Option<Event>) {
        let [_, log_area, _, _, _] = self.calculate_layout(area);

        /*
         * Handle key events part 1
         */
        let focus = self.focus;

        let (key_event, mut recalc_lines) = if let Some(Event::Resize(_, _)) = event {
            (None, true)
        } else if let Some(Event::Key(key_event)) = event {
            if key_event.kind == KeyEventKind::Press {
                (Some(key_event), match focus {
                    Focus::Main => self.handle_event_before_layout(&key_event),
                    Focus::Search => self.handle_search_event_before_layout(&key_event),
                    Focus::Help => self.handle_help_event_before_layout(&key_event),
                })
            } else {
                (None, false)
            }
        } else {
            (None, false)
        };

        /*
         * calculate cursor position on area change
         */
        if log_area.width != self.area_width || log_area.height != self.area_height {
            lD3!(MA, "process: area change: {}x{} -> {}x{}",
                self.area_width, self.area_height, log_area.width, log_area.height);
            if let Some((pos, ix, part)) = self.resolve_cursor_position() {
                if let Some(pos) = pos {
                    // when on text, keep the cursor on the same character
                    let (x, y) = self.cursor_from_pos_ix(pos, ix, log_area.width);
                    self.cursor_x = x as i16;
                    self.cursor_y = y as i16;
                } else if self.cursor_x < self.indent_chars as i16 {
                    // when in indent whitespace, keep it in the same column and part
                    let parts = self.line_parts(&self.plines[ix], log_area.width);
                    let (_, y) = self.cursor_from_pos_ix(0, ix, log_area.width);
                    self.cursor_y = (y as usize + part.min(parts)) as i16;
                    self.cursor_x = self.cursor_x.min(log_area.width as i16 - 1);
                } else {
                    // in whitespace at end of line
                    // calculate offset after last position
                    let len = self.plines[ix].chars.len();
                    let (x_end, _) = self.cursor_from_pos_ix(len - 1, ix, self.area_width);
                    let off = self.cursor_x - x_end as i16;
                    let (x, y) = self.cursor_from_pos_ix(len - 1, ix, log_area.width);
                    self.cursor_x = (x as i16 + off).min(log_area.width as i16 - 1);
                    self.cursor_y = y as i16;
                }
            } else {
                // XXX leave unchanged?
            }
            self.area_width = log_area.width;
            self.area_height = log_area.height;
        }

        /*
         * Handle key events part 2
         */
        if let Some(key_event) = key_event {
            recalc_lines |= match focus {
                Focus::Main => self.handle_event_after_layout(&key_event),
                Focus::Search => self.handle_search_event_after_layout(&key_event),
                Focus::Help => self.handle_help_event_after_layout(&key_event),
            };
        }

        /*
         * build lines
         */
        lD5!(MA, "render: recalc_lines: {}", recalc_lines);
        recalc_lines |= self.plines.is_empty();
        while recalc_lines {
            recalc_lines = false;
            let mut state_lines = Vec::new();
            let skip = self.line_offset;
            let mut curr_line_id = self.first_line;
            let mut num_lines = 0;
            loop {
                lD5!(MA, "render: curr_line_id: {} num_lines {} skip {}",
                    curr_line_id, num_lines, skip);
                let mode = self.display_mode;
                let pline = self.get_line(curr_line_id).unwrap();
                let next_line_id = self.lines.next_line(SearchType::Tag, curr_line_id,
                    &self.patterns, mode, false);
                state_lines.push(pline.clone());
                let parts = self.line_parts(&pline, log_area.width);
                num_lines += parts;
                lD5!(MA, "render: next_line_id: {:?} parts {}", next_line_id, parts);
                if num_lines > skip && num_lines - skip >= log_area.height as usize {
                    lD5!(MA, "render: breaking at {} skip {} height {}", num_lines, skip,
                        log_area.height);
                    break;
                }
                if let Some(next_line_id) = next_line_id {
                    curr_line_id = next_line_id;
                } else {
                    break;
                }
            }
            self.plines = state_lines;

            while num_lines < log_area.height as usize {
                let scrolled = self.scroll_up();
                if scrolled {
                    num_lines += 1;
                    recalc_lines = true;
                } else {
                    break;
                }
            }
        }
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        // ignore everything if the area is too small
        lD3!(MA, "render: area: {}x{} indent_chars {}", area.width, area.height, self.indent_chars);
        if area.width < self.indent_chars + 3 {
            Paragraph::new(Text::raw("Window not wide enough"))
                .alignment(Alignment::Center)
                .render(area, buf);
            return;
        }
        if area.height < 3 {
            Paragraph::new(Text::raw("Window not high enough"))
                .render(area, buf);
            return;
        }

        let [main_area, log_area, marker_area, input_area, status_area] =
            self.calculate_layout(area);

        /*
         * render lines and build index array
         */
        let mut lines = Vec::new();
        let mut line_indexes = Vec::new();
        let mut skip = self.line_offset;
        'a: for (i, pline) in self.plines.iter().enumerate() {
            let mut ix = 0;
            let mut broken_into = 0;
            while ix < pline.chars.len() {
                let max_len = if ix == 0 {
                    log_area.width as usize
                } else {
                    (log_area.width - self.indent_chars) as usize
                };
                let len = max_len.min(pline.chars.len() - ix);
                if skip > 0 {
                    skip -= 1;
                } else {
                    let mut l = Line::default();
                    if broken_into != 0 {
                        l.spans.push(Span::raw(self.indent.clone()));
                    }
                    for i in ix..ix + len {
                        let sc = &pline.chars[i];
                        l.spans.push(Span::styled(sc.c.to_string(), sc.style.style()));
                    }
                    lines.push(l);
                    line_indexes.push(LineIndex {
                        line_ix: i,
                        char_index: ix,
                        line_part: broken_into,
                    });
                    if lines.len() == (log_area.height) as usize {
                        break 'a;
                    }
                }
                broken_into += 1;
                ix += len;
            }
        }
        self.line_indexes = line_indexes;

        /*
         * adjust cursor position if we don't have enough lines
         */
        if self.cursor_y >= self.line_indexes.len() as i16 {
            self.cursor_y = self.cursor_y.min(self.line_indexes.len() as i16 - 1);
            lD5!(MA, "adjusting cursor_y to {}", self.cursor_y);
        }

        lD3!(MA, "render: patterns: {:?}", self.patterns);

        /*
         * render marker area
         */
        let mut markers = Vec::new();
        for index in &self.line_indexes {
            let line = &self.plines[index.line_ix];
            let mut spans = Vec::new();
            if index.line_part == 0 && self.lines.is_hidden(line.line_id) {
                spans.push(Span::raw("H "));
            } else if index.line_part == 0 &&
                line.matches.iter().any(|&id| self.patterns.is_hiding(id))
            {
                spans.push(Span::raw("- "));
            } else if index.line_part == 0 && self.lines.is_tagged(line.line_id) {
                spans.push(Span::raw("T "));
            } else if index.line_part == 0 &&
                line.matches.iter().any(|&id| self.patterns.is_tagging(id))
            {
                spans.push(Span::raw("* "));
            } else if index.line_part > 0 && self.overlong_fold.contains_key(&line.line_id) {
                let (lines, first) = self.overlong_fold.get(&line.line_id).unwrap();
                if index.line_part == 1 && *first > 0 {
                    spans.push(Span::raw("F-"));
                } else if index.line_part == *lines - 1 && line.cropped {
                    spans.push(Span::raw("F+"));
                } else {
                    spans.push(Span::raw("F "));
                }
            };
            if self.display_offset && index.line_part == 0 {
                let line_id_len = self.display_offset_len;
                spans.push(Span::raw(format!("{:line_id_len$} ", line.line_id)).green());
            }
            markers.push(Line::from(spans));
        }
        while markers.len() < marker_area.height as usize {
            markers.push(Line::from("~ "));
        }

        /*
         * render input area
         */
        let mut spans = Vec::new();
        if self.focus == Focus::Search {
            if self.search_match_type == MatchType::Regex {
                spans.push(Span::raw("&"));
            } else if self.search_direction == Direction::Forward {
                spans.push(Span::raw("/"));
            } else {
                spans.push(Span::raw("?"));
            }
            spans.push(Span::raw(self.current_search.clone()));
        } else if let Some(ref message) = self.status_message {
            spans.push(Span::raw(message.clone()).blue().bold());
        } else {
            spans.push(Span::raw("^H").red().bold());
            spans.push(Span::raw(" Help "));
        }   
        let input = Line::from(spans);
        self.status_message = None;

        /*
         * render status area
         */
        // current cursor position
        let cursor_pos = format!("{:3}:{:2} ", self.cursor_x, self.cursor_y);

        // current position
        let line_id = self.plines[self.line_indexes[self.cursor_y as usize].line_ix].line_id;
        let position = (line_id as f64) / (self.lines.last_line_id() + 1) as f64 * 100.0;
        let position = format!("{:3.2}%", position);
        // display mode
        let display_mode = match self.display_mode {
            DisplayMode::Normal => "Normal",
            DisplayMode::Tagged => "Tagged",
            DisplayMode::All    => "All   ",
            DisplayMode::Manual => "Manual",
        };
        let status = vec![Line::from(vec![
            Span::raw(cursor_pos),
            Span::raw(position),
            " Show ".into(),
            display_mode.red().bold(),
        ])];

        Paragraph::new(lines)
            .render(log_area, buf);

        Paragraph::new(markers)
            .render(marker_area, buf);

        Paragraph::new(input)
            .style(Style::default().fg(Color::Black).bg(Color::Gray))
            .alignment(Alignment::Left)
            .render(input_area, buf);

        Paragraph::new(status)
            .style(Style::default().fg(Color::Black).bg(Color::Gray))
            .alignment(Alignment::Right)
            .render(status_area, buf);

        if self.focus == Focus::Search {
            self.render_cursor =
                (input_area.x + self.current_search.len() as u16 + 1, input_area.y);
        } else {
            self.render_cursor =
                (log_area.x + self.cursor_x as u16, log_area.y + self.cursor_y as u16);
        }

        // XXX progress hack: save contents of input area
        let mut input_content = Vec::new();
        for x in 0..input_area.width {
            input_content.push(buf.cell((input_area.x + x, input_area.y)).unwrap().clone());
        }
        self.input_area = input_area;
        self.input_content = input_content;

        if self.focus == Focus::Help && main_area.height > 4 {
            let max_area = Rect::new(2, 2, main_area.width - 4, main_area.height - 4);
            let _max_width = max_area.width as usize;
            let max_height = max_area.height as usize;
            /*
            let cols = if self.help.lines <= max_height {
                1
            } else if self.help.columns * 2 + 2 <= max_width {
                2
            } else {
                1
            };
            */
            let cols = 1;
            let (width, height) = if cols == 1 {
                (self.help.columns + 2, self.help.lines.min(max_height - 2) + 2)
            } else {
                (2 * self.help.columns + 4, (self.help.lines + 1) / 2 + 2)
            };
            let vertical = Layout::vertical(
                [Constraint::Fill(1), Constraint::Length(height as u16), Constraint::Fill(1)]);
            let [_, help_vertical, _] = vertical.areas(max_area);
            let horizontal = Layout::horizontal(
                [Constraint::Fill(1), Constraint::Length(width as u16), Constraint::Fill(1)]);
            let [_, help_area, _] = horizontal.areas(help_vertical);
            Clear::default()
                .render(help_area, buf);
            let block = Block::default()
                .padding(Padding::uniform(1))
                .style(Style::default().fg(Color::Black).bg(Color::LightGreen))
                .title_bottom(self.help.bottom.clone());
            let block_inner = block.inner(help_area);
            block.render(help_area, buf);
            let mut lines = self.help.help.iter().map(|x| x.clone()).collect::<Vec<_>>();
            if lines.len() - self.help_first_line < block_inner.height as usize {
                self.help_first_line = lines.len() - block_inner.height as usize;
            }
            lines.drain(0..self.help_first_line);
            Paragraph::new(lines)
                .render(block_inner, buf);
        }
    }
}

impl Logrok {
    pub fn area(terminal: &DefaultTerminal) -> Result<Rect> {
        let size = terminal.size()?;
        Ok(Rect::new(0, 0, size.width, size.height))
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let (tx_req, rx_req) = std::sync::mpsc::channel();
        let (tx_rsp, rx_rsp) = std::sync::mpsc::channel();
        let s = self.clone();
        let jh = std::thread::spawn(move || {
            loop {
                let Ok((event, area)) = rx_req.recv() else {
                    break;
                };
                let mut inner = s.inner.lock().unwrap();
                inner.process_event(area, Some(event));
                tx_rsp.send(()).unwrap();
            }
        });
        let mut inner = self.inner.lock().unwrap();
        let filesearch = inner.lines.get_file_search();
        inner.process_event(Self::area(terminal)?, None);
        while !inner.exit {
            let input_area = inner.input_area; // XXX progress hack
            drop(inner);
            terminal.draw(|frame| self.draw(frame))?;
            let event = self.poll_events()?;
            let area = Self::area(terminal)?;
            tx_req.send((event, area)).unwrap();
            let mut need_restore = false;
            loop {
                match rx_rsp.recv_timeout(std::time::Duration::from_millis(200)) {
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        let progress = filesearch.get_progress();
                        draw_progress(progress, input_area, terminal)?;
                        need_restore = true;
                    },
                    Err(e) => return Err(e.into()),
                    Ok(()) => break,
                }
            }
            inner = self.inner.lock().unwrap();
            if need_restore {
                restore_progress(terminal, input_area, &inner.input_content)?;
            }
            if inner.exit {
                break;
            }
        }
        drop(tx_req);
        jh.join().unwrap();
        Ok(())
    }

    fn draw(&self, frame: &mut Frame) {
        frame.render_widget(self, frame.area());
        let cursor = self.inner.lock().unwrap().render_cursor;
        frame.set_cursor_position(cursor);
    }

    fn poll_events(&mut self) -> io::Result<Event> {
        let event = loop {
            let event = event::read()?;
            lD1!(MA, "event: {:?}", event);
            match event {
                // it's important to check that the event is a key press event as
                // crossterm also emits key release and repeat events on Windows.
                Event::Key(_) |
                Event::Resize(_, _) => break event,
                _ => (),
            };
        };
        Ok(event)
    }
}

impl Widget for &Logrok {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mut inner = self.inner.lock().unwrap();
        inner.render(area, buf)
    }
}

fn draw_progress(progress: f32, area: Rect, terminal: &mut DefaultTerminal) -> Result<()> {
    terminal.hide_cursor()?;
    let b = terminal.backend_mut();
    let message = format!("Processing... {:.2}%", progress * 100.0);
    let mut spans = Vec::new();
        spans.push(Span::raw(message).blue().bold());
    let input = Line::from(spans);

    let fake_area = Rect::new(0, 0, area.width, 1);
    let mut fake_buf = Buffer::empty(fake_area);
    Paragraph::new(input)
        .style(Style::default().fg(Color::Black).bg(Color::Gray))
        .alignment(Alignment::Left)
        .render(fake_area, &mut fake_buf);

    let mut content = Vec::new();
    for x in 0..area.width {
        let cell = fake_buf.cell((fake_area.x + x, fake_area.y)).unwrap().clone();
        content.push((area.x + x, area.y, cell));
    }
    b.draw(content.iter().map(|(x, y, c)| (*x, *y, c)))?;
    ratatui::backend::Backend::flush(b)?;

    Ok(())
}

fn restore_progress(terminal: &mut DefaultTerminal, area: Rect, contents: &Vec<Cell>) -> Result<()>
{
    let b = terminal.backend_mut();
    let mut cont = Vec::new();
    for x in 0..area.width {
        cont.push((area.x + x, area.y, &contents[x as usize]));
    }
    b.draw(cont.into_iter())?;
    ratatui::backend::Backend::flush(b)?;
    terminal.show_cursor()?;

    Ok(())
}

#[derive(Debug)]
struct Help {
    help: Vec<Line<'static>>,
    lines: usize,
    columns: usize,
    bottom: Line<'static>,
}

fn build_help() -> Help {
        /*
           Movement
           h/j/k/l: left/down/up/right
           cursor keys: left/down/up/right
           H/J/K/L: left/down/up/right (faster)
           w/W/b/B: next/previous word/WORD
           ^e/^y: scroll up/down one line
           ^d/^u: scroll up/down half a page
           ^b/^f: scroll up/down a page
           g/G: go to start/end of file
           0/$: go to start/end of line
           alt-e/y/d/u/b/f: scroll folded lines

           Marking
           m/M: toggle mark word/WORD under cursor
           >/<: extend marking to right/left

           Tagging/Hiding
           t/x: toggle tag/hide match under cursor
                or full line if not on a match
           T/X: toggle tag/hide full line only
           c: cycle color of mark

           Searching
           //?: search forward/backward
           &: regex search (forward)
           n/N: next/previous search match

           Display
           f: show All->Normal->Tagged->Manual
           d: show Manual->Tagged->Normal->All
           @: toggle display of line offsets
           F: fold current (overlong) line
           +/-: increase/decrease fold size
           i: set indent column

           Various
           u/^R: undo/redo
           q: quit
           ^H: toggle display of this help
        */

    let text = Style::default();
    let key = Style::default().bold();
    let sep_style = Style::default().fg(Color::DarkGray);
    let sep = Span::styled("/", sep_style);
    let heading = Style::default().bold();

    let help = vec![
        Line::from(vec![Span::styled("Movement", heading)]).alignment(Alignment::Center),
        Line::from(vec![
            Span::styled("h", key), sep.clone(),
            Span::styled("j", key), sep.clone(),
            Span::styled("k", key), sep.clone(),
            Span::styled("l", key),
            Span::styled(": left/down/up/right", text)]),
        Line::from(vec![
            Span::styled("cursor keys", key),
            Span::styled(": left/down/up/right", text)]),
        Line::from(vec![
            Span::styled("H", key), sep.clone(),
            Span::styled("J", key), sep.clone(),
            Span::styled("K", key), sep.clone(),
            Span::styled("L", key),
            Span::styled(": left/down/up/right (faster)", text)]),
        Line::from(vec![
            Span::styled("w", key), sep.clone(),
            Span::styled("W", key), sep.clone(),
            Span::styled("b", key), sep.clone(),
            Span::styled("B", key),
            Span::styled(": next/previous word/WORD", text)]),
        Line::from(vec![
            Span::styled("^e", key), sep.clone(),
            Span::styled("^y", key),
            Span::styled(": scroll up/down one line", text)]),
        Line::from(vec![
            Span::styled("^d", key), sep.clone(),
            Span::styled("^u", key),
            Span::styled(": scroll up/down half a page", text)]),
        Line::from(vec![
            Span::styled("^b", key), sep.clone(),
            Span::styled("^f", key),
            Span::styled(": scroll up/down a page", text)]),
        Line::from(vec![
            Span::styled("g", key), sep.clone(),
            Span::styled("G", key),
            Span::styled(": go to start/end of file", text)]),
        Line::from(vec![
            Span::styled("0", key), sep.clone(),
            Span::styled("$", key),
            Span::styled(": go to start/end of line", text)]),
            Line::from(vec![
            Span::styled("alt-e", key), sep.clone(),
            Span::styled("y", key), sep.clone(),
            Span::styled("d", key), sep.clone(),
            Span::styled("u", key), sep.clone(),
            Span::styled("b", key), sep.clone(),
            Span::styled("f", key),
            Span::styled(": scroll folded lines", text)]),
        Line::from(vec![]),
        Line::from(vec![Span::styled("Marking", heading)]).alignment(Alignment::Center),
        Line::from(vec![
            Span::styled("m", key), sep.clone(),
            Span::styled("M", key),
            Span::styled(": toggle mark word/WORD under cursor", text)]),
        Line::from(vec![
            Span::styled(">", key), sep.clone(),
            Span::styled("<", key),
            Span::styled(": extend marking to right/left", text)]),
        Line::from(vec![]),
        Line::from(vec![Span::styled("Tagging/Hiding", heading)]).alignment(Alignment::Center),
        Line::from(vec![
            Span::styled("t", key), sep.clone(),
            Span::styled("x", key),
            Span::styled(": toggle tag/hide match under cursor", text)]),
        Line::from(vec![
            Span::styled("t", key), sep.clone(),
            Span::styled("x", key),
            Span::styled(": toggle tag/hide full line", text)]),
        Line::from(vec![
            Span::styled("c", key), sep.clone(),
            Span::styled("C", key),
            Span::styled(": cycle color of mark", text)]),
        Line::from(vec![]),
        Line::from(vec![Span::styled("Searching", heading)]).alignment(Alignment::Center),
        Line::from(vec![
            Span::styled("/", key), sep.clone(),
            Span::styled("?", key),
            Span::styled(": search forward/backward", text)]),
        Line::from(vec![
            Span::styled("&", key),
            Span::styled(": regex search (forward)", text)]),
        Line::from(vec![
            Span::styled("n", key), sep.clone(),
            Span::styled("N", key),
            Span::styled(": next/previous search match", text)]),
        Line::from(vec![]),
        Line::from(vec![Span::styled("Display", heading)]).alignment(Alignment::Center),
        Line::from(vec![
            Span::styled("f", key),
            Span::styled(": toggle display of only tagged lines", text)]),
        Line::from(vec![
            Span::styled("F", key),
            Span::styled(": toggle display of hidden lines", text)]),
        Line::from(vec![
            Span::styled("@", key),
            Span::styled(": toggle display of line offsets", text)]),
        Line::from(vec![
            Span::styled("o", key),
            Span::styled(": fold current (overlong) line", text)]),
        Line::from(vec![
            Span::styled("+", key), sep.clone(),
            Span::styled("-", key),
            Span::styled(": in-/decrease fold size", text)]),
        Line::from(vec![
            Span::styled("i", key),
            Span::styled(": set indent column", text)]),
        Line::from(vec![]),
        Line::from(vec![Span::styled("Various", heading)]).alignment(Alignment::Center),
        Line::from(vec![
            Span::styled("u", key),
            Span::styled(": undo", text)]),
        Line::from(vec![
            Span::styled("q", key),
            Span::styled(": quit", text)]),
        Line::from(vec![
            Span::styled("^H", key),
            Span::styled(": toggle display of this help", text)]),
    ];
    let bottom = Line::from(vec![
            Span::styled("j", key), sep.clone(),
            Span::styled("k", key),
            Span::styled(": scroll ", text),
            Span::styled("q", key),
            Span::styled(": close help", text),
    ]).alignment(Alignment::Center);
    let mut columns = 0;
    for line in &help {
        let mut len = 0;
        for span in line {
            len += span.content.chars().count();
        }
        columns = columns.max(len);
    }
    Help {
        lines: help.len(),
        help,
        columns,
        bottom,
    }
}

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Logging configuration, can be given multiple times
    #[arg(short='l', long)]
    log: Vec<String>,

    #[arg(short='o', long)]
    output: Option<String>,

    #[arg(trailing_var_arg = true, allow_hyphen_values = false, hide = true)]
    files: Vec<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.files.len() != 1 {
        return Err(anyhow::anyhow!("Expected exactly one file"));
    }

    let mut facade = None;
    let mut level = None;
    if let Some(o) = &cli.output {
        let logfile: &'static str = o.to_string().leak();
        level = Some(Level::Info);
        facade = Some(FacadeVariant::LogFile(logfile));
    }
    log::LogKeys::clog_init("logrok", level, facade, Some(Options::default() - FUNC + TID))?;

    let v: Vec<&str> = cli.log.iter().map(|s| &**s).collect();
    CLog::set_mod_level(v)?;

    let orig_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        orig_hook(panic_info);
        let _ = std::io::stderr().flush();
        process::exit(1);
    }));

    let filename = OsString::from(&cli.files[0]);

    let mut terminal = ratatui::init();
    terminal.clear()?;
    let indent = vec![" "; 79].join("");
    let mark_style = MarkStyle::new();
    let app_result = Logrok {
        inner: Arc::new(Mutex::new(LogrokInner {
            exit: false,
            cursor_x: 0,
            cursor_y: 0,
            area_width: 1,
            area_height: 1,
            first_line: 0,
            line_offset: 0,
            patterns: PatternSet::new(mark_style.clone()),
            lines: Lines::new(&filename)?,
            display_mode: DisplayMode::Normal,
            mark_style,
            display_offset: false,
            display_offset_len: 0,
            focus: Focus::Main,
            before_filter_pos: HashMap::new(),
            current_search: String::new(),
            search_direction: Direction::Forward,
            search_match_type: MatchType::Text,
            last_search: None,
            status_message: None,
            plines: Vec::new(),
            line_indexes: Vec::new(),
            render_cursor: (0, 0),
            indent_chars: indent.chars().count() as u16,
            indent,
            overlong_fold: HashMap::new(),
            help_first_line: 0,
            help: build_help(),
            undo_stack: Vec::new(),
            input_area: Rect::default(),
            input_content: Vec::new(),
        })),
    }.run(&mut terminal);
    // move to sane position in case the terminal does not have an altscreen
    let size = terminal.size()?;
    terminal.set_cursor_position((0, size.height - 1))?;
    terminal.show_cursor()?;
    println!("");
    ratatui::restore();
    app_result
}
