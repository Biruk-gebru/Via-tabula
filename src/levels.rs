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

