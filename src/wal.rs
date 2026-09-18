//i wil store 3 thinsg op key and value both key and value along ther length before they start
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

#[derive(Debug)]
pub enum WalEntry {
    Put { key: Vec<u8>, value: Vec<u8> },
    Delete { key: Vec<u8> },
}

#[derive(Debug)]
pub enum WalError {
    // Not enough bytes present to even parse the entry's declared shape - the OS
    // flushed a prefix of the entry and nothing more, the classic partial-write case.
    Error,
    // Enough bytes were present and parsed cleanly, but the checksum computed over
    // them doesn't match what was stored - the bytes were altered after being
    // written (or a length field itself was corrupted, producing a "valid-looking"
    // but wrong slice). Length-checking alone can never catch this.
    Checksum,
}

// Standard CRC-32 (IEEE 802.3), same algorithm the crc32fast crate implements -
// hand-rolled here to keep this project's zero-runtime-dependency scope,
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFFFFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = if crc & 1 != 0 { 0xEDB88320 } else { 0 };
            crc = (crc >> 1) ^ mask;
        }
    }
    !crc
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
        // checksum covers every byte written above (op, lengths, key, value), so any
        // change to any of them is caught on decode
        let checksum = crc32(&code);
        code.extend_from_slice(&checksum.to_le_bytes());
        code
    }

    pub fn decode(code: &[u8]) -> Result<(WalEntry, usize), WalError> {
        // op tag: 1 byte. Every subsequent bounds check below exists because this
        // buffer may be a truncated tail entry - a real partial write, not a bug -
        // and slicing past its end must return an error, never panic.
        let op = *code.first().ok_or(WalError::Error)?;

        let (entry, body_len) = match op {
            0u8 => {
                if code.len() < 5 {
                    return Err(WalError::Error);
                }
                let key_len = u32::from_le_bytes(code[1..5].try_into().unwrap()) as usize;

                let val_len_start = 5 + key_len;
                if code.len() < val_len_start + 4 {
                    return Err(WalError::Error);
                }
                let key = code[5..val_len_start].to_vec();
                let val_len =
                    u32::from_le_bytes(code[val_len_start..val_len_start + 4].try_into().unwrap())
                        as usize;

                let val_start = val_len_start + 4;
                let body_len = val_start + val_len;
                if code.len() < body_len {
                    return Err(WalError::Error);
                }
                let value = code[val_start..body_len].to_vec();

                (WalEntry::Put { key, value }, body_len)
            }
            1u8 => {
                if code.len() < 5 {
                    return Err(WalError::Error);
                }
                let key_len = u32::from_le_bytes(code[1..5].try_into().unwrap()) as usize;

                let body_len = 5 + key_len;
                if code.len() < body_len {
                    return Err(WalError::Error);
                }
                let key = code[5..body_len].to_vec();

                (WalEntry::Delete { key }, body_len)
            }
            _ => return Err(WalError::Error),
        };

        // the 4-byte checksum trails the entry's body; still need those bytes present
        if code.len() < body_len + 4 {
            return Err(WalError::Error);
        }
        let stored_checksum = u32::from_le_bytes(code[body_len..body_len + 4].try_into().unwrap());
        let actual_checksum = crc32(&code[0..body_len]);
        if actual_checksum != stored_checksum {
            return Err(WalError::Checksum);
        }

        Ok((entry, body_len + 4))
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
            match WalEntry::decode(&bytes[cursor..]) {
                Ok((entry, consumed)) => {
                    entries.push(entry);
                    cursor += consumed;
                }
                Err(_) => {
                    // The WAL is append-only: everything before this point was
                    // already fully appended and durable before this entry was ever
                    // started, so only the tail can plausibly be an in-progress
                    // write interrupted by a crash. Stop here and keep what decoded
                    // cleanly rather than failing the whole replay - and thus
                    // refusing to start up at all - over one truncated or corrupted
                    // trailing entry.
                    break;
                }
            }
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
        let entry = WalEntry::Delete {
            key: b"bye".to_vec(),
        };
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
    fn test_decode_truncated_entry_returns_error_not_panic() {
        let entry = WalEntry::Put {
            key: b"hello".to_vec(),
            value: b"world".to_vec(),
        };
        let bytes = entry.encode();

        // simulate the OS having flushed only a prefix of the entry: try every
        // possible truncation point, none of them should ever panic
        for cut in 0..bytes.len() {
            let result = WalEntry::decode(&bytes[..cut]);
            assert!(
                matches!(result, Err(WalError::Error)),
                "truncating to {cut} bytes should return WalError::Error, got {result:?}"
            );
        }
    }

    #[test]
    fn test_decode_corrupted_bytes_returns_checksum_error() {
        let entry = WalEntry::Put {
            key: b"hello".to_vec(),
            value: b"world".to_vec(),
        };
        let mut bytes = entry.encode();

        // flip a bit inside the value, well within bounds, so length checks all pass
        // and only the checksum comparison can catch this
        let last = bytes.len() - 5; // last byte of "world", before the 4B checksum
        bytes[last] ^= 0xFF;

        let result = WalEntry::decode(&bytes);
        assert!(
            matches!(result, Err(WalError::Checksum)),
            "corrupted body should return WalError::Checksum, got {result:?}"
        );
    }

    #[test]
    fn test_append_and_replay() {
        let path = PathBuf::from("/tmp/tabula_test_wal.bin");
        let _ = std::fs::remove_file(&path);

        let mut wal = Wal::open(&path).unwrap();
        wal.append(&WalEntry::Put {
            key: b"foo".to_vec(),
            value: b"bar".to_vec(),
        })
        .unwrap();
        wal.append(&WalEntry::Delete {
            key: b"foo".to_vec(),
        })
        .unwrap();
        wal.append(&WalEntry::Put {
            key: b"baz".to_vec(),
            value: b"qux".to_vec(),
        })
        .unwrap();

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

    #[test]
    fn test_replay_stops_at_truncated_tail_without_failing() {
        let path = PathBuf::from("/tmp/tabula_test_wal_truncated_tail.bin");
        let _ = std::fs::remove_file(&path);

        let mut raw = Vec::new();
        raw.extend(
            WalEntry::Put {
                key: b"foo".to_vec(),
                value: b"bar".to_vec(),
            }
            .encode(),
        );
        raw.extend(
            WalEntry::Delete {
                key: b"foo".to_vec(),
            }
            .encode(),
        );

        // simulate a crash mid-append: write only a prefix of a third entry's bytes
        // directly to the file, bypassing Wal::append entirely, so this isn't an
        // artificial in-memory slice - it's an actual truncated file on disk
        let third_entry_bytes = WalEntry::Put {
            key: b"baz".to_vec(),
            value: b"qux".to_vec(),
        }
        .encode();
        raw.extend(&third_entry_bytes[..third_entry_bytes.len() - 3]);

        std::fs::write(&path, &raw).unwrap();

        // must recover the two complete entries, not fail to open at all over the
        // truncated third one
        let entries = Wal::replay(&path).unwrap();
        assert_eq!(entries.len(), 2, "should recover exactly the complete entries");

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

        std::fs::remove_file(&path).unwrap();
    }
}
