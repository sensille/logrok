use std::ffi::OsString;
use serde::{Serialize, Deserialize};
use anyhow::Result;
use std::fs::File;
use std::fs;
use md5::Context;
use std::os::unix::fs::FileExt;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QHash {
    pub filesize: u64,
    pub hash: [u8; 16],
}

pub fn check(name: &OsString, qhash: &QHash) -> bool {
    let Ok(metadata) = fs::metadata(name) else {
        return false;
    };
    let filesize = metadata.len();

    // smaller files always count as changed
    if filesize < qhash.filesize {
        return false;
    }

    let Ok(hash) = generate_with_len(name, qhash.filesize) else {
        return false;
    };

    if hash != qhash.hash {
        return false;
    }
    true
}

pub fn generate(name: &OsString, old_qhash: &Option<QHash>) -> Result<QHash> {
    let filesize = fs::metadata(name)?.len();

    // if filesize did not change, return the old qhash (if provided)
    if let Some(old_qhash) = old_qhash {
        if filesize == old_qhash.filesize {
            return Ok(old_qhash.clone());
        }
    }

    let hash = generate_with_len(name, filesize)?;
    Ok(QHash {
        filesize,
        hash,
    })
}

fn make_intervals(filesize: u64) -> Vec<(u64, usize)> {
    let nparts = 20;
    let psize = 500;
    let mut intervals = Vec::new();
    if filesize <= nparts * psize {
        let parts = (filesize + psize - 1) / psize;
        let partsize = (filesize + parts - 1) / parts;
        for i in 0..parts {
            let offset = i * partsize;
            let len = partsize.min(filesize - offset) as usize;
            intervals.push((offset as u64, len));
        }
        return intervals;
    }
    let parts = ((filesize + 499) / 500).min(19) as usize;
    let stride = (filesize - 499) as f64 / (parts - 1) as f64;
    for i in 0..parts {
        let offset = (i as f64 * stride) as u64;
        let len = 500.min((filesize - offset) as usize);
        intervals.push((offset, len));
    }
    intervals
}

fn generate_with_len(name: &OsString, filesize: u64) -> Result<[u8; 16]> {
    // check file in up to 20 places of 500 bytes each. This is not a strict check, but
    // should be sufficient in practice.
    // Distribute the checks in a way that the beginning and the end are fully covered.
    let file = File::open(name)?;
    let mut hasher = Context::new();
    let mut buffer = vec![0; 500];
    for (start, len) in make_intervals(filesize) {
        let bytes_read = file.read_at(&mut buffer[0..len], start as u64)?;
        assert_eq!(bytes_read, len);
        hasher.consume(&buffer[..bytes_read]);
    }

    Ok(hasher.finalize().0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_qhash_intervals() -> Result<()> {
        for filesize in 1..200000 {
            let intervals = make_intervals(filesize);
            println!("File size: {}, Intervals: {:?}", filesize, intervals);
            assert_eq!(intervals[0].0, 0);
            let last = intervals.last().unwrap();
            assert_eq!(last.0 + last.1 as u64, filesize);
            for i in 0..intervals.len() - 1 {
                let (start, len) = intervals[i];
                let (next_start, _) = intervals[i + 1];
                assert!(start + len as u64 <= next_start);
            }
        }
        Ok(())
    }
}
