use anyhow::Result;
use lru::LruCache;
use std::ffi::OsStr;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::cell::RefCell;
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::io::Read;

use crate::log::LogKeys::CA;
use crate::search::SplitId;
use crate::search::FileSearch;
use crate::pattern::*;
use crate::lines::LineId;

// when changed, also change SearchType::max
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SearchType {
    Tag = 0,
    Search = 1,
}

impl SearchType {
    pub fn as_ix(&self) -> usize {
        *self as usize
    }
    fn max() -> usize {
        2
    }
}

#[derive(Debug)]
pub struct Split {
    pub pattern_seq: PatternId,
    pub buf: Vec<u8>,
    pub line_ends: Vec<usize>,
    pub tagged_lines: Vec<usize>,
    pub search_lines: Vec<usize>,
    pub hidden_lines: Vec<usize>,
}

#[derive(Debug)]
pub struct SplitCacheInner {
    lru: LruCache<SplitId, Arc<Split>>,
    file_search: FileSearch,
    file: File,
}

#[derive(Debug)]
pub struct SplitCache {
    inner: RefCell<SplitCacheInner>
}

impl SplitCache {
    pub fn new(filename: &OsStr, nsplits: NonZeroUsize) -> Result<Self> {
        let file = File::open(filename)?;
        Ok(SplitCache { inner: RefCell::new(SplitCacheInner {
            lru: LruCache::new(nsplits),
            file_search: FileSearch::new(filename, SearchType::max())?,
            file,
        })})
    }

    pub fn num_splits(&self) -> usize {
        let inner = self.inner.borrow();
        inner.file_search.num_splits()
    }

    pub fn find_split(&self, line_id: LineId) -> Option<SplitId> {
        let inner = self.inner.borrow();
        inner.file_search.find_split(line_id)
    }

    pub fn get_split(&self, split: SplitId) -> Option<(LineId, LineId)> {
        let inner = self.inner.borrow();
        inner.file_search.get_split(split)
    }

    pub fn get_file_search(&self) -> FileSearch {
        let inner = self.inner.borrow();
        inner.file_search.clone()
    }

    pub fn set_re(&self, st: SearchType, patterns: &PatternSet) {
        let mut inner = self.inner.borrow_mut();
        match st {
            SearchType::Tag => {
                inner.file_search.set_re(st.as_ix(), &patterns.get_tagged_re());
            }
            SearchType::Search => {
                inner.file_search.set_re(st.as_ix(), &patterns.get_search_re());
            }
        }
    }

    pub fn has_matches(&self, st: SearchType, split_id: SplitId) -> bool {
        let inner = self.inner.borrow();
        inner.file_search.split_has_matches(st.as_ix(), split_id)
    }

    pub fn set_current_split(&self, split_id: SplitId) {
        let mut inner = self.inner.borrow_mut();
        inner.file_search.set_current_split(split_id);
    }

    pub fn get(&self, split_id: SplitId, patterns: &PatternSet) -> Result<Arc<Split>> {
        lD3!(CA, "get split {}", split_id);
        let mut inner = self.inner.borrow_mut();
        let mut split = match inner.lru.pop(&split_id) {
            Some(split) if split.pattern_seq == patterns.seq => {
                lD3!(CA, "cache hit");
                let split = split.clone();
                inner.lru.put(split_id, split.clone());
                return Ok(split.clone());
            }
            Some(split) => {
                lD3!(CA, "cache hit, but wrong pattern_seq");
                Arc::into_inner(split).unwrap()
            }
            None => {
                lD3!(CA, "cache miss");
                let Some((start, end)) = inner.file_search.get_split(split_id) else {
                    panic!("split {} not found", split_id);
                };
                inner.file.seek(SeekFrom::Start(start as u64))?;
                // XXX avoid buffer init by using bytes crate?
                // https://docs.rs/cbuffer/0.3.1/src/cbuffer/lib.rs.html#1-155
                // you can write within capacity and unsafe set_len()
                //< Widdershins> sensille: specifically you may be looking at set_len and
                //   spare_capacity_mut in vec
                // 22:10 < cehteh> ah yes that got stabilized meanwhile :D
                let buflen = (end - start) as usize;
                let mut buf = vec![0; buflen];
                inner.file.read_exact(&mut buf)?;

                let mut line_ends = Vec::new();
                let mut start = 0;
                loop {
                    lD10!(CA, "start {} buflen {}", start, buflen);
                    let Some(line_end) = memchr::memchr(b'\n', &buf[start..]) else {
                        break;
                    };
                    lD10!(CA, "line_end {}", line_end);
                    line_ends.push(start + line_end + 1);
                    start += line_end + 1;
                }
                // last split in a file that does not end with a newline
                if line_ends.last() < Some(&buflen) {
                    line_ends.push(buflen);
                }

                lD5!(CA, "split read done");

                Split {
                    pattern_seq: patterns.seq,
                    buf,
                    line_ends,
                    tagged_lines: Vec::new(),
                    search_lines: Vec::new(),
                    hidden_lines: Vec::new(),
                }
            }
        };

        let tagged_re = patterns.get_tagged_re();
        let search_re = patterns.get_search_re();
        let hidden_re = patterns.get_hidden_re();

        // split buffer into lines and scan each line for patterns
        let mut start = 0;
        let mut tagged_lines = Vec::new();
        let mut search_lines = Vec::new();
        let mut hidden_lines = Vec::new();
        for (i, &end) in split.line_ends.iter().enumerate() {
            if tagged_re.is_match(&split.buf[start..end-1]) {
                tagged_lines.push(i);
            }
            if hidden_re.is_match(&split.buf[start..end-1]) {
                hidden_lines.push(i);
            }
            if search_re.is_match(&split.buf[start..end-1]) {
                search_lines.push(i);
            }
            start = end;
        }

        split.pattern_seq = patterns.seq;
        split.tagged_lines = tagged_lines;
        split.search_lines = search_lines;
        split.hidden_lines = hidden_lines;

        let split = Arc::new(split);

        lD5!(CA, "split scan done");

        inner.lru.put(split_id, split.clone());

        Ok(split)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MarkStyle;
    use crate::MarkType;

    #[test]
    fn test_split_cache() {
        let filename = OsStr::new("/home/arne/logrok/tr");
        let mark_style = MarkStyle::new();
        let mut ps = PatternSet::new(mark_style.clone());

        ps.add("striped-8", MatchType::SmallWord, mark_style.get(MarkType::Mark),
            PatternMode::Tagging);
        ps.add("allocd-12", MatchType::SmallWord, mark_style.get(MarkType::Mark),
            PatternMode::Hiding);
        ps.add("baz", MatchType::SmallWord, mark_style.get(MarkType::Mark),
            PatternMode::Marking);

        let sc = SplitCache::new(filename, NonZeroUsize::new(100).unwrap()).unwrap();
        let split = sc.get(0, &ps).unwrap();
        assert_eq!(split.tagged_lines, vec![0]);
        assert_eq!(split.hidden_lines, vec![1]);
    }
}

