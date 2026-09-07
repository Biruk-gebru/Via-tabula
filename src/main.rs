mod bloom;
mod compaction;
mod memtable;
mod sstable;
mod types;
mod wal;

use crate::memtable::MemTable;

fn main() {
    let mut ltsm: MemTable = MemTable::new();
    ltsm.set(b"hello".to_vec(), b"world".to_vec());
    println!(
        "{:?}",
        ltsm.get(b"hello").map(|v| String::from_utf8_lossy(v))
    );
    ltsm.delete(b"hello".to_vec());
    println!(
        "{:?}",
        ltsm.get(b"hello").map(|v| String::from_utf8_lossy(v))
    );
}
