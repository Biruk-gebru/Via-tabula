//Read amplification: the amount of extra bytes we have to read from disk affecting disk IO
//Here read amplification is ~4kb
//Tradeoff is for a lower RA we will have to store more on the indexes in Vec which can be memory consumptive which is not the best scenario the other hand a larger RA mean we take significantly more time to get a single entry which is expensive for a disk read
//Therfore the ideal spot for an RA is somewhere inbetween where the index amount isnt as much but also we dont have to look too much inbetween indexes

use crate::bloom::BloomFilter;
use crate::sstable::writer::SsTableError;
use crate::types::is_tombstone;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub struct SsTableReader {
    file: File,
    // sparse index: (key, byte offset of that key's record in the data section)
    index: Vec<(Vec<u8>, u64)>,
    // byte offset where the data section ends (== filter_offset from the footer)
    data_end: u64,
    filter: BloomFilter,
}

pub struct SsTableIter<'a> {
    file: &'a mut File,
    pos: u64,
    data_end: u64,
}

impl<'a> Iterator for SsTableIter<'a> {
    type Item = (Vec<u8>, Vec<u8>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.data_end {
            return None;
        }

        let mut len_buf = [0u8; 4];
        self.file.read_exact(&mut len_buf).ok()?;
        let key_len = u32::from_le_bytes(len_buf) as usize;

        let mut key = vec![0u8; key_len];
        self.file.read_exact(&mut key).ok()?;

        self.file.read_exact(&mut len_buf).ok()?;
        let val_len = u32::from_le_bytes(len_buf) as usize;

        let mut val = vec![0u8; val_len];
        self.file.read_exact(&mut val).ok()?;

        self.pos += (4 + key_len + 4 + val_len) as u64;

        Some((key, val))
    }
}

impl SsTableReader {
    pub fn open(path: &Path) -> Result<Self, SsTableError> {
        // read the 32B footer to learn where the filter and index sections start and
        // how long each one is
        let mut file = File::open(path).map_err(|_| SsTableError::Io)?;

        file.seek(SeekFrom::End(-32))
            .map_err(|_| SsTableError::Io)?;

        let mut footer = [0u8; 32];
        file.read_exact(&mut footer).map_err(|_| SsTableError::Io)?;

        let filter_offset = u64::from_le_bytes(footer[0..8].try_into().unwrap());
        let filter_len = u64::from_le_bytes(footer[8..16].try_into().unwrap());
        let index_offset = u64::from_le_bytes(footer[16..24].try_into().unwrap());
        let index_len = u64::from_le_bytes(footer[24..32].try_into().unwrap());

        // seek to the bloom filter section and read it in full
        file.seek(SeekFrom::Start(filter_offset))
            .map_err(|_| SsTableError::Io)?;

        let mut filter_bytes = vec![0u8; filter_len as usize];
        file.read_exact(&mut filter_bytes)
            .map_err(|_| SsTableError::Io)?;

        let filter = BloomFilter::from_bytes(&filter_bytes);

        // seek to the index block and read it in full
        file.seek(SeekFrom::Start(index_offset))
            .map_err(|_| SsTableError::Io)?;

        let mut index_bytes = vec![0u8; index_len as usize];
        file.read_exact(&mut index_bytes)
            .map_err(|_| SsTableError::Io)?;

        // parse repeated [key_len: 4B][key][offset: 8B] entries until the block is consumed
        let mut index = Vec::new();
        let mut pos = 0usize;
        while pos < index_bytes.len() {
            let key_len =
                u32::from_le_bytes(index_bytes[pos..pos + 4].try_into().unwrap()) as usize;
            pos += 4;

            let key = index_bytes[pos..pos + key_len].to_vec();
            pos += key_len;

            let offset = u64::from_le_bytes(index_bytes[pos..pos + 8].try_into().unwrap());
            pos += 8;

            index.push((key, offset));
        }

        Ok(SsTableReader {
            file,
            index,
            data_end: filter_offset,
            filter,
        })
    }

    pub fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>, SsTableError> {
        // definitely absent: skip the index search and the disk seek entirely
        if !self.filter.may_contain(key) {
            return Ok(None);
        }

        let search_result = self
            .index
            .binary_search_by(|entry| entry.0.as_slice().cmp(key));

        // Ok(pos): key is itself a checkpoint, we know its offset exactly.
        // Err(0): key sorts before the very first checkpoint (== the file's smallest
        //   key), so it can't be present at all.
        // Err(insertion_point): key would sort between checkpoints; the checkpoint
        //   just before it (insertion_point - 1) is where the forward scan must start.
        let start_offset = match search_result {
            Ok(pos) => self.index[pos].1,
            Err(0) => return Ok(None),
            Err(insertion_point) => self.index[insertion_point - 1].1,
        };

        self.file
            .seek(SeekFrom::Start(start_offset))
            .map_err(|_| SsTableError::Io)?;

        let mut pos = start_offset;
        while pos < self.data_end {
            let mut len_buf = [0u8; 4];
            self.file
                .read_exact(&mut len_buf)
                .map_err(|_| SsTableError::Io)?;
            let key_len = u32::from_le_bytes(len_buf) as usize;

            let mut record_key = vec![0u8; key_len];
            self.file
                .read_exact(&mut record_key)
                .map_err(|_| SsTableError::Io)?;

            self.file
                .read_exact(&mut len_buf)
                .map_err(|_| SsTableError::Io)?;
            let val_len = u32::from_le_bytes(len_buf) as usize;

            let mut record_val = vec![0u8; val_len];
            self.file
                .read_exact(&mut record_val)
                .map_err(|_| SsTableError::Io)?;

            pos += (4 + key_len + 4 + val_len) as u64;

            match record_key.as_slice().cmp(key) {
                std::cmp::Ordering::Equal => {
                    // now that flush persists tombstones (so an older SSTable's value can't
                    // resurrect), get must hide the sentinel from callers itself
                    if is_tombstone(&record_val) {
                        return Ok(None);
                    }
                    return Ok(Some(record_val));
                }
                std::cmp::Ordering::Greater => return Ok(None),
                std::cmp::Ordering::Less => continue,
            }
        }

        Ok(None)
    }

    pub fn iter(&mut self) -> Result<SsTableIter<'_>, SsTableError> {
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|_| SsTableError::Io)?;

        Ok(SsTableIter {
            file: &mut self.file,
            pos: 0,
            data_end: self.data_end,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sstable::writer::SsTableWriter;
    use std::env::temp_dir;

    #[test]
    fn get_finds_every_key_across_many_checkpoints() {
        let path = temp_dir().join("tabula_reader_test_get.sst");
        let mut writer = SsTableWriter::new(&path, 2000).unwrap();
        let mut keys = Vec::new();
        for i in 0..2000u32 {
            let key = format!("key-{:05}", i).into_bytes();
            let val = format!("val-{:05}", i).into_bytes();
            writer.add(&key, &val).unwrap();
            keys.push((key, val));
        }
        writer.finish().unwrap();

        let mut reader = SsTableReader::open(&path).unwrap();
        assert!(
            reader.index.len() > 1,
            "expected multiple index checkpoints"
        );

        for (k, v) in &keys {
            let got = reader.get(k).unwrap();
            assert_eq!(
                got.as_ref(),
                Some(v),
                "missing key {:?}",
                String::from_utf8_lossy(k)
            );
        }

        assert_eq!(reader.get(b"aaa-not-present").unwrap(), None);
        assert_eq!(reader.get(b"zzz-not-present").unwrap(), None);
        assert_eq!(reader.get(b"key-00500x").unwrap(), None);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn get_returns_none_for_keys_deleted_before_flush() {
        use crate::memtable::MemTable;

        let path = temp_dir().join("tabula_reader_test_tombstone.sst");
        let mut memtable = MemTable::new();
        memtable.set(b"alive".to_vec(), b"still here".to_vec());
        memtable.set(b"gone".to_vec(), b"will be deleted".to_vec());
        memtable.delete(b"gone".to_vec());

        memtable.flush(&path).unwrap();

        let mut reader = SsTableReader::open(&path).unwrap();
        assert_eq!(reader.get(b"alive").unwrap(), Some(b"still here".to_vec()));
        assert_eq!(reader.get(b"gone").unwrap(), None);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn iter_still_surfaces_raw_tombstone_records() {
        use crate::memtable::MemTable;

        let path = temp_dir().join("tabula_reader_test_iter_tombstone.sst");
        let mut memtable = MemTable::new();
        memtable.set(b"alive".to_vec(), b"still here".to_vec());
        memtable.set(b"gone".to_vec(), b"will be deleted".to_vec());
        memtable.delete(b"gone".to_vec());

        memtable.flush(&path).unwrap();

        // compaction (M7) needs to see the raw tombstone record via iter to decide
        // whether it's safe to drop, so unlike get, iter must not hide it
        let mut reader = SsTableReader::open(&path).unwrap();
        let collected: Vec<(Vec<u8>, Vec<u8>)> = reader.iter().unwrap().collect();
        assert_eq!(
            collected,
            vec![
                (b"alive".to_vec(), b"still here".to_vec()),
                (b"gone".to_vec(), b"__TOMBSTONE__".to_vec()),
            ]
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn iter_yields_every_key_in_sorted_order() {
        let path = temp_dir().join("tabula_reader_test_iter.sst");
        let mut writer = SsTableWriter::new(&path, 500).unwrap();
        let mut expected = Vec::new();
        for i in 0..500u32 {
            let key = format!("key-{:05}", i).into_bytes();
            let val = format!("val-{:05}", i).into_bytes();
            writer.add(&key, &val).unwrap();
            expected.push((key, val));
        }
        writer.finish().unwrap();

        let mut reader = SsTableReader::open(&path).unwrap();
        let collected: Vec<(Vec<u8>, Vec<u8>)> = reader.iter().unwrap().collect();
        assert_eq!(collected, expected);

        std::fs::remove_file(&path).ok();
    }
}
