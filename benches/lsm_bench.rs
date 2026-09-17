// M9 baseline (cargo bench, real defaults: 100 samples, 3s warm-up, 5s measurement):
//   set_sequential_keys                ~51.3 µs
//   set_random_keys                    ~258.6 µs   (~5x slower than sequential - random
//                                                    keys force BTreeMap rebalancing all
//                                                    over the tree, sequential ones only
//                                                    ever insert at the current max)
//   get_key_present                    ~41.2 ns
//   get_key_absent_bloom_filter_path   ~42.9 ns    (essentially identical to present -
//                                                    not what naive intuition expects
//                                                    from "the Bloom filter skips work")
//   memtable_flush_1000_keys           ~73.2 µs
//
// Flamegraph of memtable_flush_1000_keys (via a dedicated src/bin/profile_flush.rs
// loop, not the criterion harness itself - profiling `cargo bench` directly showed
// criterion's own bootstrap-resampling/formatting machinery as the top "hotspots",
// which is a real example of a profiler lying to you if misread) showed SsTableWriter
// ::add at 40.6% of flush's on-CPU time and a memmove at 15.6%, both from add()'s four
// separate write_all calls each copying into BufWriter's internal buffer on their own.
//
// Optimization attempt 1: combined add()'s four write_all calls into one (build the
// record in a reused buffer, write it once). Re-measured: no significant change
// (p = 0.47). Reason: BufWriter already batches writes internally regardless of
// userspace call count, and cargo flamegraph only samples on-CPU time - it can't see
// time a thread spends blocked waiting on a write() syscall, so it over-weighted the
// cheap userspace copies relative to their real share of wall-clock time.
//
// Optimization attempt 2: BufWriter's default capacity (8 KB) is smaller than a
// typical flush's data section (~30 KB for this benchmark), so its internal buffer
// already fills and triggers a real write() syscall multiple times per flush
// regardless of attempt 1's userspace batching. Raised it to 64 KB
// (WRITER_BUFFER_CAPACITY in sstable/writer.rs), aiming to collapse most flushes to a
// single real syscall. Re-measured: memtable_flush_1000_keys ~59.6 µs, a ~9.3% drop
// (p = 0.00, "Performance has improved"). This is the one that actually targeted the
// real bottleneck (syscall count, not userspace copy count).

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use std::hint::black_box;
use std::path::PathBuf;

use tabula::lsm::Lsm;
use tabula::memtable::MemTable;

// Tiny deterministic PRNG (xorshift64) so the "random keys" benchmark is reproducible
// without pulling in the `rand` crate, matching this project's zero-dependency scope.
struct Xorshift64(u64);

impl Xorshift64 {
    fn new(seed: u64) -> Self {
        Xorshift64(seed)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

fn bench_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tabula_bench_{name}"));
    std::fs::remove_dir_all(&dir).ok();
    dir
}

fn bench_set_sequential(c: &mut Criterion) {
    let dir = bench_dir("set_sequential");
    let lsm = Lsm::open(&dir).unwrap();
    let mut i: u64 = 0;

    c.bench_function("set_sequential_keys", |b| {
        b.iter(|| {
            let key = format!("key-{i:020}").into_bytes();
            lsm.set(black_box(key), black_box(b"value".to_vec()))
                .unwrap();
            i += 1;
        });
    });

    std::fs::remove_dir_all(&dir).ok();
}

fn bench_set_random(c: &mut Criterion) {
    let dir = bench_dir("set_random");
    let lsm = Lsm::open(&dir).unwrap();
    let mut rng = Xorshift64::new(42);

    c.bench_function("set_random_keys", |b| {
        b.iter(|| {
            let key = format!("key-{:020}", rng.next_u64()).into_bytes();
            lsm.set(black_box(key), black_box(b"value".to_vec()))
                .unwrap();
        });
    });

    std::fs::remove_dir_all(&dir).ok();
}

fn bench_get_present(c: &mut Criterion) {
    let dir = bench_dir("get_present");
    let lsm = Lsm::open(&dir).unwrap();
    for i in 0..1000u32 {
        lsm.set(format!("key-{i:05}").into_bytes(), b"value".to_vec())
            .unwrap();
    }

    c.bench_function("get_key_present", |b| {
        b.iter(|| {
            black_box(lsm.get(black_box(b"key-00500")).unwrap());
        });
    });

    std::fs::remove_dir_all(&dir).ok();
}

fn bench_get_absent(c: &mut Criterion) {
    let dir = bench_dir("get_absent");
    let lsm = Lsm::open(&dir).unwrap();
    for i in 0..1000u32 {
        lsm.set(format!("key-{i:05}").into_bytes(), b"value".to_vec())
            .unwrap();
    }

    // this key was never inserted, so every call takes the Bloom-filter-says-no path
    c.bench_function("get_key_absent_bloom_filter_path", |b| {
        b.iter(|| {
            black_box(lsm.get(black_box(b"this-key-does-not-exist")).unwrap());
        });
    });

    std::fs::remove_dir_all(&dir).ok();
}

fn bench_memtable_flush(c: &mut Criterion) {
    let dir = bench_dir("flush");
    let path = dir.join("flush.sst");
    std::fs::create_dir_all(&dir).unwrap();

    c.bench_function("memtable_flush_1000_keys", |b| {
        b.iter_batched(
            // setup: build the MemTable outside the timed portion, so this benchmark
            // measures flush alone, not the writes leading up to it
            || {
                let mut mem = MemTable::new();
                for i in 0..1000u32 {
                    mem.set(format!("key-{i:05}").into_bytes(), b"value".to_vec());
                }
                mem
            },
            |mem| {
                mem.flush(black_box(&path)).unwrap();
            },
            BatchSize::SmallInput,
        );
    });

    std::fs::remove_dir_all(&dir).ok();
}

criterion_group!(
    benches,
    bench_set_sequential,
    bench_set_random,
    bench_get_present,
    bench_get_absent,
    bench_memtable_flush
);
criterion_main!(benches);
