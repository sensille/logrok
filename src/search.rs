use std::fs::File;
use std::ffi::OsString;
use std::ffi::OsStr;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::Arc;
use std::io::Read;
use std::io::Seek;
use std::io::BufReader;
use anyhow::Result;
use regex::bytes::RegexSet;
use bitvec::prelude::*;
use clog::prelude::*;

use crate::log::LogKeys::SE;

use crate::lines::LineId;

const SPLIT_CHUNK_SIZE: LineId = 1048576;

pub type SplitId = usize;

// XXX it could be generalized to hold an arbitrary number of searches instead of just
// tag and search specifically
#[derive(Debug)]
pub struct FileSearchReState {
    split_has_matches: BitVec<usize, Lsb0>,
    split_dirty: BitVec<usize, Lsb0>,
    re_seq: u64,
    re: RegexSet,
}

#[derive(Debug)]
pub struct FileSearchInner {
    filename: OsString,
    thread_handles: Vec<std::thread::JoinHandle<()>>,
    split_ids: Vec<LineId>, // ends of splits
    max_split_len: LineId,
    // shared state
    re_states: Vec<FileSearchReState>,
    split_in_progress: BitVec<usize, Lsb0>,
    current_split: usize, // index of the split that contains the current line
}

#[derive(Debug, Clone)]
pub struct FileSearch {
    // first condvar: re has changed
    // second condvar: split has been processed
    inner: Arc<(Mutex<FileSearchInner>, Condvar, Condvar)>,
}

impl FileSearch {
    pub fn new(filename: &OsStr, num_res: usize) -> Result<Self> {
        // TODO: split in background, multi-threaded
        let split_ids = split_file(&filename, SPLIT_CHUNK_SIZE)?;
        let nsplits = split_ids.len();
        let mut start = 0;
        let mut max_split_len = 0;
        for &end in &split_ids {
            max_split_len = max_split_len.max(end - start);
            start = end;
        }
        lD3!(SE, "max_split_len: {}, nsplits: {}", max_split_len, nsplits);
        let mut re_states = Vec::new();
        for _ in 0..num_res {
            re_states.push(FileSearchReState {
                split_has_matches: bitvec![0; nsplits],
                split_dirty: bitvec![0; nsplits],
                re_seq: 0,
                re: RegexSet::new(&[""; 0]).unwrap(), // never
            });
        }
        let this = FileSearch {
            inner: Arc::new(
                (Mutex::new(FileSearchInner {
                    filename: filename.into(),
                    thread_handles: Vec::new(),
                    split_ids,
                    max_split_len,
                    re_states,
                    split_in_progress: bitvec![0; nsplits],
                    current_split: 0,
                }),
                Condvar::new(),
                Condvar::new()),
            ),
        };

        // start threads
        let num_cpus = num_cpus::get();
        let mut th = Vec::new();
        for _ in 0..num_cpus {
            let s = this.clone();
            th.push(std::thread::spawn(move || {
                s.search_thread();
            }));
        }

        let mut inner = this.inner.0.lock().unwrap();
        inner.thread_handles = th;
        drop(inner);

        Ok(this)
    }

    pub fn set_re(&mut self, ix: usize, re: &RegexSet) {
        let mut inner = self.inner.0.lock().unwrap();
        assert!(ix < inner.re_states.len());
        lD3!(SE, "set_re: ix {} to {:?}", ix, re);
        inner.re_states[ix].split_dirty = bitvec![1; inner.split_ids.len()];
        inner.re_states[ix].re_seq += 1;
        inner.re_states[ix].re = re.clone();
        self.inner.1.notify_all();
    }

    pub fn set_current_split(&mut self, split_id: SplitId) {
        let mut inner = self.inner.0.lock().unwrap();
        inner.current_split = split_id;
    }

    pub fn split_has_matches(&self, ix: usize, split_id: SplitId) -> bool {
        let mut inner = self.inner.0.lock().unwrap();
        assert!(ix < inner.re_states.len());

        while inner.re_states[ix].split_dirty[split_id] {
            inner = self.inner.2.wait(inner).unwrap();
        }

        inner.re_states[ix].split_has_matches[split_id]
    }

    fn search_next(inner: &FileSearchInner, ix: usize) -> Option<SplitId> {
        let mut search_up = Some(inner.current_split);
        let mut search_down = Some(inner.current_split);
        let mut found = None;
        while search_up.is_some() || search_down.is_some() {
            if let Some(id) = search_up {
                let next = inner.re_states[ix].split_dirty[id..].first_one();
                if let Some(next) = next {
                    if inner.split_in_progress[id + next] {
                        search_up = if id + next < inner.split_ids.len() - 1 {
                            Some(id + next + 1)
                        } else {
                            None
                        };
                    } else {
                        found = Some(id + next);
                        break;
                    }
                } else {
                    search_up = None;
                }
            }
            if let Some(id) = search_down {
                let next = inner.re_states[ix].split_dirty[..id].last_one();
                if let Some(next) = next {
                    if inner.split_in_progress[next] {
                        search_down = if next > 0 {
                            Some(next - 1)
                        } else {
                            None
                        };
                    } else {
                        found = Some(next);
                        break;
                    }
                } else {
                    search_down = None;
                }
            }
        }

        found
    }

    fn search_thread(&self) {
        let mut inner = self.inner.0.lock().unwrap();
        let mut file = File::open(&inner.filename).unwrap();
        let mut buf = vec![0; inner.max_split_len as usize];

        loop {
            let mut found = None;
            let mut ix = 0;
            for i in 0..inner.re_states.len() {
                if let Some(f) = Self::search_next(&inner, i) {
                    found = Some(f);
                    ix = i;
                    break;
                }
            }
            let Some(split_id) = found else {
                lD10!(SE, "no more dirty splits");
                inner = self.inner.1.wait(inner).unwrap();
                continue;
            };
            lD10!(SE, "found dirty split: {} ix {}", split_id, ix);

            inner.split_in_progress.set(split_id, true);
            let re = inner.re_states[ix].re.clone();
            let seq = inner.re_states[ix].re_seq;

            let start = if split_id > 0 {
                inner.split_ids[split_id - 1]
            } else {
                0
            };
            let end = inner.split_ids[split_id];
            drop(inner);

            // search split for all patterns, is_match
            file.seek(std::io::SeekFrom::Start(start)).unwrap();
            file.read_exact(&mut buf[..(end - start) as usize]).unwrap();
            let m = re.is_match(&buf[..(end - start) as usize]);

            // update split state with matches
            inner = self.inner.0.lock().unwrap();
            inner.split_in_progress.set(split_id, false);
            // discard result if pattern has changed
            if inner.re_states[ix].re_seq == seq {
                inner.re_states[ix].split_dirty.set(split_id, false);
                inner.re_states[ix].split_has_matches.set(split_id, m);
                if m {
                    lD5!(SE, "match in split split_id: {} ix {}", split_id, ix);
                } else {
                    lD10!(SE, "no match in split split_id: {} ix {}", split_id, ix);
                }
                self.inner.2.notify_all();
            }
        }
    }

    pub fn num_splits(&self) -> usize {
        let inner = self.inner.0.lock().unwrap();

        inner.split_ids.len()
    }

    pub fn find_split(&self, line_id: LineId) -> Option<SplitId> {
        let inner = self.inner.0.lock().unwrap();
        if line_id < inner.split_ids[0] {
            return Some(0);
        }
        let id = match inner.split_ids.binary_search(&line_id) {
            Ok(i) => i + 1,
            Err(i) => i,
        };
        if id == inner.split_ids.len() {
            lD5!(SE, "find_split: line_id {} >= last {}", line_id, inner.split_ids.last().unwrap());
            return None;
        }
        let start = if id > 0 {
            inner.split_ids[id - 1]
        } else {
            0
        };
        let end = inner.split_ids[id];
        lD5!(SE, "find_split: line_id {} split_id {:?} start {} end {}", line_id, id,
            start, end);
        assert!(line_id >= start && line_id < end);

        Some(id)
    }

    pub fn get_split(&self, split: SplitId) -> Option<(LineId, LineId)> {
        let inner = self.inner.0.lock().unwrap();
        if split >= inner.split_ids.len() {
            return None;
        }
        if split > 0 {
            return Some((inner.split_ids[split - 1], inner.split_ids[split]));
        } else {
            return Some((0, inner.split_ids[0]));
        }
    }
}

fn split_file(name: &OsStr, chunk_size: u64) -> std::io::Result<Vec<LineId>> {
    let mut splits = Vec::new();
    let mut file = std::fs::File::open(name)?;
    let mut buf = vec![0; 1];
    let mut start = chunk_size;
    'a: loop {
        file.seek(std::io::SeekFrom::Start(start))?;
        let mut reader = BufReader::new(file);
        loop {
            let bytes_read = reader.read(&mut buf)?;
            if bytes_read == 0 {
                file = reader.into_inner();
                break 'a;
            }
            start += 1;
            if buf[0] == b'\n' {
                splits.push(start);
                start += chunk_size;
                break;
            }
        }
        file = reader.into_inner();
    }
    splits.push(file.metadata().unwrap().len());

    Ok(splits)
}
