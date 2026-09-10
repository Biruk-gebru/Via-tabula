// Trick question: what's the locking mechanism for the MemTable write and read?
// Answer: both use RwLock. Multiple readers can hold the read lock at the same time
// (unlike Mutex, which serializes even reader-vs-reader), which means better DB access
// time under read-heavy load. Same for the LevelManager.
//
// So on compaction, the ideal picture is: read from the files while the merge is
// happening, and only delete them after readers are done. In practice that's close to
// what happens on disk already: draining the tracked paths out of LevelManager is
// instant, in-memory bookkeeping, but the actual files aren't deleted until after
// merge_sstables has already finished successfully, so they stay readable throughout
// the merge. What actually governs concurrent access is the RwLock on LevelManager, not
// file deletion timing. Rust's RwLock doesn't let us choose to interrupt an active
// reader though; there's no preemption in the safe API at all. If compaction needs the
// write lock while a reader already holds the read lock, the write-lock acquisition
// simply blocks until that reader releases it. The real choice available is whether to
// block and wait (write()) or use try_write() to skip this round and retry later
// without stalling the compaction thread.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::levels::LevelManager;
use crate::memtable::MemTable;
use crate::sstable::reader::SsTableReader;
use crate::sstable::writer::SsTableError;
use crate::types::is_tombstone;
use crate::wal::{Wal, WalEntry, WalError};

const DEFAULT_FLUSH_THRESHOLD: usize = 4 * 1024 * 1024;

#[derive(Debug)]
pub enum LsmError {
    Io,
}

impl From<WalError> for LsmError {
    fn from(_: WalError) -> Self {
        LsmError::Io
    }
}

impl From<SsTableError> for LsmError {
    fn from(_: SsTableError) -> Self {
        LsmError::Io
    }
}

pub struct Lsm {
    dir: PathBuf,
    mem: RwLock<MemTable>,
    wal: Mutex<Wal>,
    // Arc, not a bare RwLock: the background compaction thread needs to keep touching
    // this after open() returns and the caller moves/owns the returned Lsm, so this
    // lock has to be independently, jointly owned rather than borrowed from self.
    levels: Arc<RwLock<LevelManager>>,
    flush_threshold: usize,
    shutdown: Arc<AtomicBool>,
    compaction_thread: Option<JoinHandle<()>>,
}

const COMPACTION_POLL_INTERVAL: Duration = Duration::from_millis(200);

impl Drop for Lsm {
    // Signals the background thread to stop, then blocks until it actually has -
    // "shuts down cleanly" means this returns only once the thread has genuinely
    // exited, not just that we asked it to.
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.compaction_thread.take() {
            let _ = handle.join();
        }
    }
}

// Rebuilds LevelManager on startup from whatever SSTable files already exist in dir,
// so files flushed/compacted in a previous session aren't left orphaned and invisible.
// Level is read from each file's own name (l0_..., l1_..., matching next_sstable_path
// and LevelManager::compact's naming), no separate manifest file needed. Files within
// each level are sorted by name, which sorts chronologically here since the suffix is
// a nanosecond timestamp, so get's newest-to-oldest L0 scan still means something.
fn discover_levels(dir: &Path) -> Result<LevelManager, LsmError> {
    let mut manager = LevelManager::new();
    manager.levels.push(Vec::new()); // L0
    manager.levels.push(Vec::new()); // L1

    for entry in std::fs::read_dir(dir).map_err(|_| LsmError::Io)? {
        let path = entry.map_err(|_| LsmError::Io)?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.ends_with(".sst") {
            continue;
        }

        let Some(level_str) = name.strip_prefix('l').and_then(|rest| rest.split('_').next())
        else {
            continue;
        };
        let Ok(level) = level_str.parse::<usize>() else {
            continue;
        };

        while manager.levels.len() <= level {
            manager.levels.push(Vec::new());
        }
        manager.levels[level].push(path);
    }

    for level in manager.levels.iter_mut() {
        level.sort();
    }

    Ok(manager)
}

impl Lsm {
    pub fn open(dir: &Path) -> Result<Self, LsmError> {
        std::fs::create_dir_all(dir).map_err(|_| LsmError::Io)?;

        let wal_path = dir.join("wal.log");
        let wal = Wal::open(&wal_path)?;

        // rebuild the MemTable from whatever the WAL already holds, in case the
        // process was restarted after writes were appended but before they were
        // flushed to an SSTable
        let mut mem = MemTable::new();
        for entry in Wal::replay(&wal_path)? {
            match entry {
                WalEntry::Put { key, value } => mem.set(key, value),
                WalEntry::Delete { key } => mem.delete(key),
            }
        }

        let levels = Arc::new(RwLock::new(discover_levels(dir)?));
        let shutdown = Arc::new(AtomicBool::new(false));

        let compaction_thread = {
            let levels = Arc::clone(&levels);
            let shutdown = Arc::clone(&shutdown);
            thread::spawn(move || {
                while !shutdown.load(Ordering::Relaxed) {
                    thread::sleep(COMPACTION_POLL_INTERVAL);

                    let needs_compaction = levels.read().unwrap().needs_compaction();
                    if needs_compaction {
                        if let Err(err) = levels.write().unwrap().compact() {
                            eprintln!("background compaction failed: {err:?}");
                        }
                    }
                }
            })
        };

        Ok(Lsm {
            dir: dir.to_path_buf(),
            mem: RwLock::new(mem),
            wal: Mutex::new(wal),
            levels,
            flush_threshold: DEFAULT_FLUSH_THRESHOLD,
            shutdown,
            compaction_thread: Some(compaction_thread),
        })
    }

    pub fn set(&self, key: Vec<u8>, value: Vec<u8>) -> Result<(), LsmError> {
        {
            let mut wal = self.wal.lock().unwrap();
            wal.append(&WalEntry::Put {
                key: key.clone(),
                value: value.clone(),
            })?;
        }

        let needs_flush = {
            let mut mem = self.mem.write().unwrap();
            mem.set(key, value);
            mem.size_bytes() >= self.flush_threshold
        };

        if needs_flush {
            self.flush_memtable()?;
        }

        Ok(())
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, LsmError> {
        // MemTable first: it's the only place still-unflushed, most current writes live
        {
            let mem = self.mem.read().unwrap();
            if let Some(value) = mem.get(key) {
                return Ok(if is_tombstone(value) {
                    None
                } else {
                    Some(value.clone())
                });
            }
        }

        let levels = self.levels.read().unwrap();

        // L0: files were pushed in append order (oldest at index 0), so newest-to-oldest
        // means walking the list in reverse
        if let Some(l0_files) = levels.levels.first() {
            for path in l0_files.iter().rev() {
                let mut reader = SsTableReader::open(path)?;
                if let Some(value) = reader.get_raw(key)? {
                    return Ok(if is_tombstone(&value) { None } else { Some(value) });
                }
            }
        }

        // L1+: each level's files have non-overlapping ranges, so at most one file per
        // level can actually hold the key; get_raw's Bloom filter check makes checking
        // every file in a level cheap for the ones that don't
        for level in levels.levels.iter().skip(1) {
            for path in level {
                let mut reader = SsTableReader::open(path)?;
                if let Some(value) = reader.get_raw(key)? {
                    return Ok(if is_tombstone(&value) { None } else { Some(value) });
                }
            }
        }

        Ok(None)
    }

    pub fn delete(&self, key: Vec<u8>) -> Result<(), LsmError> {
        {
            let mut wal = self.wal.lock().unwrap();
            wal.append(&WalEntry::Delete { key: key.clone() })?;
        }

        let needs_flush = {
            let mut mem = self.mem.write().unwrap();
            mem.delete(key);
            mem.size_bytes() >= self.flush_threshold
        };

        if needs_flush {
            self.flush_memtable()?;
        }

        Ok(())
    }

    fn flush_memtable(&self) -> Result<(), LsmError> {
        let path = self.next_sstable_path();

        // Held for the whole flush, not just the MemTable swap below: if it were
        // released right after the swap, a concurrent set/delete could append a WAL
        // entry for data that's now sitting only in the fresh MemTable, and the
        // truncate at the end would wipe that entry out even though it was never
        // actually included in this flush - losing it permanently on a crash before
        // the next flush. Holding the lock the whole time blocks new WAL appends
        // until the old data is safely on disk and the WAL has been truncated to
        // match. Consistent lock order with set/delete (wal always acquired before
        // mem) avoids a deadlock between this and them.
        let mut wal = self.wal.lock().unwrap();

        // swap the full MemTable out for an empty one
        let old_mem = {
            let mut mem = self.mem.write().unwrap();
            std::mem::replace(&mut *mem, MemTable::new())
        };

        old_mem.flush(&path)?;
        wal.truncate()?;
        drop(wal);

        let mut levels = self.levels.write().unwrap();
        levels.add_l0_file(path);

        Ok(())
    }

    fn next_sstable_path(&self) -> PathBuf {
        let unique_suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        self.dir.join(format!("l0_{unique_suffix}.sst"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env::temp_dir;
    use std::sync::Arc;
    use std::thread;

    fn test_dir(name: &str) -> PathBuf {
        let dir = temp_dir().join(format!("tabula_lsm_test_{name}"));
        std::fs::remove_dir_all(&dir).ok();
        dir
    }

    #[test]
    fn set_then_get_returns_the_value() {
        let dir = test_dir("basic");
        let lsm = Lsm::open(&dir).unwrap();

        lsm.set(b"hello".to_vec(), b"world".to_vec()).unwrap();
        assert_eq!(lsm.get(b"hello").unwrap(), Some(b"world".to_vec()));
        assert_eq!(lsm.get(b"missing").unwrap(), None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delete_hides_the_key_even_before_any_flush() {
        let dir = test_dir("delete_in_memtable");
        let lsm = Lsm::open(&dir).unwrap();

        lsm.set(b"hello".to_vec(), b"world".to_vec()).unwrap();
        lsm.delete(b"hello".to_vec()).unwrap();
        assert_eq!(lsm.get(b"hello").unwrap(), None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn set_survives_a_flush_to_l0() {
        let dir = test_dir("flush");
        let lsm = Lsm::open(&dir).unwrap();

        // exceed the flush threshold in one write so it flushes immediately
        lsm.set(b"big".to_vec(), vec![0u8; DEFAULT_FLUSH_THRESHOLD])
            .unwrap();
        lsm.set(b"after-flush".to_vec(), b"still works".to_vec())
            .unwrap();

        assert_eq!(
            lsm.get(b"big").unwrap(),
            Some(vec![0u8; DEFAULT_FLUSH_THRESHOLD])
        );
        assert_eq!(
            lsm.get(b"after-flush").unwrap(),
            Some(b"still works".to_vec())
        );

        {
            let levels = lsm.levels.read().unwrap();
            assert_eq!(
                levels.levels[0].len(),
                1,
                "the oversized set should have triggered exactly one L0 flush"
            );
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delete_after_flush_shadows_the_flushed_value() {
        let dir = test_dir("delete_after_flush");
        let lsm = Lsm::open(&dir).unwrap();

        lsm.set(b"key".to_vec(), vec![0u8; DEFAULT_FLUSH_THRESHOLD])
            .unwrap();
        // "key" is now in an L0 SSTable, not the MemTable
        lsm.delete(b"key".to_vec()).unwrap();

        assert_eq!(lsm.get(b"key").unwrap(), None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn open_replays_the_wal_after_a_restart() {
        let dir = test_dir("replay");
        {
            let lsm = Lsm::open(&dir).unwrap();
            lsm.set(b"hello".to_vec(), b"world".to_vec()).unwrap();
            // lsm dropped here without any explicit shutdown, simulating a restart
        }

        let reopened = Lsm::open(&dir).unwrap();
        assert_eq!(reopened.get(b"hello").unwrap(), Some(b"world".to_vec()));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn concurrent_writes_during_a_flush_are_not_lost_on_restart() {
        // regression test for the WAL truncate race: a write landing in the WAL
        // between another thread's MemTable swap and its post-flush truncate must
        // not be silently erased by that truncate
        let dir = test_dir("wal_race");
        let lsm = Arc::new(Lsm::open(&dir).unwrap());

        let flushing = lsm.clone();
        let flush_thread = thread::spawn(move || {
            flushing
                .set(b"big".to_vec(), vec![0u8; DEFAULT_FLUSH_THRESHOLD])
                .unwrap();
        });

        let writer = lsm.clone();
        let writer_thread = thread::spawn(move || {
            for i in 0..50 {
                writer
                    .set(format!("concurrent-{i}").into_bytes(), b"v".to_vec())
                    .unwrap();
            }
        });

        flush_thread.join().unwrap();
        writer_thread.join().unwrap();
        drop(lsm);

        let reopened = Lsm::open(&dir).unwrap();
        assert_eq!(
            reopened.get(b"big").unwrap(),
            Some(vec![0u8; DEFAULT_FLUSH_THRESHOLD])
        );
        for i in 0..50 {
            let key = format!("concurrent-{i}");
            assert_eq!(
                reopened.get(key.as_bytes()).unwrap(),
                Some(b"v".to_vec()),
                "{key} was lost across restart"
            );
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn background_thread_compacts_l0_without_being_called_directly() {
        let dir = test_dir("background_compaction");
        let lsm = Lsm::open(&dir).unwrap();

        // 4 oversized writes each individually trigger a flush, producing 4 L0 files -
        // needs_compaction's threshold - without ever calling compact() ourselves
        for i in 0..4 {
            lsm.set(
                format!("big-{i}").into_bytes(),
                vec![0u8; DEFAULT_FLUSH_THRESHOLD],
            )
            .unwrap();
        }

        // give the background thread a couple of poll cycles to notice and compact
        thread::sleep(COMPACTION_POLL_INTERVAL * 3);

        {
            let levels = lsm.levels.read().unwrap();
            assert!(
                levels.levels[0].is_empty(),
                "background thread should have compacted L0 away on its own"
            );
            assert_eq!(levels.levels[1].len(), 1);
        }

        // data must still be readable after the automatic compaction
        for i in 0..4 {
            let key = format!("big-{i}");
            assert_eq!(
                lsm.get(key.as_bytes()).unwrap(),
                Some(vec![0u8; DEFAULT_FLUSH_THRESHOLD])
            );
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dropping_lsm_shuts_down_the_background_thread() {
        let dir = test_dir("clean_shutdown");
        let lsm = Lsm::open(&dir).unwrap();

        // Drop's join() blocks until the thread has actually exited; if shutdown
        // signaling were broken, this would hang instead of returning
        drop(lsm);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn concurrent_writers_and_readers_1000_ops_each() {
        const WRITERS: usize = 4;
        const READERS: usize = 4;
        const OPS_PER_THREAD: usize = 1000;

        let dir = test_dir("stress");
        let lsm = Arc::new(Lsm::open(&dir).unwrap());

        // never written by any thread below: readers use this to prove they can never
        // observe torn/corrupted data, even while writers are mutating other keys
        // concurrently on the same locks
        lsm.set(b"baseline".to_vec(), b"stable-value".to_vec())
            .unwrap();

        let mut handles = Vec::new();

        // writers: each owns a disjoint key range, so the expected final state per key
        // is deterministic once every thread has finished, with no cross-writer races
        for writer_id in 0..WRITERS {
            let lsm = Arc::clone(&lsm);
            handles.push(thread::spawn(move || {
                for i in 0..OPS_PER_THREAD {
                    let key = format!("writer-{writer_id}-{i}").into_bytes();
                    let value = format!("value-{writer_id}-{i}").into_bytes();
                    lsm.set(key, value).unwrap();
                }
            }));
        }

        // readers: run concurrently with the writers above, repeatedly reading the
        // untouched baseline key (must always come back correct) and opportunistically
        // probing keys the writers may or may not have written yet (no assertion on
        // those beyond "must not panic", since that outcome is inherently racy)
        for reader_id in 0..READERS {
            let lsm = Arc::clone(&lsm);
            handles.push(thread::spawn(move || {
                for i in 0..OPS_PER_THREAD {
                    let baseline = lsm.get(b"baseline").unwrap();
                    assert_eq!(
                        baseline,
                        Some(b"stable-value".to_vec()),
                        "reader {reader_id} saw a corrupted baseline value on op {i}"
                    );

                    let probe_writer = i % WRITERS;
                    let probe_key = format!("writer-{probe_writer}-{i}").into_bytes();
                    lsm.get(&probe_key).unwrap();
                }
            }));
        }

        // .unwrap() here is the actual "assert no panics" check: if any spawned
        // closure panicked, join() returns Err and this unwrap fails the test
        for handle in handles {
            handle.join().unwrap();
        }

        // final state check: every write from every thread must be present with
        // exactly its expected value, now that all racing is done
        for writer_id in 0..WRITERS {
            for i in 0..OPS_PER_THREAD {
                let key = format!("writer-{writer_id}-{i}");
                let expected = format!("value-{writer_id}-{i}").into_bytes();
                assert_eq!(
                    lsm.get(key.as_bytes()).unwrap(),
                    Some(expected),
                    "{key} missing or wrong after all threads joined"
                );
            }
        }
        assert_eq!(
            lsm.get(b"baseline").unwrap(),
            Some(b"stable-value".to_vec())
        );

        drop(lsm);
        std::fs::remove_dir_all(&dir).ok();
    }
}
