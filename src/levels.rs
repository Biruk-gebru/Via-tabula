use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::compaction::{merge_sstables, CompactionError};
use crate::sstable::reader::SsTableReader;

const L0_COMPACTION_THRESHOLD: usize = 4;

pub struct LevelManager {
    pub levels: Vec<Vec<PathBuf>>,
}

// Full-scans a reader to find its smallest and largest key. SSTables don't store this in
// the footer, so the only way to know it is to walk every record once.
fn key_range(reader: &mut SsTableReader) -> Result<(Vec<u8>, Vec<u8>), CompactionError> {
    let mut iter = reader.iter()?;
    let (first_key, _) = iter.next().ok_or(CompactionError::EmptySsTable)?;

    let mut last_key = first_key.clone();
    for (key, _) in iter {
        last_key = key;
    }

    Ok((first_key, last_key))
}

fn ranges_overlap(a: &(Vec<u8>, Vec<u8>), b: &(Vec<u8>, Vec<u8>)) -> bool {
    let (a_min, a_max) = a;
    let (b_min, b_max) = b;
    a_min <= b_max && b_min <= a_max
}

impl LevelManager {
    pub fn new() -> Self {
        LevelManager { levels: Vec::new() }
    }

    pub fn add_l0_file(&mut self, path: PathBuf) {
        if self.levels.is_empty() {
            self.levels.push(Vec::new());
        }
        self.levels[0].push(path);
    }

    pub fn needs_compaction(&self) -> bool {
        self.levels
            .first()
            .map(|l0| l0.len() >= L0_COMPACTION_THRESHOLD)
            .unwrap_or(false)
    }

    // Takes all of L0, but only the L1 files whose key range overlaps L0's combined
    // range; L1 files with no overlap are left untouched, since nothing being merged
    // could possibly change what they hold. This is still simplified versus a full
    // leveled design: one merged output file rather than splitting into ~2MB chunks,
    // and no cascading past L1 into L2+.
    pub fn compact(&mut self) -> Result<(), CompactionError> {
        while self.levels.len() < 2 {
            self.levels.push(Vec::new());
        }

        if self.levels[0].is_empty() {
            return Ok(());
        }

        let l0_paths: Vec<PathBuf> = self.levels[0].drain(..).collect();

        let mut l0_readers: Vec<SsTableReader> = l0_paths
            .iter()
            .map(|p| SsTableReader::open(p))
            .collect::<Result<Vec<_>, _>>()?;

        // combined key range across every L0 file being compacted
        let mut l0_range: Option<(Vec<u8>, Vec<u8>)> = None;
        for reader in l0_readers.iter_mut() {
            let (min_key, max_key) = key_range(reader)?;
            l0_range = Some(match l0_range {
                None => (min_key, max_key),
                Some((current_min, current_max)) => {
                    (current_min.min(min_key), current_max.max(max_key))
                }
            });
        }
        let l0_range = l0_range.expect("l0_paths is non-empty, checked above");

        // split L1 into files that overlap L0's range (must be merged in) and files
        // that don't (left alone, still valid, no reason to touch them)
        let mut overlapping_l1_paths = Vec::new();
        let mut overlapping_l1_readers = Vec::new();
        let mut untouched_l1_paths = Vec::new();
        for path in self.levels[1].drain(..) {
            let mut reader = SsTableReader::open(&path)?;
            let l1_range = key_range(&mut reader)?;
            if ranges_overlap(&l0_range, &l1_range) {
                overlapping_l1_paths.push(path);
                overlapping_l1_readers.push(reader);
            } else {
                untouched_l1_paths.push(path);
            }
        }

        // L0 readers first so they win ties over L1 in merge_sstables (most recently
        // flushed data wins on an overlapping key)
        let mut readers = l0_readers;
        readers.extend(overlapping_l1_readers);

        let unique_suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let output_path = l0_paths[0].with_file_name(format!("l1_{unique_suffix}.sst"));

        merge_sstables(readers, &output_path)?;

        for path in l0_paths.iter().chain(overlapping_l1_paths.iter()) {
            fs::remove_file(path).ok();
        }

        self.levels[1] = untouched_l1_paths;
        self.levels[1].push(output_path);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memtable::MemTable;
    use std::env::temp_dir;

    fn build_sstable(name: &str, key: &str, value: &str) -> PathBuf {
        let path = temp_dir().join(format!("tabula_levels_test_{name}.sst"));
        let mut memtable = MemTable::new();
        memtable.set(key.as_bytes().to_vec(), value.as_bytes().to_vec());
        memtable.flush(&path).unwrap();
        path
    }

    #[test]
    fn needs_compaction_true_once_l0_hits_threshold() {
        let mut manager = LevelManager::new();
        assert!(!manager.needs_compaction());

        for i in 0..3 {
            manager.add_l0_file(build_sstable(&format!("threshold_{i}"), "k", "v"));
        }
        assert!(!manager.needs_compaction(), "3 files should not trigger yet");

        manager.add_l0_file(build_sstable("threshold_3", "k", "v"));
        assert!(manager.needs_compaction(), "4 files should trigger");

        for path in &manager.levels[0] {
            std::fs::remove_file(path).ok();
        }
    }

    #[test]
    fn compact_merges_l0_into_l1_and_clears_l0() {
        let mut manager = LevelManager::new();
        manager.add_l0_file(build_sstable("compact_0", "apple", "newest"));
        manager.add_l0_file(build_sstable("compact_1", "apple", "stale"));
        manager.add_l0_file(build_sstable("compact_2", "banana", "banana-val"));
        manager.add_l0_file(build_sstable("compact_3", "cherry", "cherry-val"));

        let old_l0_paths: Vec<PathBuf> = manager.levels[0].clone();

        manager.compact().unwrap();

        for path in &old_l0_paths {
            assert!(
                !path.exists(),
                "old L0 file {path:?} must be deleted after compaction"
            );
        }

        assert!(
            manager.levels[0].is_empty(),
            "L0 must be empty after compaction"
        );
        assert_eq!(
            manager.levels[1].len(),
            1,
            "L1 must hold exactly one merged file when it started empty"
        );

        let l1_path = &manager.levels[1][0];
        let mut reader = SsTableReader::open(l1_path).unwrap();
        assert_eq!(reader.get(b"apple").unwrap(), Some(b"newest".to_vec()));
        assert_eq!(
            reader.get(b"banana").unwrap(),
            Some(b"banana-val".to_vec())
        );
        assert_eq!(
            reader.get(b"cherry").unwrap(),
            Some(b"cherry-val".to_vec())
        );

        std::fs::remove_file(l1_path).ok();
    }

    #[test]
    fn compact_leaves_non_overlapping_l1_file_untouched() {
        let mut manager = LevelManager::new();

        // pre-existing L1 file whose key range ("zebra") is nowhere near L0's
        // ("apple".."cherry") below; nothing being merged could affect it
        let untouched_path = build_sstable("l1_untouched", "zebra", "old-zebra");
        manager.levels = vec![vec![], vec![untouched_path.clone()]];

        manager.add_l0_file(build_sstable("overlap_0", "apple", "apple-val"));
        manager.add_l0_file(build_sstable("overlap_1", "banana", "banana-val"));
        manager.add_l0_file(build_sstable("overlap_2", "cherry", "cherry-val"));
        manager.add_l0_file(build_sstable("overlap_3", "date", "date-val"));

        manager.compact().unwrap();

        assert_eq!(
            manager.levels[1].len(),
            2,
            "the untouched L1 file plus one new merged file"
        );
        assert!(
            manager.levels[1].contains(&untouched_path),
            "non-overlapping L1 file must survive compaction unchanged"
        );

        // the untouched file's own content must be unaffected
        let mut untouched_reader = SsTableReader::open(&untouched_path).unwrap();
        assert_eq!(
            untouched_reader.get(b"zebra").unwrap(),
            Some(b"old-zebra".to_vec())
        );

        // the new merged file holds everything from the overlapping L0 batch
        let merged_path = manager.levels[1]
            .iter()
            .find(|p| **p != untouched_path)
            .unwrap();
        let mut merged_reader = SsTableReader::open(merged_path).unwrap();
        assert_eq!(
            merged_reader.get(b"apple").unwrap(),
            Some(b"apple-val".to_vec())
        );
        assert_eq!(
            merged_reader.get(b"date").unwrap(),
            Some(b"date-val".to_vec())
        );

        for path in &manager.levels[1] {
            std::fs::remove_file(path).ok();
        }
    }
}
