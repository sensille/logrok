use std::io;
use anyhow::Result;
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    prelude::*,
    buffer::Buffer,
    layout::{Alignment, Rect},
    style::Stylize,
    text::Line,
    widgets::{Paragraph, Widget, Block, Clear, Padding},
    DefaultTerminal, Frame,
};
use std::cell::RefCell;
use std::collections::HashMap;
use clog::prelude::*;
use std::panic;
use std::process;
use std::io::Write;
use std::ffi::OsString;
use std::rc::Rc;
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
    styles: Rc<Vec<MarkStyleSet>>,
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
            styles: Rc::new(mark_styles),
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

#[derive(Debug)]
struct State {
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
    in_search_input: bool,
    current_search: String,
    last_search: Option<PatternId>,
    search_direction: Direction,
    search_match_type: MatchType,
    mark_style: MarkStyle,
    display_offset: bool,
    before_filter_pos: HashMap<usize, (LineId, usize, i16)>,
    display_help: bool,
    status_message: Option<String>,
    // the fields below are rebuilt on each render
    plines: Vec<ProcessedLine>,
    line_indexes: Vec<LineIndex>,
}

impl State {
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
}

#[derive(Debug)]
struct LineIndex {
    line_ix: usize,  // index into the lines vector
    char_index: usize,
    line_part: usize,
}

#[derive(Debug)]
struct App {
    state: RefCell<State>,
    render_cursor: RefCell<(u16, u16)>,
    event: Option<Event>,
    indent: String,
    indent_chars: u16,
    help: Help,
}

impl App {
    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.state.borrow().exit {
            terminal.draw(|frame| self.draw(frame))?;
            let quit = self.poll_events()?;
            if quit {
                break;
            }
        }
        Ok(())
    }

    fn draw(&self, frame: &mut Frame) {
        frame.render_widget(self, frame.area());
        frame.set_cursor_position((self.render_cursor.borrow().0 , self.render_cursor.borrow().1));
    }

    fn poll_events(&mut self) -> io::Result<bool> {
        let event = event::read()?;
        self.event = None;
        lD1!(MA, "event: {:?}", event);
        match event {
            // it's important to check that the event is a key press event as
            // crossterm also emits key release and repeat events on Windows.
            Event::Key(key_event) if key_event.kind == KeyEventKind::Press => {
                if key_event.code == KeyCode::Char('q') {
                    self.exit();
                    return Ok(true);
                } else {
                    self.event = Some(event);
                }
            }
            Event::Resize(_, _) => self.event = Some(event),
            _ => {}
        };
        Ok(false)
    }

    // events that don't need the layout or may change the layout
    fn handle_event_before_layout(&self) -> bool {
       if let Some(Event::Resize(_, _)) = self.event {
            return true;
        }
       let Some(Event::Key(key_event)) = self.event else {
            return false;
        };
        if key_event.kind != KeyEventKind::Press {
            return false;
        }

        if key_event.modifiers.contains(KeyModifiers::CONTROL) {
            match key_event.code {
                KeyCode::Char('h') => self.help(),
                _ => false,
            }
        } else {
            match key_event.code {
                KeyCode::Char('@') => self.offsets(),
                _ => false,
            }
        }
    }

    // events that need the layout. this must not change the layout. It is possible
    // to split an event in both before and after.
    fn handle_event_after_layout(&self) -> bool {
       let Some(Event::Key(key_event)) = self.event else {
            return false;
        };
        if key_event.kind != KeyEventKind::Press {
            return false;
        }

        if key_event.modifiers.contains(KeyModifiers::CONTROL) {
            let area_height = self.state.borrow().area_height;
            let cnt = match key_event.code {
                KeyCode::Char('e') => 1,
                KeyCode::Char('d') => area_height / 2,
                KeyCode::Char('f') => area_height,
                _ => 0,
            };
            for _ in 0..cnt {
                let scrolled = self.scroll_down();
                if cnt == 1 && scrolled && self.state.borrow().cursor_y > 0 {
                    self.move_cursor(0, -1);
                }
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
                    self.state.borrow().cursor_y < (area_height - 1) as i16
                {
                    self.move_cursor(0, 1);
                }
            }
            true
        } else {
            match key_event.code {
                KeyCode::Char('q') => self.exit(),
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
                // todo: fast movement with shift
                KeyCode::Left => self.move_cursor(-1, 0),
                KeyCode::Right => self.move_cursor(1, 0),
                KeyCode::Up => self.move_cursor(0, -1),
                KeyCode::Down => self.move_cursor(0, 1),
                _ => false,
            }
        }
    }

    fn handle_search_event_before_layout(&self) -> bool {
        return false;
    }

    fn handle_search_event_after_layout(&self) -> bool {
       lD3!(MA, "search event: {:?}", self.event);
       let Some(Event::Key(key_event)) = self.event else {
            return false;
        };
        if key_event.kind != KeyEventKind::Press {
            return false;
        }

        if key_event.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }

        let mut state = self.state.borrow_mut();
        match key_event.code {
            KeyCode::Char(c) => {
                state.current_search.push(c);
                false
            }
            KeyCode::Backspace => {
                if state.current_search.is_empty() {
                    state.in_search_input = false;
                    return true;
                }
                state.current_search.pop();
                false
            }
            KeyCode::Enter => {
                state.in_search_input = false;
                let input = state.current_search.clone();
                state.current_search.clear();
                drop(state);
                self.do_search(input);
                true
            }
            _ => false,
        }
    }

    fn move_cursor(&self, dx: i16, mut dy: i16) -> bool {
        let mut state = self.state.borrow_mut();
        state.cursor_x = (state.cursor_x + dx).max(0).min(state.area_width as i16 - 1);
        let mut cursor_y = state.cursor_y;
        let area_height = state.area_height as i16;
        drop(state);

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
        let mut state = self.state.borrow_mut();
        state.cursor_y = cursor_y;
        state.before_filter_pos.clear();

        moved
    }

    fn move_start(&self) -> bool {
        let mut state = self.state.borrow_mut();
        state.cursor_x = 0;
        state.cursor_y = 0;
        state.first_line = 0;
        state.line_offset = 0;
        state.lines.set_current_line(state.first_line);
        if let Some(id) = self.adjust_to_unfiltered_line(&mut state, 0) {
            state.first_line = id;
        }

        true
    }

    fn move_end(&self) -> bool {
        let mut state = self.state.borrow_mut();
        let mut last_line_id = state.lines.last_line_id();
        state.lines.set_current_line(state.first_line);
        if let Some(id) = self.adjust_to_unfiltered_line(&mut state, last_line_id) {
            last_line_id = id;
        }

        state.cursor_x = 0;
        state.cursor_y = state.area_height as i16 - 1;

        lD3!(MA, "move_end: last_line_id: {}", last_line_id);
        state.first_line = last_line_id;

        let pline = state.lines.get(last_line_id, &state.patterns).unwrap();
        let parts = self.line_parts(&pline, state.area_width) as usize;
        self.move_line_under_cursor(&mut state, last_line_id, parts - 1);

        true
    }

    fn move_word(&self, match_type: MatchType, direction: Direction) -> bool {
        let mut state = self.state.borrow_mut();
        let Some((pos, line_ix, line_part)) = self.resolve_cursor_position(&mut state) else {
            return false;
        };
        let pline = &state.plines[line_ix];
        let linelen = pline.chars.len();
        let mut pos = if let Some(pos) = pos {
            pos
        } else {
            let parts = self.line_parts(pline, state.area_width) as usize;
            // set pos to non-indent part of the line
            if line_part == parts - 1 && state.cursor_x >= self.indent_chars as i16 {
                if direction == Direction::Forward {
                    return false;
                }
                linelen - 1
            } else {
                assert!(state.cursor_x < self.indent_chars as i16);
                state.area_width as usize +
                    (line_part - 1) * (state.area_width as usize - self.indent_chars as usize)
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

        let (x, y) = self.cursor_from_pos_ix(&state, pos, line_ix, state.area_width);
        state.cursor_x = x as i16;
        state.cursor_y = y as i16;

        false
    }

    fn exit(&self) -> bool {
        self.state.borrow_mut().exit = true;
        false
    }

    fn scroll_down(&self) -> bool {
        let mut state = self.state.borrow_mut();
        lD4!(MA, "scroll_down: state.line_offset: {} indexes {:?}",
            state.line_offset, state.line_indexes);

        /*
         * don't scroll down if the bottom line is the last line
         */
        let mode = state.display_mode;
        let last_line_index = state.line_indexes.last().unwrap();
        let last_pline = &state.plines[last_line_index.line_ix];
        let last_parts = self.line_parts(last_pline, state.area_width);
        lD5!(MA, "scroll_down: last_line_index: {:?} last_parts: {}", last_line_index, last_parts);
        if last_line_index.line_part == last_parts - 1 && state.lines.next_line(SearchType::Tag,
            last_pline.line_id, &state.patterns, mode, false).is_none()
        {
            return false;
        }

        let Some(index1) = state.line_indexes.get(1) else {
            return false;
        };
        if index1.line_part > 0 && state.line_offset < index1.line_part {
            state.line_offset += 1;
            return true;
        }
        state.line_offset = 0;

        let first_line = state.first_line;
        let Some(next_line_id) = state.lines.next_line(SearchType::Tag, first_line,
            &state.patterns, mode, false) else
        {
            return false;
        };

        state.first_line = next_line_id;
        state.lines.set_current_line(state.first_line);
        return true;
    }

    fn scroll_up(&self) -> bool {
        let mut state = self.state.borrow_mut();
        lD4!(MA, "scroll_up: state.line_offset: {} indexes {:?}",
            state.line_offset, state.line_indexes);

        if state.line_offset > 0 {
            state.line_offset -= 1;
            return true;
        }

        let mode = state.display_mode;
        let first_line = state.first_line;
        let Some(line_id) = state.lines.prev_line(SearchType::Tag, first_line,
            &state.patterns, mode, false) else
        {
            return false;
        };
        state.first_line = line_id;
        state.lines.set_current_line(state.first_line);

        let pline = state.lines.get(line_id, &state.patterns).unwrap();
        let linelen = pline.chars.len() as u16;
        if linelen > state.area_width {
            state.line_offset = (linelen - state.area_width) as usize /
                (state.area_width as usize - self.indent_chars as usize) + 1;
        }
        return true;
    }

    fn start_of_line(&self) -> bool {
        let mut state = self.state.borrow_mut();
        let Some((_, line_ix, _)) = self.resolve_cursor_position(&mut state) else {
            return false;
        };
        let (x, y) = self.cursor_from_pos_ix(&state, 0, line_ix, state.area_width);
        state.cursor_x = x as i16;
        state.cursor_y = y as i16;

        false
    }

    fn end_of_line(&self) -> bool {
        let mut state = self.state.borrow_mut();
        let Some((_, line_ix, _)) = self.resolve_cursor_position(&mut state) else {
            return false;
        };
        let (x, y) = self.cursor_from_pos_ix(&state, state.plines[line_ix].chars.len() - 1,
            line_ix, state.area_width);
        state.cursor_x = x as i16;
        state.cursor_y = y as i16;

        false
    }

    // return : None if cursor is not on a line
    // return : Some(None, index, part) if cursor is on the whitespace part of the line
    // return : Some(Some(position), index, part) if cursor is on the text part of the line
    fn resolve_cursor_position(&self, state: &mut State) -> Option<(Option<usize>, usize, usize)> {
        let Some(index) = state.line_indexes.get(state.cursor_y as usize) else {
            return None;
        };
        lD5!(MA, "cursor_y {} index {:?}", state.cursor_y, index);
        let pos = if index.line_part > 0 {
            if state.cursor_x < self.indent_chars as i16 {
                lD5!(MA, "returns None, line_ix: {} line_part: {}", index.line_ix, index.line_part);
                return Some((None, index.line_ix, index.line_part));
            }
            index.char_index + state.cursor_x as usize - self.indent_chars as usize
        } else {
            index.char_index + state.cursor_x as usize
        };
        if pos >= state.plines[index.line_ix].chars.len() {
            lD5!(MA, "returns None, line_ix: {} line_part: {}", index.line_ix, index.line_part);
            return Some((None, index.line_ix, index.line_part));
        }

        lD5!(MA, "returns Some({}), line_ix: {} line_part: {}", pos, index.line_ix,
            index.line_part);
        Some((Some(pos), index.line_ix, index.line_part))
    }

    fn cursor_from_pos_ix(&self, state: &State, pos: usize, ix: usize, width: u16) -> (u16, u16) {
        // find new y position
        lD5!(MA, "cursor_from_pos_ix: pos: {} ix: {} width: {}", pos, ix, width);
        let mut y = 0;
        for i in 0..ix {
            let mut parts = self.line_parts(&state.plines[i], width);
            if i == 0 {
                parts -= state.line_offset;
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

    fn tag_hide(&self, all: bool, patmode: PatternMode) -> bool {
        let mut state = self.state.borrow_mut();
        let Some((pos, line_ix, line_part)) = self.resolve_cursor_position(&mut state) else {
            return false;
        };

        let line = &state.plines[line_ix];
        let line_id = line.line_id;

        if all {
            if let Some(pos) = pos {
                if let Some(ref matches) = line.chars[pos].matches {
                    if matches.len() != 1 {
                        // don't do anything on more that one match, it might be confusing
                        state.status_message = Some("ambiguous selection".to_string());
                        return false;
                    }
                    let id = matches[0];
                    let mode = state.patterns.get(id).mode;
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
                        state.last_search = None;
                        (PatternMode::Tagging, MarkType::Tag)
                    } else {
                        return false;
                    };

                    lD1!(MA, "mark/hide: set pattern {} tagging to {:?}", id, new_mode);
                    let mode = state.patterns.get(id).mode;
                    state.patterns.with(id, |p| {
                        p.mode = new_mode;
                        p.style.variant = new_variant;
                    });
                    state.update_patterns(mode);
                    state.update_patterns(new_mode);

                    self.move_line_under_cursor(&mut state, line_id, line_part);

                    return true;
                }
            }
        }

        lD3!(MA, "tag: line_ix: {} pos: {:?} id {}", line_ix, pos, line.line_id);
        lD5!(MA, "line_indexes: {:?}", state.line_indexes);
        let line_id = line.line_id;
        match patmode {
            PatternMode::Tagging => state.lines.toggle_tag(line_id),
            PatternMode::Hiding => state.lines.toggle_hide(line_id),
            _ => panic!("unexpected pattern mode {:?}", patmode),
        }

        self.move_line_under_cursor(&mut state, line_id, line_part);

        true
    }

    fn mark(&self, match_type: MatchType) -> bool {
        let mut state = self.state.borrow_mut();
        let Some((pos, line_ix, line_part)) = self.resolve_cursor_position(&mut state) else {
            return false;
        };
        let Some(mut pos) = pos else {
            return false;
        };
        lD1!(MA, "mark: line: {} pos: {} char: {}", line_ix, pos,
            state.plines[line_ix].chars[pos].c);
        if let Some(ref matches) = state.plines[line_ix].chars[pos].matches {
            let matches = matches.clone();
            // only act on last match
            if let Some(&id) = matches.last() {
                // convert search to mark
                if state.patterns.get(id).mode == PatternMode::Search {
                    // give it a new color
                    let match_index = state.mark_style.index;
                    state.mark_style.cycle_forward();
                    state.patterns.with(id, |p| {
                        p.mode = PatternMode::Marking;
                        p.style.variant = MarkType::Mark;
                        p.style.index = match_index;
                    });
                    state.update_patterns(PatternMode::Search);
                    if Some(id) == state.last_search {
                        state.last_search = None;
                    }
                    return true;
                }
                state.remove_pattern(id);
                let line_id = state.plines[line_ix].line_id;
                self.move_line_under_cursor(&mut state, line_id, line_part);

                return true;
            }

            return false;
        }

        let deliminator = match_type.delimiter();

        let line = &state.plines[line_ix];
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
        let style = state.mark_style.get(MarkType::Mark);
        state.mark_style.cycle_forward();
        state.add_pattern(&pattern, match_type, style, PatternMode::Marking);

        true
    }

    fn cycle_color(&self, direction: Direction) -> bool {
        let mut state = self.state.borrow_mut();
        let Some((pos, line_ix, _)) = self.resolve_cursor_position(&mut state) else {
            return false;
        };
        let Some(pos) = pos else {
            return false;
        };
        let line = &state.plines[line_ix];
        lD1!(MA, "mark: line: {} pos: {} char: {}", line_ix, pos, line.chars[pos].c);
        if let Some(ref matches) = line.chars[pos].matches {
            if matches.len() != 1 {
                // we don't know what to do when the char has multiple matches
                return false;
            }
            let id = matches[0];
            state.patterns.with(id, |p| {
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

    fn mark_extend(&self, extend: bool, direction: Direction) -> bool {
        let mut state = self.state.borrow_mut();
        let Some((pos, line_ix, _)) = self.resolve_cursor_position(&mut state) else {
            return false;
        };
        let pline = &state.plines[line_ix];
        let Some(mut pos) = pos else {
            return false;
        };
        lD1!(MA, "line: {} pos: {} char: {}", line_ix, pos, pline.chars[pos].c);
        if let Some(ref matches) = pline.chars[pos].matches {
            if matches.len() != 1 {
                // we don't know what to do when the char has multiple matches
                return false;
            }
            let id = matches[0];
            loop {
                if extend {
                    if pline.chars[pos].matches.is_some() &&
                       pline.chars[pos].matches.as_ref().unwrap().contains(&id)
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
                    state.patterns.with(id, |p| {
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
                    state.patterns.with(id, |p| {
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
            let mode = state.patterns.get(id).mode;
            state.update_patterns(mode);
        } else if extend {
            let c = pline.chars[pos].c;
            let style = state.mark_style.get(MarkType::Mark);
            state.mark_style.cycle_forward();
            state.add_pattern(&c.to_string(), MatchType::Text, style, PatternMode::Marking);
        }

        return true;
    }

    fn offsets(&self) -> bool {
        let mut state = self.state.borrow_mut();
        state.display_offset = !state.display_offset;

        return true;
    }

    fn adjust_to_unfiltered_line(&self, state: &mut State, line_id: LineId) -> Option<LineId> {
        lD2!(MA, "filter: current line {} is filtered", line_id);
        let mut res = state.lines.next_line(SearchType::Tag, line_id, &state.patterns,
            state.display_mode, true);
        if res.is_none() {
            lD2!(MA, "filter: trying next line backwards");
            res = state.lines.prev_line(SearchType::Tag, line_id, &state.patterns,
                state.display_mode, true);
        }
        let Some(id) = res else {
            lD2!(MA, "filter: nothing found, staying in normal mode");
            state.status_message = Some("nothing to display".to_string());
            state.display_mode = DisplayMode::Normal;
            return None;
        };

        Some(id)
    }

    fn move_line_under_cursor(&self, state: &mut State, line_id: LineId, line_part: usize) {
        // we want line_id in display line line_ix. find lines backwards to find a suitable
        // first_line and offset
        let mut first_line = line_id;
        let mut lines_to_go_back = state.cursor_y as isize - line_part as isize;
        lD2!(MA, "move_under_cursor: line_id {} line_part: {} lines_to_go_back: {} first_line {}",
            line_id, line_part, lines_to_go_back, first_line);
        lD2!(MA, "cursor_x {} cursor_y: {}", state.cursor_x, state.cursor_y);
        let mut first_parts = None;
        while lines_to_go_back > 0 {
            let Some(prev_line_id) = state.lines.prev_line(SearchType::Tag, first_line,
                &state.patterns, state.display_mode, false) else
            {
                lD2!(MA, "can't get back any further");
                break;
            };
            let pline = state.lines.get(prev_line_id, &state.patterns).unwrap();
            first_line = prev_line_id;
            let parts = self.line_parts(&pline, state.area_width) as isize;
            lines_to_go_back -= parts;
            lD2!(MA, "line {} has {} parts, lines_to_go_back {}", prev_line_id, parts,
                lines_to_go_back);
            first_parts = Some(parts);
        }
        lD2!(MA, "first_line: {} lines_to_go_back: {} first_parts {:?}", first_line,
            lines_to_go_back, first_parts);
        state.first_line = first_line;
        state.lines.set_current_line(state.first_line);
        if lines_to_go_back > 0 {
            state.cursor_y -= lines_to_go_back as i16;
        }
        state.line_offset = 0;
        if let Some(first_parts) = first_parts {
            if lines_to_go_back < 0 && -lines_to_go_back < first_parts {
                state.line_offset = -lines_to_go_back as usize;
                lD2!(MA, "filter: line_offset: {}", state.line_offset);
            }
        }
    }

    fn display(&self, direction: Direction) -> bool {
        let mut state = self.state.borrow_mut();
        let old_mode = state.display_mode;

        if direction == Direction::Forward {
            match state.display_mode {
                DisplayMode::All => state.display_mode = DisplayMode::Normal,
                DisplayMode::Normal => state.display_mode = DisplayMode::Tagged,
                DisplayMode::Tagged => state.display_mode = DisplayMode::Manual,
                DisplayMode::Manual => return false,
            }
        } else {
            match state.display_mode {
                DisplayMode::Manual => state.display_mode = DisplayMode::Tagged,
                DisplayMode::Tagged => state.display_mode = DisplayMode::Normal,
                DisplayMode::Normal => state.display_mode = DisplayMode::All,
                DisplayMode::All => return false,
            }
        }

        let line_index = &state.line_indexes[state.cursor_y as usize];
        let line_ix = line_index.line_ix;
        let line_part = line_index.line_part;
        let line_id = state.plines[line_ix].line_id;
        let y = state.cursor_y;
        state.before_filter_pos.insert(old_mode as usize, (line_id, line_part, y));

        // move cursor to next unfiltered line
        let (line_id, line_part) = if let Some(&(id, part, y)) =
            state.before_filter_pos.get(&(state.display_mode as usize))
        {
            // cursor hasn't moved sind last mode change, try to restore the old position
            state.cursor_y = y as i16;
            (id, part)
        } else if let Some(new) = self.adjust_to_unfiltered_line(&mut state, line_id) {
            if new != line_id {
                (new, 0)
            } else {
                (line_id, line_part)
            }
        } else {
            // if we can't find a line, stay in previous mode
            state.display_mode = old_mode;
            return true;
        };

        self.move_line_under_cursor(&mut state, line_id, line_part);

        true
    }

    fn search(&self, direction: Direction, match_type: MatchType) -> bool {
        let mut state = self.state.borrow_mut();
        state.in_search_input = true;
        state.current_search = String::new();
        state.search_direction = direction;
        state.search_match_type = match_type;

        false
    }

    // search string is collected, do the actual search
    fn do_search(&self, search: String) {
        let mut state = self.state.borrow_mut();
        lD5!(MA, "do_search: search: {}", search);
        if let Some(id) = state.last_search {
            state.remove_pattern(id);
            state.last_search = None;
        }
        if search.is_empty() {
            return;
        }
        // TODO: check if the pattern is valid
        let style = state.mark_style.get(MarkType::Search);
        let match_type = state.search_match_type;
        let id = state.add_pattern(&search, match_type, style, PatternMode::Search);
        state.last_search = Some(id);
        drop(state);

        self.search_cont(Direction::Forward);
    }

    fn match_has_mode(&self, state: &State, pline: &ProcessedLine, pos: usize, mode: PatternMode)
        -> bool
    {
        if let Some(ref matches) = pline.chars[pos].matches {
            for &id in matches {
                if state.patterns.get(id).mode == mode {
                    return true;
                }
            }
        }

        false
    }

    // return the start position of the search match, if any
    fn get_search_match_forward(&self, state: &State, pline: &ProcessedLine, pos: usize,
        mut skip_current: bool) -> Option<usize>
    {
        // XXX could be improved in case we have back-to-back matches. Use re matches instead
        // or store match number besides pattern
        let mut pos = pos;
        while pos < pline.chars.len() {
            if self.match_has_mode(state, pline, pos, PatternMode::Search) {
                if !skip_current {
                    return Some(pos);
                }
            } else {
                skip_current = false;
            }
            pos += 1;
        }
        None
    }

    // return the start position of the search match, if any
    fn get_search_match_backward(&self, state: &State, pline: &ProcessedLine, pos: usize,
        mut skip_current: bool) -> Option<usize>
    {
        let mut pos = pos as isize;
        while pos >= 0 {
            if self.match_has_mode(state, pline, pos as usize, PatternMode::Search) {
                if !skip_current {
                    // found a match, now find the start of the match
                    while pos > 0 && self.match_has_mode(state, pline, pos as usize - 1,
                        PatternMode::Search)
                    {
                        pos -= 1;
                    }
                    return Some(pos as usize);
                }
            } else {
                skip_current = false;
            }
            pos -= 1;
        }
        None
    }

    fn search_cont(&self, direction: Direction) -> bool {
        let search_dir = self.state.borrow().search_direction;
        if search_dir == direction {
            self.search_next()
        } else {
            self.search_prev()
        }
    }

    fn search_next(&self) -> bool {
        let mut state = self.state.borrow_mut();

        let (pos, ix, part) = match self.resolve_cursor_position(&mut state) {
            Some(x) => x,
            None => (None, 0, 0),
        };
        let pos = if let Some(pos) = pos {
            pos
        } else if part == 0 {
            0
        } else {
            state.area_width as usize +
                (part - 1) * (state.area_width as usize - self.indent_chars as usize)
        };
        let line_id = state.plines[ix].line_id;
        // don't take line from cache, as the matches aren't up-to-date here
        let pline = state.lines.get(line_id, &state.patterns).unwrap();
        lD2!(MA, "search_next: pos: {} ix: {} part: {} line: {}", pos, ix, part, pline.line_id);
        if let Some(match_pos) = self.get_search_match_forward(&state, &pline, pos, true) {
            lD2!(MA, "do_search: found match at {}", match_pos);
            let (x, y) = self.cursor_from_pos_ix(&state, match_pos, ix, state.area_width);
            state.cursor_x = x as i16;
            state.cursor_y = y as i16;
            return true;
        }
        let mut res = state.lines.next_line(SearchType::Search, pline.line_id, &state.patterns,
            DisplayMode::Normal, false);
        lD2!(MA, "do_search: next_line: {:?}", res);
        if res.is_none() {
            state.lines.set_current_line(0);  // hint for FileSearch
            res = state.lines.next_line(SearchType::Search, 0, &state.patterns,
                DisplayMode::Normal, true);
            lD2!(MA, "do_search: next_line from 0: {:?}", res);
            if res.is_some() {
                state.status_message = Some("Search wrapped".to_string());
            }
        }
        let Some(line_id) = res else {
            lD2!(MA, "do_search: nothing found");
            state.status_message = Some("No matches".to_string());
            return false;
        };

        let pline = state.lines.get(line_id, &state.patterns).unwrap();
        lD10!(MA, "current line: {} {:?}", line_id, pline);
        let match_pos = self.get_search_match_forward(&state, &pline, 0, false).unwrap();

        // if line is on screen, do not scroll
        let ix = state.line_indexes.iter().position(|x| state.plines[x.line_ix].line_id == line_id);
        if let Some(ix) = ix {
            let (x, y) = self.cursor_from_pos_len(match_pos, state.area_width);
            let y = y + ix as u16;
            if y < state.area_height {
                state.cursor_x = x as i16;
                state.cursor_y = y as i16;
                return true;
            }
        }

        lD2!(MA, "do_search: found match at {}", match_pos);
        let (x, y) = self.cursor_from_pos_len(match_pos, state.area_width);
        state.cursor_x = x as i16;
        state.cursor_y = y as i16;

        state.first_line = line_id;
        state.lines.set_current_line(state.first_line);
        state.line_offset = 0;

        true
    }

    fn search_prev(&self) -> bool {
        let mut state = self.state.borrow_mut();

        let (pos, ix, part) = match self.resolve_cursor_position(&mut state) {
            Some(x) => x,
            None => (None, 0, 0),
        };
        let pos = if let Some(pos) = pos {
            pos
        } else if part == 0 {
            0
        } else {
            state.area_width as usize +
                (part - 1) * (state.area_width as usize - self.indent_chars as usize)
        };
        let line_id = state.plines[ix].line_id;
        // don't take line from cache, as the matches aren't up-to-date here
        let pline = state.lines.get(line_id, &state.patterns).unwrap();
        lD2!(MA, "search_prev: pos: {} ix: {} part: {} line: {}", pos, ix, part, pline.line_id);
        if let Some(match_pos) = self.get_search_match_backward(&state, &pline, pos, true) {
            lD2!(MA, "do_search: found match at {}", match_pos);
            let (x, y) = self.cursor_from_pos_ix(&state, match_pos, ix, state.area_width);
            state.cursor_x = x as i16;
            state.cursor_y = y as i16;
            return true;
        }
        let mut res = state.lines.prev_line(SearchType::Search, pline.line_id, &state.patterns,
            DisplayMode::Normal, false);
        lD2!(MA, "do_search: next_line: {:?}", res);
        if res.is_none() {
            let last_line_id = state.lines.last_line_id();
            state.lines.set_current_line(last_line_id); // hint for FileSearch
            res = state.lines.prev_line(SearchType::Search, last_line_id, &state.patterns,
                DisplayMode::Normal, true);
            lD2!(MA, "do_search: next_line from 0: {:?}", res);
        }
        let Some(line_id) = res else {
            lD2!(MA, "do_search: nothing found");
            state.status_message = Some("No matches".to_string());
            return false;
        };

        let pline = state.lines.get(line_id, &state.patterns).unwrap();
        lD10!(MA, "current line: {} {:?}", line_id, pline);
        let len = pline.chars.len();
        let match_pos = self.get_search_match_backward(&state, &pline, len - 1, false).unwrap();

        // if line is on screen, do not scroll
        let ix = state.line_indexes.iter().position(|x| state.plines[x.line_ix].line_id == line_id);
        if let Some(ix) = ix {
            let (x, y) = self.cursor_from_pos_len(match_pos, state.area_width);
            let y = y + ix as u16;
            if y < state.area_height {
                state.cursor_x = x as i16;
                state.cursor_y = y as i16;
                return true;
            }
        }

        lD2!(MA, "do_search: found match at {}", match_pos);
        let (x, y) = self.cursor_from_pos_len(match_pos, state.area_width);
        state.cursor_x = x as i16;
        state.cursor_y = y as i16;

        state.first_line = line_id;
        state.lines.set_current_line(state.first_line);
        state.line_offset = 0;

        true
    }

    fn help(&self) -> bool {
        let mut state = self.state.borrow_mut();
        state.display_help = !state.display_help;
        true
    }
}

impl Widget for &App {
    fn render(self, area: Rect, buf: &mut Buffer) {
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

        /*
         * Handle key events part 1
         */
        let mut recalc_lines = false;
        //idea: don't keep first line, but reference line, together with a position on screen
        if self.state.borrow().in_search_input {
            recalc_lines |= self.handle_search_event_before_layout();
        } else {
            recalc_lines |= self.handle_event_before_layout();
        }

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

        let line_id_len = self.state.borrow().lines.last_line_id().to_string().len();
        let marker_len = if self.state.borrow().display_offset {
            2 + line_id_len as usize + 1
        } else {
            2
        };
        let [marker_area, log_area] =
            Layout::horizontal([Constraint::Length(marker_len as u16), Constraint::Fill(1)])
                .spacing(0)
                .areas(main_area);

        /*
         * calculate cursor position on area change
         */
        let mut state = self.state.borrow_mut();
        if log_area.width != state.area_width || log_area.height != state.area_height {
            lD3!(MA, "render: area change: {}x{} -> {}x{}",
                state.area_width, state.area_height, log_area.width, log_area.height);
            if let Some((pos, ix, part)) = self.resolve_cursor_position(&mut state) {
                if let Some(pos) = pos {
                    // when on text, keep the cursor on the same character
                    let (x, y) = self.cursor_from_pos_ix(&state, pos, ix, log_area.width);
                    state.cursor_x = x as i16;
                    state.cursor_y = y as i16;
                } else if state.cursor_x < self.indent_chars as i16 {
                    // when in indent whitespace, keep it in the same column and part
                    let parts = self.line_parts(&state.plines[ix], log_area.width);
                    let (_, y) = self.cursor_from_pos_ix(&state, 0, ix, log_area.width);
                    state.cursor_y = (y as usize + part.min(parts)) as i16;
                    state.cursor_x = state.cursor_x.min(log_area.width as i16 - 1);
                } else {
                    // in whitespace at end of line
                    // calculate offset after last position
                    let len = state.plines[ix].chars.len();
                    let (x_end, _) = self.cursor_from_pos_ix(&state, len - 1, ix, state.area_width);
                    let off = state.cursor_x - x_end as i16;
                    let (x, y) = self.cursor_from_pos_ix(&state, len - 1, ix, log_area.width);
                    state.cursor_x = (x as i16 + off).min(log_area.width as i16 - 1);
                    state.cursor_y = y as i16;
                }
            } else {
                // XXX leave unchanged?
            }
            state.area_width = log_area.width;
            state.area_height = log_area.height;
        }
        drop(state);

        /*
         * Handle key events part 2
         */
        if self.state.borrow().in_search_input {
            recalc_lines |= self.handle_search_event_after_layout();
        } else {
            recalc_lines |= self.handle_event_after_layout();
        }

        /*
         * build lines
         */
        lD5!(MA, "render: recalc_lines: {}", recalc_lines);
        recalc_lines |= self.state.borrow().plines.is_empty();
        while recalc_lines {
            let mut state = self.state.borrow_mut();
            recalc_lines = false;
            let mut state_lines = Vec::new();
            let skip = state.line_offset;
            let mut curr_line_id = state.first_line;
            let mut num_lines = 0;
            loop {
                lD5!(MA, "render: curr_line_id: {} num_lines {} skip {}",
                    curr_line_id, num_lines, skip);
                let mode = state.display_mode;
                let pline = state.lines.get(curr_line_id, &state.patterns).unwrap();
                let next_line_id = state.lines.next_line(SearchType::Tag, curr_line_id,
                    &state.patterns, mode, false);
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
            state.plines = state_lines;
            drop(state);

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

        /*
         * render lines and build index array
         */
        let mut lines = Vec::new();
        let mut line_indexes = Vec::new();
        let mut state = self.state.borrow_mut();
        let mut skip = state.line_offset;
        'a: for (i, pline) in state.plines.iter().enumerate() {
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
        state.line_indexes = line_indexes;

        /*
         * adjust cursor position if we don't have enough lines
         */
        if state.cursor_y >= state.line_indexes.len() as i16 {
            state.cursor_y = state.cursor_y.min(state.line_indexes.len() as i16 - 1);
            lD5!(MA, "adjusting cursor_y to {}", state.cursor_y);
        }

        lD3!(MA, "render: patterns: {:?}", state.patterns);
        /*
         * render marker area
         */
        let mut markers = Vec::new();
        for index in &state.line_indexes {
            let line = &state.plines[index.line_ix];
            let mut spans = Vec::new();
            if index.line_part == 0 && state.lines.is_hidden(line.line_id) {
                spans.push(Span::raw("H "));
            } else if index.line_part == 0 &&
                line.matches.iter().any(|&id| state.patterns.is_hiding(id))
            {
                spans.push(Span::raw("- "));
            } else if index.line_part == 0 && state.lines.is_tagged(line.line_id) {
                spans.push(Span::raw("T "));
            } else if index.line_part == 0 &&
                line.matches.iter().any(|&id| state.patterns.is_tagging(id))
            {
                spans.push(Span::raw("* "));
            } else {
                spans.push(Span::raw("  "));
            };
            if state.display_offset && index.line_part == 0 {
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
        if state.in_search_input {
            if state.search_match_type == MatchType::Regex {
                spans.push(Span::raw("&"));
            } else if state.search_direction == Direction::Forward {
                spans.push(Span::raw("/"));
            } else {
                spans.push(Span::raw("?"));
            }
            spans.push(Span::raw(state.current_search.clone()));
        } else if let Some(ref message) = state.status_message {
            spans.push(Span::raw(message.clone()).blue().bold());
        } else {
            spans.push(Span::raw("^H").red().bold());
            spans.push(Span::raw(" Help "));
        }   
        let input = Line::from(spans);
        state.status_message = None;

        /*
         * render status area
         */
        // current cursor position
        let cursor_pos = format!("{:3}:{:2} ", state.cursor_x, state.cursor_y);

        // current position
        let line_id = state.plines[state.line_indexes[state.cursor_y as usize].line_ix].line_id;
        let position = (line_id as f64) / (state.lines.last_line_id() + 1) as f64 * 100.0;
        let position = format!("{:3.2}%", position);
        // display mode
        let display_mode = match state.display_mode {
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

        if state.in_search_input {
            *self.render_cursor.borrow_mut() =
                (input_area.x + state.current_search.len() as u16 + 1, input_area.y);
        } else {
            *self.render_cursor.borrow_mut() =
                (log_area.x + state.cursor_x as u16, log_area.y + state.cursor_y as u16);
        }

        if state.display_help && main_area.height > 4 {
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
                .style(Style::default().fg(Color::Black).bg(Color::LightGreen));
            let block_inner = block.inner(help_area);
            block.render(help_area, buf);
            let lines = self.help.help.iter().map(|x| x.clone()).collect::<Vec<_>>();
            Paragraph::new(lines)
                .render(block_inner, buf);
        }
    }
}

#[derive(Debug)]
struct Help {
    help: Vec<Line<'static>>,
    lines: usize,
    columns: usize,
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

           Various
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
        Line::from(vec![]),
        Line::from(vec![Span::styled("Various", heading)]).alignment(Alignment::Center),
        Line::from(vec![
            Span::styled("q", key),
            Span::styled(": quit", text)]),
        Line::from(vec![
            Span::styled("^H", key),
            Span::styled(": toggle display of this help", text)]),
    ];
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
    }
}

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Logging configuration, can be given multiple times
    #[arg(short='l', long)]
    log: Vec<String>,

    #[arg(trailing_var_arg = true, allow_hyphen_values = false, hide = true)]
    files: Vec<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.files.len() != 1 {
        return Err(anyhow::anyhow!("Expected exactly one file"));
    }

    CLog::init_modules("uss", log::LOG_KEYS, Level::Info, FacadeVariant::StdErr,
        Some(Options::default() - FUNC + TID)).unwrap();

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
    let app_result = App {
        state: RefCell::new(State {
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
            in_search_input: false,
            before_filter_pos: HashMap::new(),
            current_search: String::new(),
            search_direction: Direction::Forward,
            search_match_type: MatchType::Text,
            display_help: false,
            last_search: None,
            status_message: None,
            plines: Vec::new(),
            line_indexes: Vec::new(),
        }),
        render_cursor: RefCell::new((0, 0)),
        event: None,
        indent_chars: indent.chars().count() as u16,
        indent,
        help: build_help(),
    }.run(&mut terminal);
    // move to sane position in case the terminal does not have an altscreen
    let size = terminal.size()?;
    let _ = terminal.set_cursor_position((0, size.height - 1));
    println!("");
    ratatui::restore();
    app_result
}
