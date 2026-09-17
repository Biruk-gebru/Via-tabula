// SSTable file layout (immutable, written sequentially, never modified)
//
// ── data section ──────────────────────────────────────────────────────
//  [ key_len: 4B ][ key: key_len B ][ val_len: 4B ][ val: val_len B ]
//  [ key_len: 4B ][ key: key_len B ][ val_len: 4B ][ val: val_len B ]
//  ...  (one record per key-value pair, written in sorted key order)
//
// ── bloom filter section ─────────────────────────────────────────────
//  [ k: 8B ][ m: 8B ][ bits: m.div_ceil(8) B ]
//  (see BloomFilter::to_bytes; lets a reader skip a disk seek for keys
//   that were never inserted)
//
// ── index block ───────────────────────────────────────────────────────
//  [ key_len: 4B ][ key: key_len B ][ offset: 8B ]
//  ...  (one entry per ~4 KB of data section; offset = byte position
//        in this file where that key's record starts)
//
// ── footer (always the last 32 bytes of the file) ─────────────────────
//  [ filter_offset: 8B ][ filter_len: 8B ][ index_offset: 8B ][ index_len: 8B ]
//
// To read: seek to file_size - 32, read footer → tells you where the
// bloom filter and index block start. Load the filter, check it first;
// load the index, binary-search for your key, seek to that offset in
// the data section, scan forward to find it.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::bloom::BloomFilter;

#[derive(Debug)]
pub enum SsTableError {
    Io,
}

pub struct SsTableWriter {
    writer: BufWriter<File>,
    index: Vec<(Vec<u8>, u64)>,
    bytes_since_last_index: usize,
    offset: u64,
    filter: BloomFilter,
    // Reused across add() calls (cleared, not reallocated) so building each record's
    // bytes doesn't allocate a fresh buffer every time. See add()'s comment for why
    // it exists at all: M9's flamegraph showed this was worth doing.
    record_buf: Vec<u8>,
    // The name the caller actually asked for. We write to tmp_path instead and only
    // rename to this at the very end of finish(), so a crash mid-write never leaves a
    // half-formed file sitting under the name a reader would expect a real one at.
    final_path: PathBuf,
    tmp_path: PathBuf,
}

// BufWriter's default (8 KB) is smaller than a typical flush's whole data section, so
// the internal buffer already fills and triggers a real write() syscall multiple times
// per flush regardless of how many userspace write_all calls we make - that syscall
// count, not userspace copy count, is what wall-clock time is actually bottlenecked on
// (see the M9 flamegraph discussion). 64 KB comfortably covers this benchmark's whole
// flush (~30 KB), aiming to collapse most flushes down to a single real syscall.
const WRITER_BUFFER_CAPACITY: usize = 64 * 1024;

impl SsTableWriter {
    pub fn new(path: &Path, expected_keys: usize) -> Result<Self, SsTableError> {
        let fp_rate: f64 = 0.01;
        let tmp_path = path.with_extension("tmp");
        let file = File::create(&tmp_path).map_err(|_| SsTableError::Io)?;
        Ok(SsTableWriter {
            writer: BufWriter::with_capacity(WRITER_BUFFER_CAPACITY, file),
            index: Vec::new(),
            bytes_since_last_index: 0,
            offset: 0,
            filter: BloomFilter::new(expected_keys, fp_rate),
            record_buf: Vec::new(),
            final_path: path.to_path_buf(),
            tmp_path,
        })
    }

    pub fn add(&mut self, key: &[u8], value: &[u8]) -> Result<(), SsTableError> {
        if self.index.is_empty() || self.bytes_since_last_index >= 4096 {
            self.index.push((key.to_vec(), self.offset));
            self.bytes_since_last_index = 0;
        }

        let key_len = key.len() as u32;
        let val_len = value.len() as u32;

        // M9 flamegraph (flamegraph_flush_isolated.svg): the top two hotspots inside
        // flush were add() itself (40.6%) and a memmove (15.6%) from four separate
        // write_all calls each copying into BufWriter's internal buffer on their own.
        // Building the whole record in one buffer first and writing it in a single
        // call cuts both: one copy into BufWriter instead of four, and no per-call
        // heap allocation once record_buf's capacity settles (clear() keeps it).
        self.record_buf.clear();
        self.record_buf.extend_from_slice(&key_len.to_le_bytes());
        self.record_buf.extend_from_slice(key);
        self.record_buf.extend_from_slice(&val_len.to_le_bytes());
        self.record_buf.extend_from_slice(value);

        self.writer
            .write_all(&self.record_buf)
            .map_err(|_| SsTableError::Io)?;

        let record_size = self.record_buf.len();
        self.offset += record_size as u64;
        self.bytes_since_last_index += record_size;
        self.filter.insert(key);

        Ok(())
    }

    pub fn finish(mut self) -> Result<(), SsTableError> {
        // bloom filter section: written right after the data section ends
        let filter_offset = self.offset;
        let filter_bytes = self.filter.to_bytes();
        self.writer
            .write_all(&filter_bytes)
            .map_err(|_| SsTableError::Io)?;
        let filter_len = filter_bytes.len() as u64;

        // index block: starts wherever the filter section ended
        let index_offset = filter_offset + filter_len;

        for (key, offset) in &self.index {
            let key_len = key.len() as u32;
            self.writer
                .write_all(&key_len.to_le_bytes())
                .map_err(|_| SsTableError::Io)?;
            self.writer.write_all(key).map_err(|_| SsTableError::Io)?;
            self.writer
                .write_all(&offset.to_le_bytes())
                .map_err(|_| SsTableError::Io)?;
        }

        let index_len = self
            .index
            .iter()
            .map(|(k, _)| 4 + k.len() + 8)
            .sum::<usize>() as u64;

        // footer: 4 fixed 8B fields, always the last 32 bytes of the file
        self.writer
            .write_all(&filter_offset.to_le_bytes())
            .map_err(|_| SsTableError::Io)?;
        self.writer
            .write_all(&filter_len.to_le_bytes())
            .map_err(|_| SsTableError::Io)?;
        self.writer
            .write_all(&index_offset.to_le_bytes())
            .map_err(|_| SsTableError::Io)?;
        self.writer
            .write_all(&index_len.to_le_bytes())
            .map_err(|_| SsTableError::Io)?;

        self.writer.flush().map_err(|_| SsTableError::Io)?;

        // Atomic on the same filesystem: final_path either doesn't exist yet at all,
        // or exists fully-formed, footer included - never observable half-written,
        // even across a crash right here.
        std::fs::rename(&self.tmp_path, &self.final_path).map_err(|_| SsTableError::Io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env::temp_dir;

    #[test]
    fn final_path_does_not_exist_until_finish_succeeds() {
        let path = temp_dir().join("tabula_writer_test_atomic.sst");
        std::fs::remove_file(&path).ok();

        let mut writer = SsTableWriter::new(&path, 10).unwrap();
        writer.add(b"key", b"value").unwrap();

        // mid-write: the real name must not exist yet, only the tmp one
        assert!(
            !path.exists(),
            "final path must not exist before finish() completes"
        );
        let tmp_path = path.with_extension("tmp");
        assert!(tmp_path.exists(), "writes must land in the tmp file");

        writer.finish().unwrap();

        // after finish(): the real name exists, and the tmp name is gone (renamed,
        // not copied)
        assert!(path.exists(), "final path must exist once finish() succeeds");
        assert!(
            !tmp_path.exists(),
            "tmp path must not survive a successful finish()"
        );

        std::fs::remove_file(&path).ok();
    }
}
