use crate::sstable::writer::SsTableError;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub struct SsTableReader {
    file: File,
    // sparse index: (key, byte offset of that key's record in the data section)
    index: Vec<(Vec<u8>, u64)>,
}

pub struct SsTableIter;

impl SsTableReader {
    pub fn open(path: &Path) -> Result<Self, SsTableError> {
        // read the 16B footer to learn where the index block starts and how long it is
        let mut file = File::open(path).map_err(|_| SsTableError::Io)?;

        file.seek(SeekFrom::End(-16))
            .map_err(|_| SsTableError::Io)?;

        let mut footer = [0u8; 16];
        file.read_exact(&mut footer).map_err(|_| SsTableError::Io)?;

        let index_offset = u64::from_le_bytes(footer[0..8].try_into().unwrap());
        let index_len = u64::from_le_bytes(footer[8..16].try_into().unwrap());

        // seek to the index block and read it in full
        file.seek(SeekFrom::Start(index_offset))
            .map_err(|_| SsTableError::Io)?;

        let mut index_bytes = vec![0u8; index_len as usize];
        file.read_exact(&mut index_bytes)
            .map_err(|_| SsTableError::Io)?;

        // parse repeated [key_len: 4B][key][offset: 8B] entries until the block is consumed
        let mut index = Vec::new();
        let mut pos = 0usize;
        while pos < index_bytes.len() {
            let key_len =
                u32::from_le_bytes(index_bytes[pos..pos + 4].try_into().unwrap()) as usize;
            pos += 4;

            let key = index_bytes[pos..pos + key_len].to_vec();
            pos += key_len;

            let offset = u64::from_le_bytes(index_bytes[pos..pos + 8].try_into().unwrap());
            pos += 8;

            index.push((key, offset));
        }

        Ok(SsTableReader { file, index })
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, SsTableError> {
        todo!("binary-search self.index, seek, scan forward for key")
    }

    pub fn iter(&self) -> SsTableIter {
        todo!("full sequential scan of the data section")
    }
}
