use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::path::Path;

use crate::sstable::reader::SsTableReader;
use crate::sstable::writer::{SsTableError, SsTableWriter};
use crate::types::is_tombstone;

#[derive(Debug)]
pub enum CompactionError {
    Io,
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
