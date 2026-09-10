//i wil store 3 thinsg op key and value both key and value along ther length before they start
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

pub enum WalEntry {
    Put { key: Vec<u8>, value: Vec<u8> },
    Delete { key: Vec<u8> },
}

#[derive(Debug)]
pub enum WalError {
    Error,
}

impl WalEntry {
    pub fn encode(&self) -> Vec<u8> {
        let mut code: Vec<u8> = Vec::new();
        match self {
            WalEntry::Put { key, value } => {
                let key_len = key.len() as u32;
                let value_len = value.len() as u32;
                code.push(0u8);
                code.extend_from_slice(&key_len.to_le_bytes());
                code.extend_from_slice(key);
                code.extend_from_slice(&value_len.to_le_bytes());
                code.extend_from_slice(value);
            }
            WalEntry::Delete { key } => {
                let key_len = key.len() as u32;
                code.push(1u8);
                code.extend_from_slice(&key_len.to_le_bytes());
                code.extend_from_slice(key);
            }
        }
        code
    }

    pub fn decode(code: &[u8]) -> Result<(WalEntry, usize), WalError> {
        match code[0] {
            0u8 => {
                let key_len = u32::from_le_bytes(code[1..5].try_into().unwrap()) as usize;
                let key = code[5..5 + key_len].to_vec();
                let val_len =
                    u32::from_le_bytes(code[5 + key_len..9 + key_len].try_into().unwrap()) as usize;
                let val = code[9 + key_len..9 + key_len + val_len].to_vec();
                Ok((
                    WalEntry::Put {
                        key: key,
                        value: val,
                    },
                    1 + key_len + 8 + val_len,
                ))
            }
            1u8 => {
                let key_len = u32::from_le_bytes(code[1..5].try_into().unwrap()) as usize;
                let key = code[5..5 + key_len].to_vec();
                Ok((WalEntry::Delete { key: key }, 1 + 4 + key_len))
            }
            _ => Err(WalError::Error),
        }
    }
}

pub struct Wal {
    file: File,
}

impl Wal {
    pub fn open(path: &Path) -> Result<Wal, WalError> {
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .map_err(|_| WalError::Error)?;
        Ok(Wal { file })
    }
    // Called once a flush has safely persisted everything the WAL was protecting, so
    // its contents are now redundant. Caller must ensure nothing else appends to the
    // WAL between the flush succeeding and this call, or a concurrent write's entry
    // could be wiped out here despite that write never having reached an SSTable.
    pub fn truncate(&mut self) -> Result<(), WalError> {
        self.file.set_len(0).map_err(|_| WalError::Error)?;
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|_| WalError::Error)?;
        Ok(())
    }

    pub fn append(&mut self, entry: &WalEntry) -> Result<(), WalError> {
        let bytes = entry.encode();
        self.file.write_all(&bytes).map_err(|_| WalError::Error)
    }

    pub fn replay(path: &Path) -> Result<Vec<WalEntry>, WalError> {
        let bytes = std::fs::read(path).map_err(|_| WalError::Error)?;
        let mut entries = Vec::new();
        let mut cursor = 0;

        while cursor < bytes.len() {
            let (entry, consumed) = WalEntry::decode(&bytes[cursor..])?;
            entries.push(entry);
            cursor += consumed;
        }

        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_encode_decode_put() {
        let entry = WalEntry::Put {
            key: b"hello".to_vec(),
            value: b"world".to_vec(),
        };
        let bytes = entry.encode();
        let (decoded, consumed) = WalEntry::decode(&bytes).unwrap();
        match decoded {
            WalEntry::Put { key, value } => {
                assert_eq!(key, b"hello");
                assert_eq!(value, b"world");
            }
            _ => panic!("expected Put"),
        }
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn test_encode_decode_delete() {
        let entry = WalEntry::Delete { key: b"bye".to_vec() };
        let bytes = entry.encode();
        let (decoded, consumed) = WalEntry::decode(&bytes).unwrap();
        match decoded {
            WalEntry::Delete { key } => assert_eq!(key, b"bye"),
            _ => panic!("expected Delete"),
        }
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn test_decode_invalid_op_returns_error() {
        let bad = vec![9u8, 0, 0, 0, 0];
        assert!(WalEntry::decode(&bad).is_err());
    }

    #[test]
    fn test_append_and_replay() {
        let path = PathBuf::from("/tmp/tabula_test_wal.bin");
        let _ = std::fs::remove_file(&path);

        let mut wal = Wal::open(&path).unwrap();
        wal.append(&WalEntry::Put { key: b"foo".to_vec(), value: b"bar".to_vec() }).unwrap();
        wal.append(&WalEntry::Delete { key: b"foo".to_vec() }).unwrap();
        wal.append(&WalEntry::Put { key: b"baz".to_vec(), value: b"qux".to_vec() }).unwrap();

        let entries = Wal::replay(&path).unwrap();
        assert_eq!(entries.len(), 3);

        match &entries[0] {
            WalEntry::Put { key, value } => {
                assert_eq!(key, b"foo");
                assert_eq!(value, b"bar");
            }
            _ => panic!("expected Put"),
        }
        match &entries[1] {
            WalEntry::Delete { key } => assert_eq!(key, b"foo"),
            _ => panic!("expected Delete"),
        }
        match &entries[2] {
            WalEntry::Put { key, value } => {
                assert_eq!(key, b"baz");
                assert_eq!(value, b"qux");
            }
            _ => panic!("expected Put"),
        }

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn test_replay_empty_file() {
        let path = PathBuf::from("/tmp/tabula_test_wal_empty.bin");
        std::fs::write(&path, b"").unwrap();
        let entries = Wal::replay(&path).unwrap();
        assert_eq!(entries.len(), 0);
        std::fs::remove_file(&path).unwrap();
    }
}
