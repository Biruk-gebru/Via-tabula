// Purpose-built for flamegraphing MemTable::flush in isolation: no criterion, no
// rayon, no formatting machinery in the process at all, so a profile of this binary
// contains (almost) nothing but the code actually under test. The MemTable is built
// once, outside the loop, and reused for every flush call (safe: flush takes &self,
// it doesn't consume or mutate), so set()'s cost doesn't contaminate the profile
// either - only SsTableWriter::new/add/finish's own cost should show up.
use tabula::memtable::MemTable;

fn main() {
    let dir = std::env::temp_dir().join("tabula_profile_flush");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("flush.sst");

    let mut mem = MemTable::new();
    for i in 0..1000u32 {
        mem.set(format!("key-{i:05}").into_bytes(), b"value".to_vec());
    }

    for _ in 0..5_000 {
        mem.flush(&path).unwrap();
    }

    std::fs::remove_dir_all(&dir).ok();
}
