# Tabula

Tabula is a small LSM tree storage engine written in Rust, built as a learning project. It's the same kind of engine that sits underneath real databases like LevelDB, RocksDB, and Cassandra, just built from scratch and kept small enough to actually read and understand in one sitting.

This isn't meant to be a production database. It's meant to teach how one actually works, piece by piece, with zero runtime dependencies for the core logic (the hash functions, the checksum, everything is hand rolled instead of pulled in from a crate).

## What an LSM tree is, in one paragraph

Writing straight to disk in a random order is slow. An LSM tree avoids that by always writing sequentially and never touching a file in place once it's written. New writes land in memory first, get flushed to disk as an immutable file once they build up, and a background process periodically merges those files back together to keep things tidy. You trade a bit of extra work later (compaction) for writes that are cheap right now.

## The moving parts

- **MemTable** (`src/memtable.rs`): the only mutable thing in the whole system. A sorted in-memory map that every write hits first. Deletes are recorded as tombstones, not actually removed, so an older value on disk can't accidentally come back to life.

- **WAL** (`src/wal.rs`): the write-ahead log. Every write gets appended here before it touches the MemTable, so if the process crashes before that data makes it to disk, it can be replayed back on restart. Each entry carries a CRC32 checksum so a partially written entry from a crash gets detected and skipped instead of corrupting the database.

- **SSTable** (`src/sstable/`): once the MemTable gets big enough, it gets flushed to disk as a Sorted String Table, an immutable file with a sparse index for fast lookups and a Bloom filter so a reader can skip opening files that definitely don't contain the key it's after.

- **Bloom filter** (`src/bloom.rs`): a small, hand built probabilistic structure. It can say "this key is definitely not here" for free, which saves a disk read on every miss.

- **Compaction and levels** (`src/compaction.rs`, `src/levels.rs`): as SSTables pile up, a background job merges them, drops old tombstones, and keeps only the newest value for each key. Files are organized into levels so a read only ever has to check a small number of files instead of every file ever written.

- **The engine** (`src/lsm.rs`): ties everything above into one `Lsm` struct that many threads can read and write at once. Reads check the MemTable first, then the SSTables newest to oldest. A background thread runs compaction on its own schedule and shuts down cleanly when the engine is dropped.

## Trying it out

```
cargo run
cargo test
cargo bench
```

## How this was built

This project follows `GUIDE.md`, a self study guide with ten milestones, each one a real piece of the engine: the MemTable, the WAL, SSTables, Bloom filters, compaction, concurrency, benchmarking, and crash recovery. Every milestone has a design question to answer before writing any code, and a checkpoint to pass before moving to the next one.
