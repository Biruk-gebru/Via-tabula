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
