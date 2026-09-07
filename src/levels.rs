use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::compaction::{merge_sstables, CompactionError};
use crate::sstable::reader::SsTableReader;

const L0_COMPACTION_THRESHOLD: usize = 4;

pub struct LevelManager {
    pub levels: Vec<Vec<PathBuf>>,
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

    // Merges every L0 file, plus whatever L1 already has, into one new L1 file. L0
    // files are listed first so they win over L1 on any overlapping key, matching
    // merge_sstables' "lowest source_index wins" rule for the most recently flushed
    // data. This is a simplified single-file L1 (no key-range partitioning or the
    // 2MB-per-file splitting a full leveled design would use); it only promotes L0
    // into L1, not cascading further into L2+.
    pub fn compact(&mut self) -> Result<(), CompactionError> {
        while self.levels.len() < 2 {
            self.levels.push(Vec::new());
        }

        let mut input_paths: Vec<PathBuf> = Vec::new();
        input_paths.extend(self.levels[0].drain(..));
        input_paths.extend(self.levels[1].drain(..));

        let readers: Vec<SsTableReader> = input_paths
            .iter()
            .map(|p| SsTableReader::open(p))
            .collect::<Result<Vec<_>, _>>()?;

        let unique_suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let output_path = input_paths[0].with_file_name(format!("l1_{unique_suffix}.sst"));

        merge_sstables(readers, &output_path)?;

        for path in &input_paths {
            fs::remove_file(path).ok();
        }

        self.levels[1].push(output_path);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memtable::MemTable;
    use crate::sstable::reader::SsTableReader;
    use std::env::temp_dir;

    fn build_l0_file(name: &str, key: &str, value: &str) -> PathBuf {
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
            manager.add_l0_file(build_l0_file(&format!("threshold_{i}"), "k", "v"));
        }
        assert!(!manager.needs_compaction(), "3 files should not trigger yet");

        manager.add_l0_file(build_l0_file("threshold_3", "k", "v"));
        assert!(manager.needs_compaction(), "4 files should trigger");

        for path in &manager.levels[0] {
            std::fs::remove_file(path).ok();
        }
    }

    #[test]
    fn compact_merges_l0_into_l1_and_clears_l0() {
        let mut manager = LevelManager::new();
        manager.add_l0_file(build_l0_file("compact_0", "apple", "newest"));
        manager.add_l0_file(build_l0_file("compact_1", "apple", "stale"));
        manager.add_l0_file(build_l0_file("compact_2", "banana", "banana-val"));
        manager.add_l0_file(build_l0_file("compact_3", "cherry", "cherry-val"));

        manager.compact().unwrap();

        assert!(manager.levels[0].is_empty(), "L0 must be empty after compaction");
        assert_eq!(manager.levels[1].len(), 1, "L1 must hold exactly one merged file");

        let l1_path = &manager.levels[1][0];
        assert!(l1_path.exists());

        let mut reader = SsTableReader::open(l1_path).unwrap();
        assert_eq!(reader.get(b"apple").unwrap(), Some(b"newest".to_vec()));
        assert_eq!(reader.get(b"banana").unwrap(), Some(b"banana-val".to_vec()));
        assert_eq!(reader.get(b"cherry").unwrap(), Some(b"cherry-val".to_vec()));

        std::fs::remove_file(l1_path).ok();
    }
}
