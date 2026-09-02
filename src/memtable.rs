// question: why does BTreeMap store Vec<u8> instead of &str/&[u8] — what breaks with references?
// answer: references borrow from a caller's value; when that caller drops their value the reference dangles.
//         Vec<u8> moves ownership into the map so the map fully owns its data and no lifetime is needed.
use std::collections::BTreeMap;
use std::ops::Bound;
use std::path::{Path, PathBuf};

use crate::sstable::writer::{SsTableError, SsTableWriter};
use crate::types::is_tombstone;

pub struct MemTable {
    data: BTreeMap<Vec<u8>, Vec<u8>>,
}
impl MemTable {
    pub fn new() -> Self {
        MemTable {
            data: BTreeMap::new(),
        }
    }
    pub fn get(&self, key: &[u8]) -> Option<&Vec<u8>> {
        self.data.get(key)
    }
    pub fn set(&mut self, key: Vec<u8>, value: Vec<u8>) {
        self.data.insert(key, value);
    }
    pub fn delete(&mut self, key: Vec<u8>) {
        self.data.insert(key, b"__TOMBSTONE__".to_vec());
    } //store tombstone

    #[allow(dead_code)]
    pub fn size_bytes(&self) -> usize {
        self.data.iter().map(|(k, v)| k.len() + v.len()).sum()
    }

    pub fn flush(&self, path: &Path) -> Result<PathBuf, SsTableError> {
        let mut writer = SsTableWriter::new(path, self.data.len())?;
        for (key, value) in self {
            writer.add(key, value)?;
        }
        writer.finish()?;
        Ok(path.to_path_buf())
    }

    pub fn scan<'a>(
        &'a self,
        start: &[u8],
        end: &[u8],
    ) -> impl Iterator<Item = (&'a Vec<u8>, &'a Vec<u8>)> {
        self.data
            .range::<[u8], _>((Bound::Included(start), Bound::Excluded(end)))
            .filter(|(_, v)| !is_tombstone(v))
    }
}

impl<'a> IntoIterator for &'a MemTable {
    type Item = (&'a Vec<u8>, &'a Vec<u8>);
    type IntoIter = Box<dyn Iterator<Item = (&'a Vec<u8>, &'a Vec<u8>)> + 'a>;
    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.data.iter().filter(|(_, v)| !is_tombstone(v)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_path(name: &str) -> PathBuf {
        PathBuf::from(format!("/tmp/tabula_test_{}.sst", name))
    }

    #[test]
    fn test_set_and_get() {
        let mut t = MemTable::new();
        t.set(b"hello".to_vec(), b"test".to_vec());
        assert_eq!(t.get(b"hello"), Some(&b"test".to_vec()));
    }

    #[test]
    fn test_delete() {
        let mut t = MemTable::new();
        t.set(b"hello".to_vec(), b"test".to_vec());
        t.delete(b"hello".to_vec());
        assert_eq!(t.get(b"hello"), Some(&b"__TOMBSTONE__".to_vec()));
    }

    #[test]
    fn test_size() {
        let mut t = MemTable::new();
        t.set(b"hello".to_vec(), b"test".to_vec());
        let size = t.size_bytes();
        assert_eq!(size, 9);
    }

    #[test]
    fn test_scan() {
        let mut t = MemTable::new();
        t.set(b"apple".to_vec(), b"1".to_vec());
        t.set(b"mango".to_vec(), b"2".to_vec());
        t.set(b"banana".to_vec(), b"3".to_vec());
        t.set(b"cherry".to_vec(), b"4".to_vec());

        let results: Vec<_> = t.scan(b"banana", b"mango").collect();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, &b"banana".to_vec());
        assert_eq!(results[1].0, &b"cherry".to_vec());
    }

    #[test]
    fn test_flush_creates_file() {
        let mut t = MemTable::new();
        t.set(b"apple".to_vec(), b"1".to_vec());
        t.set(b"banana".to_vec(), b"2".to_vec());
        t.set(b"cherry".to_vec(), b"3".to_vec());

        let path = tmp_path("flush_creates");
        let _ = std::fs::remove_file(&path);

        let result = t.flush(&path).unwrap();
        assert_eq!(result, path);
        assert!(path.exists());

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn test_flush_file_has_valid_footer() {
        let mut t = MemTable::new();
        t.set(b"apple".to_vec(), b"1".to_vec());
        t.set(b"banana".to_vec(), b"2".to_vec());

        let path = tmp_path("flush_footer");
        let _ = std::fs::remove_file(&path);
        t.flush(&path).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert!(
            bytes.len() >= 16,
            "file must be at least 16 bytes for footer"
        );

        let footer_start = bytes.len() - 16;
        let index_offset =
            u64::from_le_bytes(bytes[footer_start..footer_start + 8].try_into().unwrap());
        let index_len = u64::from_le_bytes(
            bytes[footer_start + 8..footer_start + 16]
                .try_into()
                .unwrap(),
        );

        assert!(
            index_offset < bytes.len() as u64,
            "index_offset must be inside file"
        );
        assert!(
            index_offset + index_len <= footer_start as u64,
            "index block must not overlap footer"
        );

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn test_flush_excludes_tombstones() {
        let mut t = MemTable::new();
        t.set(b"apple".to_vec(), b"1".to_vec());
        t.set(b"banana".to_vec(), b"2".to_vec());
        t.delete(b"apple".to_vec());

        let path = tmp_path("flush_tombstone");
        let _ = std::fs::remove_file(&path);
        t.flush(&path).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert!(
            !bytes.windows(b"apple".len()).any(|w| w == b"apple"),
            "tombstoned key must not appear in SSTable"
        );

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn test_scan_sorted_and_tombstone_filtered() {
        let mut t = MemTable::new();
        // inserted out of order intentionally
        t.set(b"fig".to_vec(), b"6".to_vec());
        t.set(b"apple".to_vec(), b"1".to_vec());
        t.set(b"kiwi".to_vec(), b"8".to_vec());
        t.set(b"date".to_vec(), b"4".to_vec());
        t.set(b"cherry".to_vec(), b"3".to_vec());
        t.set(b"elderberry".to_vec(), b"5".to_vec());
        t.set(b"banana".to_vec(), b"2".to_vec());
        t.set(b"grape".to_vec(), b"7".to_vec());
        t.set(b"honeydew".to_vec(), b"8".to_vec());
        t.set(b"jackfruit".to_vec(), b"9".to_vec());

        // delete one key inside the range — should not appear
        t.delete(b"elderberry".to_vec());

        let results: Vec<_> = t.scan(b"banana", b"kiwi").collect();

        // expected sorted: banana, cherry, date, fig, grape, honeydew (elderberry filtered)
        let keys: Vec<&Vec<u8>> = results.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            keys,
            vec![
                &b"banana".to_vec(),
                &b"cherry".to_vec(),
                &b"date".to_vec(),
                &b"fig".to_vec(),
                &b"grape".to_vec(),
                &b"honeydew".to_vec(),
                &b"jackfruit".to_vec(),
            ]
        );
    }
}
