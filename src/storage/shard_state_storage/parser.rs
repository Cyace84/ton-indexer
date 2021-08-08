use std::io::Read;

use anyhow::{Context, Result};
use crc::{crc32, Hasher32};
use ton_types::ByteOrderRead;

macro_rules! try_read {
    ($expr:expr) => {
        match $expr {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
    };
}

pub struct ShardStatePacketReader {
    hasher: crc32::Digest,
    offset: usize,
    current_packet: Vec<u8>,
    next_packet: Vec<u8>,
    bytes_to_skip: usize,
}

impl ShardStatePacketReader {
    pub fn new() -> Self {
        Self {
            hasher: crc32::Digest::new(crc32::CASTAGNOLI),
            offset: 0,
            current_packet: Default::default(),
            next_packet: Default::default(),
            bytes_to_skip: 0,
        }
    }

    pub fn read_header(&mut self) -> Result<Option<BocHeader>> {
        const BOC_INDEXED_TAG: u32 = 0x68ff65f3;
        const BOC_INDEXED_CRC32_TAG: u32 = 0xacc3a728;
        const BOC_GENERIC_TAG: u32 = 0xb5ee9c72;

        if self.process_skip() == ReaderAction::Incomplete {
            return Ok(None);
        }

        let mut src = self.begin();

        let magic = try_read!(src.read_be_u32());
        let first_byte = try_read!(src.read_byte());

        let index_included;
        let mut has_crc = false;
        let ref_size;

        match magic {
            BOC_INDEXED_TAG => {
                ref_size = first_byte as usize;
                index_included = true;
            }
            BOC_INDEXED_CRC32_TAG => {
                ref_size = first_byte as usize;
                index_included = true;
                has_crc = true;
            }
            BOC_GENERIC_TAG => {
                index_included = first_byte & 0b1000_0000 != 0;
                has_crc = first_byte & 0b0100_0000 != 0;
                ref_size = (first_byte & 0b0000_0111) as usize;
            }
            _ => {
                return Err(ShardStateParserError::InvalidShardStateHeader).context("Invalid flags")
            }
        }

        if ref_size == 0 || ref_size > 4 {
            return Err(ShardStateParserError::InvalidShardStateHeader)
                .context("Ref size must be in range [1;4]");
        }

        let offset_size = try_read!(src.read_byte()) as usize;
        if offset_size == 0 || offset_size > 8 {
            return Err(ShardStateParserError::InvalidShardStateHeader)
                .context("Offset size must be in range [1;8]");
        }

        let cell_count = try_read!(src.read_be_uint(ref_size));
        let root_count = try_read!(src.read_be_uint(ref_size));
        try_read!(src.read_be_uint(ref_size)); // skip absent

        if root_count != 1 {
            return Err(ShardStateParserError::InvalidShardStateHeader)
                .context("Expected one root cell");
        }
        if root_count > cell_count {
            return Err(ShardStateParserError::InvalidShardStateHeader)
                .context("Root count is greater then cell count");
        }

        try_read!(src.read_be_uint(offset_size)); // skip total cells size

        let root_index = if magic == BOC_GENERIC_TAG {
            Some(try_read!(src.read_be_uint(ref_size)))
        } else {
            None
        };

        src.end();

        if index_included {
            self.set_skip(cell_count * offset_size);
        }

        Ok(Some(BocHeader {
            root_index,
            index_included,
            has_crc,
            ref_size,
            offset_size,
            cell_count,
        }))
    }

    pub fn read_cell(&mut self, ref_size: usize, buffer: &mut [u8]) -> Result<Option<usize>> {
        if self.process_skip() == ReaderAction::Incomplete {
            return Ok(None);
        }

        let mut src = self.begin();

        let d1 = try_read!(src.read_byte());
        let l = d1 >> 5;
        let h = (d1 & 0b0001_0000) != 0;
        let r = (d1 & 0b0000_0111) as usize;
        let absent = r == 0b111 && h;

        buffer[0] = d1;

        let size = if absent {
            let data_size = 32 * ((ton_types::LevelMask::with_mask(l).level() + 1) as usize);
            try_read!(src.read_exact(&mut buffer[1..1 + data_size]));

            log::info!("ABSENT");

            // 1 byte of d1 + fixed data size of absent cell
            1 + data_size
        } else {
            if r > 4 {
                log::error!("CELLS: {}", r);
                return Err(ShardStateParserError::InvalidShardStateCell)
                    .context("Cell must contain at most 4 references");
            }

            let d2 = try_read!(src.read_byte());
            buffer[1] = d2;

            // Skip optional precalculated hashes
            let hash_count = ton_types::LevelMask::with_mask(l).level() as usize + 1;
            if h && !src.skip(hash_count * (32 + 2)) {
                return Ok(None);
            }

            let data_size = ((d2 >> 1) + if d2 & 1 != 0 { 1 } else { 0 }) as usize;
            try_read!(src.read_exact(&mut buffer[2..2 + data_size + r * ref_size]));

            // 2 bytes for d1 and d2 + data size + total references size
            2 + data_size + r * ref_size
        };

        src.end();

        Ok(Some(size))
    }

    pub fn read_crc(&mut self) -> Result<Option<()>> {
        if self.process_skip() == ReaderAction::Incomplete {
            return Ok(None);
        }

        let current_crc = self.hasher.sum32();

        let mut src = self.begin();
        let target_crc = try_read!(src.read_le_u32());
        src.end();

        if current_crc == target_crc {
            Ok(Some(()))
        } else {
            Err(ShardStateParserError::CrcMismatch.into())
        }
    }

    pub fn set_next_packet(&mut self, packet: Vec<u8>) {
        self.next_packet = packet;
    }

    fn begin(&'_ mut self) -> ShardStatePacketReaderTransaction<'_> {
        let offset = self.offset;
        ShardStatePacketReaderTransaction {
            reader: self,
            reading_next_packet: false,
            offset,
        }
    }

    fn set_skip(&mut self, n: usize) {
        self.bytes_to_skip = n;
    }

    fn process_skip(&mut self) -> ReaderAction {
        if self.bytes_to_skip == 0 {
            return ReaderAction::Complete;
        }

        let mut n = std::mem::take(&mut self.bytes_to_skip);

        let remaining = self.current_packet.len() - self.offset;
        match n.cmp(&remaining) {
            std::cmp::Ordering::Less => {
                self.hasher
                    .write(&self.current_packet[self.offset..self.offset + n]);
                self.offset += n;
                ReaderAction::Complete
            }
            std::cmp::Ordering::Equal => {
                self.hasher.write(&self.current_packet[self.offset..]);
                self.offset = 0;
                self.current_packet = std::mem::take(&mut self.next_packet);
                ReaderAction::Complete
            }
            std::cmp::Ordering::Greater => {
                n -= remaining;
                self.hasher.write(&self.current_packet[self.offset..]);
                self.offset = 0;
                self.current_packet = std::mem::take(&mut self.next_packet);

                if n > self.current_packet.len() {
                    n -= self.current_packet.len();
                    self.hasher.write(&self.current_packet);
                    self.current_packet = Vec::new();
                    self.bytes_to_skip = n;
                    ReaderAction::Incomplete
                } else {
                    self.offset = n;
                    self.hasher.write(&self.current_packet[..self.offset]);
                    ReaderAction::Complete
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct BocHeader {
    pub root_index: Option<usize>,
    pub index_included: bool,
    pub has_crc: bool,
    pub ref_size: usize,
    pub offset_size: usize,
    pub cell_count: usize,
}

pub struct RawCell<'a> {
    pub cell_type: ton_types::CellType,
    pub level_mask: u8,
    pub data: &'a [u8],
    pub bit_len: usize,
    pub reference_indices: Vec<u32>,
}

impl<'a> RawCell<'a> {
    pub fn from_stored_data<R>(
        src: &mut R,
        ref_size: usize,
        cell_count: usize,
        cell_index: usize,
        data_buffer: &'a mut [u8],
    ) -> Result<Self>
    where
        R: Read,
    {
        let d1 = src.read_byte()?;
        let l = d1 >> 5;
        let h = (d1 & 0b0001_0000) != 0;
        let s = (d1 & 0b0000_1000) != 0;
        let r = (d1 & 0b0000_0111) as usize;
        let absent = r == 0b111 && h;

        if absent {
            let data_size = 32 * ((ton_types::LevelMask::with_mask(l).level() + 1) as usize);
            let cell_data = &mut data_buffer[0..data_size + 1];
            src.read_exact(&mut cell_data[..data_size])?;
            cell_data[data_size] = 0x80;

            return Ok(RawCell {
                cell_type: ton_types::CellType::Ordinary,
                level_mask: l,
                data: cell_data,
                bit_len: ton_types::find_tag(cell_data), // ?!
                reference_indices: Vec::new(),
            });
        }

        let d2 = src.read_byte()?;
        let data_size = ((d2 >> 1) + if d2 & 1 != 0 { 1 } else { 0 }) as usize;
        let no_completion_tag = d2 & 1 == 0;

        let cell_data = &mut data_buffer[0..data_size + if no_completion_tag { 1 } else { 0 }];
        src.read_exact(&mut cell_data[..data_size])?;

        if no_completion_tag {
            cell_data[data_size] = 0x80;
        }

        let cell_type = if !s {
            ton_types::CellType::Ordinary
        } else {
            ton_types::CellType::from(cell_data[0])
        };

        let mut reference_indices = Vec::with_capacity(r);
        if r > 0 {
            for _ in 0..r {
                let index = src.read_be_uint(ref_size)?;
                if index > cell_count || index <= cell_index {
                    return Err(ShardStateParserError::InvalidShardStateCell)
                        .context("Reference index out of range");
                } else {
                    reference_indices.push(index as u32);
                }
            }
        }

        Ok(RawCell {
            cell_type,
            level_mask: l,
            data: cell_data,
            bit_len: ton_types::find_tag(cell_data),
            reference_indices,
        })
    }
}

#[derive(Copy, Clone, Eq, PartialEq)]
pub enum ReaderAction {
    Incomplete,
    Complete,
}

pub struct ShardStatePacketReaderTransaction<'a> {
    reader: &'a mut ShardStatePacketReader,
    reading_next_packet: bool,
    offset: usize,
}

impl<'a> ShardStatePacketReaderTransaction<'a> {
    pub fn skip(&mut self, mut n: usize) -> bool {
        loop {
            let current_packet = match self.reading_next_packet {
                // Reading non-empty current packet
                false if self.offset < self.reader.current_packet.len() => {
                    &self.reader.current_packet
                }

                // Current packet is empty - retry and switch to next
                false => {
                    self.reading_next_packet = true;
                    self.offset = 0;
                    continue;
                }

                // Reading non-empty next packet
                true if self.offset < self.reader.next_packet.len() => &self.reader.next_packet,

                // Reading next packet which is empty
                true => return false,
            };

            let skipped = std::cmp::min(current_packet.len() - self.offset, n);
            n -= skipped;
            self.offset += skipped;

            if n == 0 {
                return true;
            }
        }
    }

    pub fn end(self) {
        if self.reading_next_packet {
            // Write to the hasher until the end of current packet
            self.reader
                .hasher
                .write(&self.reader.current_packet[self.reader.offset..]);

            // Write to the hasher current bytes
            self.reader
                .hasher
                .write(&self.reader.next_packet[..self.offset]);

            // Replace current packet
            self.reader.current_packet = std::mem::take(&mut self.reader.next_packet);
        } else {
            // Write to the hasher current bytes
            self.reader
                .hasher
                .write(&self.reader.current_packet[self.reader.offset..self.offset]);
        }

        // Bump offset
        self.reader.offset = self.offset;
    }
}

impl<'a> Read for ShardStatePacketReaderTransaction<'a> {
    fn read(&mut self, mut buf: &mut [u8]) -> std::io::Result<usize> {
        let mut result = 0;

        loop {
            let current_packet = match self.reading_next_packet {
                // Reading non-empty current packet
                false if self.offset < self.reader.current_packet.len() => {
                    &self.reader.current_packet
                }

                // Current packet is empty - retry and switch to next
                false => {
                    self.reading_next_packet = true;
                    self.offset = 0;
                    continue;
                }

                // Reading non-empty next packet
                true if self.offset < self.reader.next_packet.len() => &self.reader.next_packet,

                // Reading next packet which is empty
                true => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "packet buffer underflow",
                    ))
                }
            };

            let n = std::cmp::min(current_packet.len() - self.offset, buf.len());
            for i in 0..n {
                buf[i] = current_packet[self.offset + i];
            }

            result += n;
            self.offset += n;

            let tmp = buf;
            buf = &mut tmp[n..];

            if buf.is_empty() {
                return Ok(result);
            }
        }
    }
}

#[derive(thiserror::Error, Debug)]
enum ShardStateParserError {
    #[error("Invalid shard state header")]
    InvalidShardStateHeader,
    #[error("Invalid shard state cell")]
    InvalidShardStateCell,
    #[error("Crc mismatch")]
    CrcMismatch,
}