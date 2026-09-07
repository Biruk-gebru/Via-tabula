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
