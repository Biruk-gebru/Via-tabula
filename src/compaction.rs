// L1+ files need non-overlapping ranges to avoid read amplification: without that
// guarantee, a simple get could turn into reading every file in that level, since any
// of them could hold the key. Because the ranges never overlap, we can check each
// file's min and max key first, ruling most files out immediately, then go straight to
// the one appropriate file and read only that single file.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::path::Path;

use crate::sstable::reader::SsTableReader;
use crate::sstable::writer::{SsTableError, SsTableWriter};
use crate::types::is_tombstone;

#[derive(Debug)]
pub enum CompactionError {
    Io,
    EmptySsTable,
}

impl From<SsTableError> for CompactionError {
    fn from(_: SsTableError) -> Self {
        CompactionError::Io
    }
}

// One "current head" candidate pulled from one reader's stream. BinaryHeap is a max-heap,
// so Ord below is reversed on key alone, making the smallest key pop first.
struct HeapEntry {
    key: Vec<u8>,
    value: Vec<u8>,
    source_index: usize,
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for HeapEntry {}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        other.key.cmp(&self.key)
    }
}

pub fn merge_sstables(
    mut readers: Vec<SsTableReader>,
    output_path: &Path,
) -> Result<(), CompactionError> {
    // BloomFilter::new needs an upfront key count, but we won't know the true
    // deduplicated count until the merge itself runs. Sum each input's raw record count
    // as a safe upper bound: the real count is <= this, so the filter ends up slightly
    // bigger than strictly necessary, never smaller.
    let mut expected_keys = 0usize;
    for reader in readers.iter_mut() {
        expected_keys += reader.iter()?.count();
    }

    let mut writer = SsTableWriter::new(output_path, expected_keys)?;

    // one live iterator per input reader; these stay open for the whole merge below
    let mut iters = readers
        .iter_mut()
        .map(|r| r.iter())
        .collect::<Result<Vec<_>, _>>()?;

    let mut heap: BinaryHeap<HeapEntry> = BinaryHeap::new();
    for (source_index, it) in iters.iter_mut().enumerate() {
        if let Some((key, value)) = it.next() {
            heap.push(HeapEntry {
                key,
                value,
                source_index,
            });
        }
    }

    while let Some(first) = heap.pop() {
        let current_key = first.key;
        let mut best_value = first.value;
        let mut best_source = first.source_index;
        let mut consumed_sources = vec![first.source_index];

        // drain every other heap entry that shares this key; keep only the value from
        // the lowest source_index (the most recently flushed input)
        while let Some(next) = heap.peek() {
            if next.key != current_key {
                break;
            }
            let next = heap.pop().unwrap();
            consumed_sources.push(next.source_index);
            if next.source_index < best_source {
                best_value = next.value;
                best_source = next.source_index;
            }
        }

        // every source whose head we just consumed needs its next item pulled in
        for source in consumed_sources {
            if let Some((next_key, next_value)) = iters[source].next() {
                heap.push(HeapEntry {
                    key: next_key,
                    value: next_value,
                    source_index: source,
                });
            }
        }

        if !is_tombstone(&best_value) {
            writer.add(&current_key, &best_value)?;
        }
    }

    writer.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memtable::MemTable;
    use std::env::temp_dir;

    // builds one L0 SSTable file from a list of (key, is_delete, value) ops, in the
    // given order, and returns its path
    fn build_sstable(name: &str, ops: &[(&str, Option<&str>)]) -> std::path::PathBuf {
        let path = temp_dir().join(format!("tabula_compaction_test_{name}.sst"));
        let mut memtable = MemTable::new();
        for (key, value) in ops {
            match value {
                Some(v) => memtable.set(key.as_bytes().to_vec(), v.as_bytes().to_vec()),
                None => memtable.delete(key.as_bytes().to_vec()),
            }
        }
        memtable.flush(&path).unwrap();
        path
    }

    #[test]
    fn merge_sstables_dedupes_tombstones_and_keeps_newest() {
        // 6 L0 files, index 0 = most recently flushed. Some keys appear in multiple
        // files to exercise the "keep newest, skip tombstones" logic.
        let paths = vec![
            build_sstable(
                "0_newest",
                &[("apple", Some("new-apple")), ("banana", None)],
            ),
            build_sstable(
                "1",
                &[("apple", Some("old-apple")), ("cherry", Some("cherry-val"))],
            ),
            build_sstable(
                "2",
                &[("banana", Some("old-banana")), ("date", Some("date-val"))],
            ),
            build_sstable("3", &[("elderberry", Some("elderberry-val"))]),
            build_sstable("4", &[("fig", Some("fig-val"))]),
            build_sstable(
                "5_oldest",
                &[("grape", Some("grape-val")), ("cherry", Some("old-cherry"))],
            ),
        ];

        let readers: Vec<SsTableReader> = paths
            .iter()
            .map(|p| SsTableReader::open(p).unwrap())
            .collect();

        let output_path = temp_dir().join("tabula_compaction_test_output.sst");
        merge_sstables(readers, &output_path).unwrap();

        let mut merged = SsTableReader::open(&output_path).unwrap();

        // apple: newest value from file 0 wins over file 1's stale copy
        assert_eq!(merged.get(b"apple").unwrap(), Some(b"new-apple".to_vec()));
        // banana: file 0's tombstone is the newest record, so it's gone entirely
        assert_eq!(merged.get(b"banana").unwrap(), None);
        // cherry: file 1 (index 1) beats file 5 (index 5) as the more recent copy
        assert_eq!(merged.get(b"cherry").unwrap(), Some(b"cherry-val".to_vec()));
        // keys that only ever appeared once pass through unchanged
        assert_eq!(merged.get(b"date").unwrap(), Some(b"date-val".to_vec()));
        assert_eq!(
            merged.get(b"elderberry").unwrap(),
            Some(b"elderberry-val".to_vec())
        );
        assert_eq!(merged.get(b"fig").unwrap(), Some(b"fig-val".to_vec()));
        assert_eq!(merged.get(b"grape").unwrap(), Some(b"grape-val".to_vec()));

        // each surviving key appears exactly once in the merged output (6 unique
        // survivors: apple, cherry, date, elderberry, fig, grape; banana dropped)
        let all: Vec<(Vec<u8>, Vec<u8>)> = merged.iter().unwrap().collect();
        assert_eq!(
            all.len(),
            6,
            "expected exactly 6 surviving keys, got {all:?}"
        );

        let mut seen_keys: Vec<&Vec<u8>> = all.iter().map(|(k, _)| k).collect();
        let unique_count = {
            seen_keys.sort();
            seen_keys.dedup();
            seen_keys.len()
        };
        assert_eq!(unique_count, 6, "every surviving key must be unique");

        for path in &paths {
            std::fs::remove_file(path).ok();
        }
        std::fs::remove_file(&output_path).ok();
    }
}
