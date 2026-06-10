// question: why does BTreeMap store Vec<u8> instead of &str/&[u8] — what breaks with references?
// answer: references borrow from a caller's value; when that caller drops their value the reference dangles.
//         Vec<u8> moves ownership into the map so the map fully owns its data and no lifetime is needed.
use std::collections::BTreeMap;

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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
