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
