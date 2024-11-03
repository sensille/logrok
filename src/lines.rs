use std::collections::BTreeSet;
use anyhow::Result;
use std::num::NonZeroUsize;
use std::ffi::OsStr;
use bitvec::prelude::*;
use clog::prelude::*;
use std::sync::Arc;

use crate::log::LogKeys::LI;
use crate::cache::*;
use crate::pattern::*;
use crate::search::*;

pub type LineId = u64;

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum DisplayMode {
    All,
    Normal,
    Tagged,
    Manual,     // only manually tagged lines
}

#[derive(Debug, Clone)]
pub struct ProcessedLine {
    pub line_id: LineId,
    pub chars: Vec<StyledChar>,
    pub matches: Vec<PatternId>,
}

#[derive(Debug)]
pub struct Lines {
    tagged_lines: BTreeSet<LineId>,
    hidden_lines: BTreeSet<LineId>,
    all_hidden_splits: BitVec<usize, Lsb0>,
    split_cache: SplitCache,
    _hidden_seq: usize,
}

impl Lines {
    pub fn new(filename: &OsStr) -> Result<Self> {
        let split_cache = SplitCache::new(filename, NonZeroUsize::new(50).unwrap())?;
        Ok(Self {
            tagged_lines: BTreeSet::new(),
            hidden_lines: BTreeSet::new(),
            all_hidden_splits: bitvec![0; split_cache.num_splits()],
            split_cache,
            _hidden_seq: 0,
        })
    }

    pub fn toggle_tag(&mut self, line_id: LineId) {
        if self.tagged_lines.contains(&line_id) {
            self.tagged_lines.remove(&line_id);
        } else {
            self.tagged_lines.insert(line_id);
        }
    }

    pub fn is_tagged(&self, line_id: LineId) -> bool {
        self.tagged_lines.contains(&line_id)
    }

    pub fn is_hidden(&self, line_id: LineId) -> bool {
        self.hidden_lines.contains(&line_id)
    }

    pub fn toggle_hide(&mut self, line_id: LineId) {
        if self.hidden_lines.contains(&line_id) {
            self.hidden_lines.remove(&line_id);
        } else {
            self.hidden_lines.insert(line_id);
        }
    }

    // points somewhere into a line, resolves that line to a split and a line ix
    fn resolve_line_id(&self, line_id: LineId, patterns: &PatternSet)
        -> Option<(SplitId, LineId, Arc<Split>, usize)>
    {
        let split_id = self.split_cache.find_split(line_id)?;
        let (split_start, _) = self.split_cache.get_split(split_id)?;
        let split = self.split_cache.get(split_id, patterns).ok()?;

        let rel_id = (line_id - split_start) as usize;
        let line_ix = match split.line_ends.binary_search(&rel_id) {
            Ok(i) => i + 1,
            Err(i) => i,
        };

        Some((split_id, split_start, split, line_ix))
    }

    pub fn get(&self, line_id: LineId, patterns: &PatternSet, crop_chars: Option<usize>)
        -> Option<ProcessedLine>
    {
        let (_, split_start, split, line_ix) = self.resolve_line_id(line_id, patterns)?;

        let (rel_start, rel_end) = if line_ix == 0 {
            (0, split.line_ends[0])
        } else {
            (split.line_ends[line_ix - 1], split.line_ends[line_ix])
        };

        // XXX handle/convert non-utf8 lines
        let line = String::from_utf8(split.buf[rel_start..rel_end].to_vec()).unwrap();
        let (pline, matches) = patterns.process_line(&line, crop_chars);

        return Some(ProcessedLine {
            line_id: split_start + rel_start as LineId,
            chars: pline,
            matches,
        });
    }

    #[allow(dead_code)]
    pub fn is_filtered_line(&self, line_id: LineId, mode: DisplayMode, patterns: &PatternSet)
        -> Option<bool>
    {
        lD5!(LI, "is_filtered_line {} {:?}", line_id, mode);
        let (_, split_start, split, line_ix) = self.resolve_line_id(line_id, patterns)?;

        Some(self.is_filtered(SearchType::Tag, line_ix, &split, split_start, mode))
    }

    fn is_filtered(&self, st: SearchType, line_ix: usize, split: &Split, split_start: LineId,
        mode: DisplayMode) -> bool
    {
        lD5!(LI, "is_filtered {} {} {:?} st {:?}", line_ix, split_start, mode, st);
        // if the line is part of a search result, it's always displayed
        if split.search_lines.contains(&line_ix) {
            lD5!(LI, "search line");
            return false;
        }
        if st == SearchType::Search {
            return true;
        }
        let line_id = if line_ix == 0 {
            0
        } else {
            split.line_ends[line_ix - 1]
        } as LineId + split_start;
        lD5!(LI, "line_id {}", line_id);
        match mode {
            DisplayMode::Normal =>
                split.hidden_lines.contains(&line_ix) || self.hidden_lines.contains(&line_id),
            DisplayMode::Tagged =>
                !(split.tagged_lines.contains(&line_ix) || self.tagged_lines.contains(&line_id)),
            DisplayMode::Manual =>
                !self.tagged_lines.contains(&line_id),
            DisplayMode::All => false,
        }
    }

    fn skip_split(&self, st: SearchType, split_id: SplitId, split_start: LineId, split_end: LineId,
        mode: DisplayMode) -> bool
    {
        // if the split is part of a search result, it's always displayed
        if self.split_cache.has_matches(SearchType::Search, split_id) {
            lD5!(LI, "don't skip split {} search", split_id);
            return false;
        }
        if st == SearchType::Search {
            return true;
        }
        match mode {
            DisplayMode::Normal => {
                if self.all_hidden_splits[split_id] {
                    return true;
                }
            }
            DisplayMode::Tagged => {
                if self.tagged_lines.range(split_start..split_end).next().is_some() {
                    lD3!(LI, "don't skip split {} tagged", split_id);
                    return false;
                }
                if !self.split_cache.has_matches(SearchType::Tag, split_id) {
                    return true;
                }
            }
            DisplayMode::Manual => {
                if self.tagged_lines.range(split_start..split_end).next().is_some() {
                    lD3!(LI, "don't skip split {} tagged", split_id);
                    return false;
                }
                return true;
            }
            DisplayMode::All => {
                return false;
            }
        }
        false
    }

    // line_id points somewhere into the current line. Returns the id of the next unfiltered line
    // if inclusive is true, the current line is included in the search
    pub fn next_line(&self, st: SearchType, line_id: LineId, patterns: &PatternSet,
        mode: DisplayMode, inclusive: bool) -> Option<LineId>
    {
        lD3!(LI, "next line for {} mode {:?}", line_id, mode);

        /*
        if patterns.hidden_seq != self.hidden_seq {
            self.all_hidden_splits = bitvec![0; self.split_cache.file_search.num_splits()];
        }
        */

        let (mut split_id, _, split, mut line_ix) = self.resolve_line_id(line_id, patterns)?;
        let num_splits = self.split_cache.num_splits();
        lD5!(LI, "current_line is split_id {} ({}) line_ix {}", split_id, num_splits, line_ix);

        // advance to the next line
        if !inclusive {
            line_ix += 1;
            if line_ix == split.line_ends.len() {
                split_id += 1;
                line_ix = 0;
            }
        }
        // get split id
        'a: while split_id < num_splits {
            let (split_start, split_end) = self.split_cache.get_split(split_id)?;
            if self.skip_split(st, split_id, split_start, split_end, mode) {
                lD6!(LI, "skipping split {}", split_id);
                split_id += 1;
                line_ix = 0;
                continue;
            }
            let split = self.split_cache.get(split_id, patterns).ok()?;
            lD4!(LI, "split tagged lines {:?} search_lines {:?} hidden lines {:?}",
                split.tagged_lines, split.search_lines, split.hidden_lines);

            loop {
                lD6!(LI, "loop2: split_id {} line_ix {}", split_id, line_ix);
                if !self.is_filtered(st, line_ix, &split, split_start, mode) {
                    lD5!(LI, "found {}", line_ix);
                    break;
                }

                if line_ix == split.line_ends.len() - 1 {
                    split_id += 1;
                    line_ix = 0;
                    continue 'a;
                }

                line_ix += 1;
            }
            lD5!(LI, "after loops: split_start {} line_ix {}", split_start, line_ix);

            if line_ix == 0 {
                return Some(split_start);
            } else {
                return Some(split_start + split.line_ends[line_ix - 1] as LineId);
            }
        }

        None
    }

    // line_id points somewhere into the current line. Returns the id of the previous unfiltered line
    // if inclusive is true, the current line is included in the search
    pub fn prev_line(&self, st: SearchType, line_id: LineId, patterns: &PatternSet,
        mode: DisplayMode, inclusive: bool) -> Option<LineId>
    {
        lD3!(LI, "prev line for {} mode {:?}", line_id, mode);

        /*
        if patterns.hidden_seq != self.hidden_seq {
            self.all_hidden_splits = bitvec![0; self.split_cache.file_search.num_splits()];
        }
        */

        let (mut split_id, _, _, mut line_ix) = self.resolve_line_id(line_id, patterns)?;
        let num_splits = self.split_cache.num_splits();
        lD5!(LI, "current_line is split_id {} ({}) line_ix {}", split_id, num_splits, line_ix);

        // back to the prev line
        if !inclusive {
            if line_ix == 0 {
                if split_id == 0 {
                    return None;
                }
                split_id -= 1;
                line_ix = usize::MAX;
            } else {
                line_ix -= 1;
            }
        }
        // get split id
        'a: loop {
            let (split_start, split_end) = self.split_cache.get_split(split_id)?;
            if self.skip_split(st, split_id, split_start, split_end, mode) {
                if split_id == 0 {
                    return None;
                }
                split_id -= 1;
                continue;
            }
            let split = self.split_cache.get(split_id, patterns).ok()?;
            lD8!(LI, "split tagged lines {:?} hidden lines {:?}",
                split.tagged_lines, split.hidden_lines);
            if line_ix == usize::MAX {
                line_ix = split.line_ends.len() - 2;
            }

            loop {
                lD5!(LI, "loop2: split_id {} line_ix {}", split_id, line_ix);
                if !self.is_filtered(st, line_ix, &split, split_start, mode) {
                    lD5!(LI, "found {}", line_ix);
                    break;
                }

                if line_ix == 0 {
                    if split_id == 0 {
                        return None;
                    }
                    split_id -= 1;
                    line_ix = usize::MAX;
                    continue 'a;
                }

                line_ix -= 1;
            }
            lD5!(LI, "after loops: split_start {} line_ix {}", split_start, line_ix);

            if line_ix == 0 {
                return Some(split_start);
            } else {
                return Some(split_start + split.line_ends[line_ix - 1] as LineId);
            }
        }
    }

    pub fn update_patterns(&self, st: SearchType, patterns: &PatternSet) {
        self.split_cache.set_re(st, patterns);
    }

    pub fn last_line_id(&self) -> LineId {
        let num_splits = self.split_cache.num_splits();
        let (_, split_end) = self.split_cache.get_split(num_splits - 1).unwrap();

        split_end - 1
    }

    pub fn set_current_line(&self, line_id: LineId) {
        let split_id = self.split_cache.find_split(line_id).unwrap();
        self.split_cache.set_current_split(split_id);
    }

    pub fn get_file_search(&self) -> FileSearch {
        self.split_cache.get_file_search()
    }
}
