use std::{
    collections::VecDeque,
    io::{Read, Seek, SeekFrom},
};

use rayon::prelude::*;

use crate::{
    BlockType,
    constants::*,
    error::{PgzfError, Result},
    format::{is_pgzf_member, read_u32_le, validate_gzip_header},
    index::PgzfIndex,
};

const DEFAULT_READAHEAD: usize = 8;

struct BufferedBlock {
    data: Vec<u8>,
}

/// (raw_bytes, header_size, compressed_size, block_type)
type RawBlock = (Vec<u8>, usize, u32, BlockType);

pub struct PgzfReader<R: Read + Seek> {
    inner: R,
    is_pgzf: bool,
    index: Option<PgzfIndex>,
    current_block: Vec<u8>,
    current_pos: usize,
    eof: bool,
    pending_seek: Option<u64>,
    readahead: VecDeque<BufferedBlock>,
    readahead_size: usize,
}

impl<R: Read + Seek> PgzfReader<R> {
    pub fn new(mut inner: R) -> Result<Self> {
        inner.seek(SeekFrom::Start(0))?;
        let mut header = [0u8; GZIP_FIXED_HEADER_SIZE];
        inner.read_exact(&mut header)?;
        let (flg, xfl) = validate_gzip_header(&header)?;
        let is_pgzf = is_pgzf_member(flg, xfl);

        let index = if is_pgzf {
            inner.seek(SeekFrom::Start(0))?;
            Some(PgzfIndex::build(&mut inner)?)
        } else {
            None
        };

        inner.seek(SeekFrom::Start(0))?;

        Ok(Self {
            inner,
            is_pgzf,
            index,
            current_block: Vec::new(),
            current_pos: 0,
            eof: false,
            pending_seek: None,
            readahead: VecDeque::new(),
            readahead_size: DEFAULT_READAHEAD,
        })
    }

    pub fn is_pgzf(&self) -> bool {
        self.is_pgzf
    }

    pub fn index(&self) -> Option<&PgzfIndex> {
        self.index.as_ref()
    }

    pub fn total_uncompressed_size(&self) -> Option<u64> {
        self.index.as_ref().map(|i| i.total_uncompressed())
    }

    pub fn total_compressed_size(&self) -> Option<u64> {
        self.index.as_ref().map(|i| i.total_compressed())
    }

    pub fn block_count(&self) -> Option<usize> {
        self.index.as_ref().map(|i| i.block_count())
    }

    pub fn seek_to_byte(&mut self, offset: u64) -> Result<()> {
        if !self.is_pgzf {
            return Err(PgzfError::IndexNotAvailable);
        }
        self.pending_seek = Some(offset);
        self.current_block.clear();
        self.current_pos = 0;
        self.readahead.clear();
        Ok(())
    }

    pub fn seek_to_block(&mut self, block_index: i64) -> Result<()> {
        if !self.is_pgzf {
            return Err(PgzfError::IndexNotAvailable);
        }
        let index = self.index.as_ref().unwrap();
        let compressed_offset = index.seek_block(block_index)?;
        self.inner.seek(SeekFrom::Start(compressed_offset))?;
        self.current_block.clear();
        self.current_pos = 0;
        self.eof = false;
        self.pending_seek = None;
        self.readahead.clear();
        Ok(())
    }

    /// Read a contiguous range of blocks and return their decompressed data.
    ///
    /// This seeks to the given `start_block`, reads `count` consecutive data blocks
    /// (skipping index blocks), and decompresses them in parallel.
    ///
    /// After this call, the reader's position is at the end of the last read block;
    /// subsequent reads will continue from there.
    pub fn read_blocks(&mut self, start_block: usize, count: usize) -> Result<Vec<u8>> {
        if !self.is_pgzf {
            return Err(PgzfError::IndexNotAvailable);
        }

        let index = self.index.as_ref().unwrap();
        let total_blocks = index.block_count();

        if start_block >= total_blocks {
            return Err(PgzfError::SeekBeyondEnd {
                target: start_block as u64,
                total: total_blocks as u64,
            });
        }

        let end_block = total_blocks.min(start_block + count);
        let actual_count = end_block - start_block;

        if actual_count == 0 {
            return Ok(Vec::new());
        }

        // Seek to start block's compressed offset
        let offset = index.compressed_offset(start_block).unwrap();
        self.inner.seek(SeekFrom::Start(offset))?;

        // Read raw blocks sequentially, skipping IDX blocks
        let mut raw_batch: Vec<(Vec<u8>, usize)> = Vec::with_capacity(actual_count);
        let mut blocks_read = 0;
        while blocks_read < actual_count {
            match self.read_one_raw_block()? {
                Some((raw, header_size, _zc, block_type)) => {
                    if block_type == BlockType::Idx {
                        continue;
                    }
                    raw_batch.push((raw, header_size));
                    blocks_read += 1;
                }
                None => break,
            }
        }

        // Decompress in parallel
        let decompressed = raw_batch
            .par_iter()
            .map(|(raw, header_size)| -> Result<Vec<u8>> {
                let data = Self::decompress_raw_block(raw, *header_size)?;
                Ok(data)
            })
            .collect::<Result<Vec<_>>>()?;

        // Concatenate
        let total_size: usize = decompressed.iter().map(|d| d.len()).sum();
        let mut result = Vec::with_capacity(total_size);
        for data in &decompressed {
            result.extend_from_slice(data);
        }

        // Reset reader state — file position has moved
        self.current_block.clear();
        self.current_pos = 0;
        self.eof = false;
        self.pending_seek = None;
        self.readahead.clear();

        Ok(result)
    }

    fn execute_pending_seek(&mut self) -> std::io::Result<()> {
        if let Some(target_byte) = self.pending_seek.take() {
            let index = self.index.as_ref().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::Unsupported, "not a PGZF file")
            })?;
            let (block_idx, skip) = index
                .seek_byte(target_byte)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

            let compressed_offset = index.compressed_offset(block_idx).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "block not found")
            })?;

            self.inner.seek(SeekFrom::Start(compressed_offset))?;
            self.current_block.clear();
            self.current_pos = 0;
            self.eof = false;
            self.readahead.clear();

            self.fill_readahead()?;

            if let Some(buf) = self.readahead.pop_front() {
                self.current_block = buf.data;
                self.current_pos = skip as usize;
            } else {
                self.eof = true;
            }
        }
        Ok(())
    }

    /// Read one raw PGZF block from the current file position.
    /// Returns (raw_bytes, header_size, zc, block_type) or None at EOF.
    fn read_one_raw_block(&mut self) -> std::io::Result<Option<RawBlock>> {
        // Read fixed 10-byte gzip header
        let mut header_buf = [0u8; GZIP_FIXED_HEADER_SIZE];
        match self.inner.read_exact(&mut header_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }

        // Read XLEN
        let mut xlen_buf = [0u8; 2];
        self.inner.read_exact(&mut xlen_buf)?;
        let xlen = u16::from_le_bytes(xlen_buf) as usize;

        // Read extra field
        let mut extra = vec![0u8; xlen];
        self.inner.read_exact(&mut extra)?;

        // Parse tags to get ZC and determine block type
        let mut zc: u32 = 0;
        let mut has_gc = false;
        let mut has_ix = false;
        let mut eoff = 0;
        while eoff + 4 <= extra.len() {
            let tag = [extra[eoff], extra[eoff + 1]];
            let slen = u16::from_le_bytes([extra[eoff + 2], extra[eoff + 3]]) as usize;
            eoff += 4;
            if tag == TAG_ZC && slen >= 4 && eoff + 4 <= extra.len() {
                zc = read_u32_le(&extra[eoff..eoff + 4]);
            } else if tag == TAG_GC {
                has_gc = true;
            } else if tag == TAG_IX {
                has_ix = true;
            }
            eoff += slen;
        }

        let block_type = if has_ix {
            BlockType::Idx
        } else if has_gc {
            BlockType::Beg
        } else {
            BlockType::Dat
        };

        if zc == 0 {
            return Ok(None);
        }

        // Sanity check: ZC shouldn't exceed 1GB
        if zc > 1 << 30 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("block size too large: {zc} bytes"),
            ));
        }

        let header_size = GZIP_FIXED_HEADER_SIZE + 2 + xlen;
        let remaining = (zc as usize).saturating_sub(header_size);
        let mut rest = vec![0u8; remaining];
        self.inner.read_exact(&mut rest)?;

        // Build full raw block
        let mut raw = Vec::with_capacity(zc as usize);
        raw.extend_from_slice(&header_buf);
        raw.extend_from_slice(&xlen_buf);
        raw.extend_from_slice(&extra);
        raw.extend_from_slice(&rest);

        // File position is now at start of next block (start_pos + zc)
        // No seek needed - sequential read

        Ok(Some((raw, header_size, zc, block_type)))
    }

    /// Decompress a raw PGZF block in memory (for parallel use).
    fn decompress_raw_block(raw: &[u8], header_size: usize) -> std::io::Result<Vec<u8>> {
        // Parse trailer to get expected size
        let zc = raw.len();
        if zc < PGZF_TAIL_SIZE + header_size {
            return Ok(Vec::new());
        }
        let trailer_off = zc - PGZF_TAIL_SIZE;
        let expected_size = u32::from_le_bytes([
            raw[trailer_off + 4],
            raw[trailer_off + 5],
            raw[trailer_off + 6],
            raw[trailer_off + 7],
        ]) as usize;

        if expected_size == 0 {
            return Ok(Vec::new());
        }

        let deflate_data = &raw[header_size..trailer_off];
        let mut output = vec![0u8; expected_size];
        let actual_size = crate::decompress::decompress_block(deflate_data, &mut output)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        output.truncate(actual_size);

        // Verify CRC
        let expected_crc = u32::from_le_bytes([
            raw[trailer_off],
            raw[trailer_off + 1],
            raw[trailer_off + 2],
            raw[trailer_off + 3],
        ]);
        let actual_crc = crc32fast::hash(&output);
        if actual_crc != expected_crc {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "CRC32 mismatch: expected {expected_crc:#010x}, computed {actual_crc:#010x}"
                ),
            ));
        }

        Ok(output)
    }

    /// Fill the readahead buffer by reading sequential raw blocks then decompressing in parallel.
    fn fill_readahead(&mut self) -> std::io::Result<()> {
        if !self.is_pgzf {
            return Ok(());
        }

        // Don't refill if readahead still has data
        if !self.readahead.is_empty() {
            return Ok(());
        }

        // Read raw blocks sequentially from current file position
        let mut raw_batch: Vec<(Vec<u8>, usize)> = Vec::new();
        let mut data_blocks_read = 0;
        let batch_limit = self.readahead_size;
        let mut hit_eof = false;

        while data_blocks_read < batch_limit {
            match self.read_one_raw_block() {
                Ok(Some((raw, header_size, _zc, block_type))) => {
                    if block_type == BlockType::Idx {
                        continue;
                    }
                    raw_batch.push((raw, header_size));
                    data_blocks_read += 1;
                }
                Ok(None) => {
                    hit_eof = true;
                    break;
                }
                Err(e) => return Err(e),
            }
        }

        if raw_batch.is_empty() {
            self.eof = true;
            return Ok(());
        }

        // Decompress all blocks in parallel
        let decompressed: Vec<BufferedBlock> = raw_batch
            .par_iter()
            .map(|(raw, header_size)| {
                let data = Self::decompress_raw_block(raw, *header_size)?;
                Ok::<_, std::io::Error>(BufferedBlock { data })
            })
            .collect::<std::io::Result<Vec<_>>>()?;

        for block in decompressed {
            if !block.data.is_empty() {
                self.readahead.push_back(block);
            }
        }

        // Only set EOF if we hit end of file AND have no buffered data
        if hit_eof && self.readahead.is_empty() {
            self.eof = true;
        }

        Ok(())
    }

    fn advance_to_next_block_sequential(&mut self) -> std::io::Result<()> {
        loop {
            match crate::decompress::read_pgzf_block(&mut self.inner) {
                Ok(Some((data, block_type, _zc))) => {
                    if block_type == BlockType::Idx {
                        continue;
                    }
                    if data.is_empty() {
                        self.eof = true;
                        return Ok(());
                    }
                    self.current_block = data;
                    self.current_pos = 0;
                    return Ok(());
                }
                Ok(None) => {
                    self.eof = true;
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

impl<R: Read + Seek> Read for PgzfReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        self.execute_pending_seek()?;

        // Serve from current block
        if self.current_pos < self.current_block.len() {
            let available = &self.current_block[self.current_pos..];
            let take = available.len().min(buf.len());
            buf[..take].copy_from_slice(&available[..take]);
            self.current_pos += take;
            return Ok(take);
        }

        if self.eof {
            return Ok(0);
        }

        if self.is_pgzf {
            // Serve from readahead buffer
            if let Some(next) = self.readahead.pop_front() {
                self.current_block = next.data;
                self.current_pos = 0;
                let take = self.current_block.len().min(buf.len());
                buf[..take].copy_from_slice(&self.current_block[..take]);
                self.current_pos = take;
                return Ok(take);
            }

            // Readahead empty, fill it
            self.fill_readahead()?;

            if let Some(next) = self.readahead.pop_front() {
                self.current_block = next.data;
                self.current_pos = 0;
                let take = self.current_block.len().min(buf.len());
                buf[..take].copy_from_slice(&self.current_block[..take]);
                self.current_pos = take;
                return Ok(take);
            }

            self.eof = true;
            Ok(0)
        } else {
            // Sequential for plain gzip
            self.advance_to_next_block_sequential()?;

            if self.current_block.is_empty() || self.eof {
                return Ok(0);
            }

            let take = self.current_block.len().min(buf.len());
            buf[..take].copy_from_slice(&self.current_block[..take]);
            self.current_pos = take;
            Ok(take)
        }
    }
}

impl<R: Read + Seek> Seek for PgzfReader<R> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let total = self
            .index
            .as_ref()
            .map(|i| i.total_uncompressed())
            .unwrap_or(0);

        let target = match pos {
            SeekFrom::Start(n) => n,
            SeekFrom::End(n) => {
                if !self.is_pgzf {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Unsupported,
                        "seek from end requires PGZF index",
                    ));
                }
                (total as i64 + n) as u64
            }
            SeekFrom::Current(n) => {
                let current = self.current_pos as u64;
                (current as i64 + n) as u64
            }
        };

        self.pending_seek = Some(target);
        self.current_block.clear();
        self.current_pos = 0;
        self.readahead.clear();
        Ok(target)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use super::*;
    use crate::{format::PgzfConfig, writer::PgzfWriter};

    fn create_pgzf_data(data: &[u8], block_size: usize) -> Vec<u8> {
        let config = PgzfConfig::builder()
            .block_size(block_size)
            .group_blocks(8000)
            .build();
        let buf = Vec::new();
        let cursor = Cursor::new(buf);
        let mut writer = PgzfWriter::with_config(cursor, config);
        writer.write_all(data).unwrap();
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn test_read_sequential() {
        let original = b"Hello, PGZF reader test data!";
        let pgzf_data = create_pgzf_data(original, 1024);
        let cursor = Cursor::new(pgzf_data);
        let mut reader = PgzfReader::new(cursor).unwrap();
        assert!(reader.is_pgzf());
        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(output, original);
    }

    #[test]
    fn test_read_multiblock() {
        let block_size = 64;
        let original = vec![0x42_u8; block_size * 3];
        let pgzf_data = create_pgzf_data(&original, block_size);
        let cursor = Cursor::new(pgzf_data);
        let mut reader = PgzfReader::new(cursor).unwrap();
        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(output, original);
    }

    #[test]
    fn test_seek_and_read() {
        let block_size = 64;
        let original: Vec<u8> = (0..256).map(|i| i as u8).collect();
        let pgzf_data = create_pgzf_data(&original, block_size);
        let cursor = Cursor::new(pgzf_data);
        let mut reader = PgzfReader::new(cursor).unwrap();
        reader.seek_to_byte(100).unwrap();
        let mut output = vec![0u8; 10];
        let n = reader.read(&mut output).unwrap();
        assert_eq!(n, 10);
        assert_eq!(output, &original[100..110]);
    }

    #[test]
    fn test_parallel_read_ordering() {
        let block_size = 32;
        let num_blocks = 20;
        let original: Vec<u8> = (0..num_blocks)
            .flat_map(|i| vec![i as u8; block_size])
            .collect();
        let pgzf_data = create_pgzf_data(&original, block_size);
        let cursor = Cursor::new(pgzf_data);
        let mut reader = PgzfReader::new(cursor).unwrap();
        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(output, original);
    }

    #[test]
    fn test_parallel_seek_and_read() {
        let block_size = 64;
        let num_blocks = 10;
        let original: Vec<u8> = (0..num_blocks)
            .flat_map(|i| vec![i as u8; block_size])
            .collect();
        let pgzf_data = create_pgzf_data(&original, block_size);
        let cursor = Cursor::new(pgzf_data);
        let mut reader = PgzfReader::new(cursor).unwrap();
        let offset = block_size * 5 + 10;
        reader.seek_to_byte(offset as u64).unwrap();
        let mut output = vec![0u8; 20];
        let n = reader.read(&mut output).unwrap();
        assert_eq!(n, 20);
        assert_eq!(output, &original[offset..offset + 20]);
    }

    #[test]
    fn test_read_blocks_range() {
        let block_size = 64;
        let num_blocks = 10;
        let original: Vec<u8> = (0..num_blocks)
            .flat_map(|i| vec![i as u8; block_size])
            .collect();
        let pgzf_data = create_pgzf_data(&original, block_size);
        let cursor = Cursor::new(pgzf_data);
        let mut reader = PgzfReader::new(cursor).unwrap();

        // Read blocks 2-5 (4 blocks: indices 2, 3, 4, 5)
        let data = reader.read_blocks(2, 4).unwrap();
        let expected: Vec<u8> = (2..6).flat_map(|i| vec![i as u8; block_size]).collect();
        assert_eq!(data, expected);
    }

    #[test]
    fn test_read_blocks_all() {
        let block_size = 64;
        let num_blocks = 5;
        let original: Vec<u8> = (0..num_blocks)
            .flat_map(|i| vec![i as u8; block_size])
            .collect();
        let pgzf_data = create_pgzf_data(&original, block_size);
        let cursor = Cursor::new(pgzf_data);
        let mut reader = PgzfReader::new(cursor).unwrap();

        let data = reader.read_blocks(0, num_blocks).unwrap();
        assert_eq!(data, original);
    }

    #[test]
    fn test_read_blocks_count_truncated() {
        let block_size = 64;
        let num_blocks = 3;
        let original: Vec<u8> = (0..num_blocks)
            .flat_map(|i| vec![i as u8; block_size])
            .collect();
        let pgzf_data = create_pgzf_data(&original, block_size);
        let cursor = Cursor::new(pgzf_data);
        let mut reader = PgzfReader::new(cursor).unwrap();

        // Request more blocks than available
        let data = reader.read_blocks(1, 10).unwrap();
        let expected: Vec<u8> = (1..3).flat_map(|i| vec![i as u8; block_size]).collect();
        assert_eq!(data, expected);
    }

    #[test]
    fn test_read_blocks_subsequent_read() {
        // After read_blocks, subsequent reads should continue from where we left off
        let block_size = 64;
        let num_blocks = 8;
        let original: Vec<u8> = (0..num_blocks)
            .flat_map(|i| vec![i as u8; block_size])
            .collect();
        let pgzf_data = create_pgzf_data(&original, block_size);
        let cursor = Cursor::new(pgzf_data);
        let mut reader = PgzfReader::new(cursor).unwrap();

        // Read first 3 blocks
        let data = reader.read_blocks(0, 3).unwrap();
        assert_eq!(data.len(), block_size * 3);

        // Read remaining via normal Read trait
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, original[block_size * 3..]);
    }

    #[test]
    fn test_read_blocks_zero_count() {
        let block_size = 64;
        let original = vec![0x42u8; block_size * 3];
        let pgzf_data = create_pgzf_data(&original, block_size);
        let cursor = Cursor::new(pgzf_data);
        let mut reader = PgzfReader::new(cursor).unwrap();

        let data = reader.read_blocks(0, 0).unwrap();
        assert!(data.is_empty());
    }

    #[test]
    fn test_read_blocks_out_of_range() {
        let block_size = 64;
        let original = vec![0x42u8; block_size * 2];
        let pgzf_data = create_pgzf_data(&original, block_size);
        let cursor = Cursor::new(pgzf_data);
        let mut reader = PgzfReader::new(cursor).unwrap();

        let result = reader.read_blocks(10, 1);
        assert!(result.is_err());
    }
}
