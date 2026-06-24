// SSTable file layout (immutable, written sequentially, never modified)
//
// ── data section ──────────────────────────────────────────────────────
//  [ key_len: 4B ][ key: key_len B ][ val_len: 4B ][ val: val_len B ]
//  [ key_len: 4B ][ key: key_len B ][ val_len: 4B ][ val: val_len B ]
//  ...  (one record per key-value pair, written in sorted key order)
//
// ── index block ───────────────────────────────────────────────────────
//  [ key_len: 4B ][ key: key_len B ][ offset: 8B ]
//  ...  (one entry per ~4 KB of data section; offset = byte position
//        in this file where that key's record starts)
//
// ── footer (always the last 16 bytes of the file) ─────────────────────
//  [ index_offset: 8B ][ index_len: 8B ]
//
// To read: seek to file_size - 16, read footer → tells you where the
// index block starts. Load the index, binary-search for your key,
// seek to that offset in the data section, scan forward to find it.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

#[derive(Debug)]
pub enum SsTableError {
    Io,
}

pub struct SsTableWriter {
    writer: BufWriter<File>,
    index: Vec<(Vec<u8>, u64)>,
    bytes_since_last_index: usize,
    offset: u64,
}

impl SsTableWriter {
    pub fn new(path: &Path) -> Result<Self, SsTableError> {
        let file = File::create(path).map_err(|_| SsTableError::Io)?;
        Ok(SsTableWriter {
            writer: BufWriter::new(file),
            index: Vec::new(),
            bytes_since_last_index: 0,
            offset: 0,
        })
    }

    pub fn add(&mut self, key: &[u8], value: &[u8]) -> Result<(), SsTableError> {
        if self.index.is_empty() || self.bytes_since_last_index >= 4096 {
            self.index.push((key.to_vec(), self.offset));
            self.bytes_since_last_index = 0;
        }

        let key_len = key.len() as u32;
        let val_len = value.len() as u32;

        self.writer.write_all(&key_len.to_le_bytes()).map_err(|_| SsTableError::Io)?;
        self.writer.write_all(key).map_err(|_| SsTableError::Io)?;
        self.writer.write_all(&val_len.to_le_bytes()).map_err(|_| SsTableError::Io)?;
        self.writer.write_all(value).map_err(|_| SsTableError::Io)?;

        let record_size = 4 + key.len() + 4 + value.len();
        self.offset += record_size as u64;
        self.bytes_since_last_index += record_size;

        Ok(())
    }

    pub fn finish(mut self) -> Result<(), SsTableError> {
        let index_offset = self.offset;

        for (key, offset) in &self.index {
            let key_len = key.len() as u32;
            self.writer.write_all(&key_len.to_le_bytes()).map_err(|_| SsTableError::Io)?;
            self.writer.write_all(key).map_err(|_| SsTableError::Io)?;
            self.writer.write_all(&offset.to_le_bytes()).map_err(|_| SsTableError::Io)?;
        }

        let index_len = self.index.iter()
            .map(|(k, _)| 4 + k.len() + 8)
            .sum::<usize>() as u64;

        self.writer.write_all(&index_offset.to_le_bytes()).map_err(|_| SsTableError::Io)?;
        self.writer.write_all(&index_len.to_le_bytes()).map_err(|_| SsTableError::Io)?;

        self.writer.flush().map_err(|_| SsTableError::Io)
    }
}
