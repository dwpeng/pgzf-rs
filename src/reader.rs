use std::{
    collections::VecDeque,
    io::{Read, Seek, SeekFrom},
    sync::Arc,
};

use rayon::prelude::*;

use crate::{
    BlockType,
    block_cache::BlockCache,
    constants::*,
    error::{PgzfError, Result},
    format::{is_pgzf_member, read_u32_le, validate_gzip_header},
    index::PgzfIndex,
};

const DEFAULT_READAHEAD: usize = 8;
const DEFAULT_BLOCK_CACHE_CAPACITY: usize = 64;

struct BufferedBlock {
    data: Arc<[u8]>,
}

pub struct RawBlock {
    pub raw: Vec<u8>,
    pub header_size: usize,
    pub block_type: BlockType,
    pub block_index: usize,
}

pub struct PgzfReader<R: Read + Seek> {
    inner: R,
    is_pgzf: bool,
    index: Option<PgzfIndex>,
    current_block: Arc<[u8]>,
    current_pos: usize,
    eof: bool,
    pending_seek: Option<u64>,
    readahead: VecDeque<BufferedBlock>,
    readahead_size: usize,
    next_block_index: usize,
    block_cache: BlockCache,
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
            current_block: Arc::from([]),
            current_pos: 0,
            eof: false,
            pending_seek: None,
            readahead: VecDeque::new(),
            readahead_size: DEFAULT_READAHEAD,
            next_block_index: 0,
            block_cache: BlockCache::new(DEFAULT_BLOCK_CACHE_CAPACITY),
        })
    }

    pub fn with_readahead(mut self, size: usize) -> Self {
        self.readahead_size = size.max(1);
        self
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

    /// Returns the current readahead batch size (number of blocks prefetched
    /// and decompressed in parallel).
    pub fn readahead_size(&self) -> usize {
        self.readahead_size
    }

    /// Set the number of blocks to prefetch and decompress in parallel.
    ///
    /// A larger value increases memory usage but can improve throughput on
    /// multi-core systems. The default is 8.
    ///
    /// Parallel decompression uses the global rayon thread pool. To control the
    /// number of threads, configure rayon before creating a reader:
    ///
    /// ```rust,no_run
    /// rayon::ThreadPoolBuilder::new()
    ///     .num_threads(4)
    ///     .build_global()
    ///     .unwrap();
    /// ```
    pub fn set_readahead_size(&mut self, n: usize) {
        self.readahead_size = n.max(1);
    }

    /// Set the block cache capacity (number of decompressed blocks to retain).
    ///
    /// The cache survives seeks, so repeated access to the same blocks avoids
    /// re-reading and re-decompressing from disk. Setting to 0 disables caching.
    /// The default capacity is 64 blocks.
    pub fn with_block_cache(mut self, capacity: usize) -> Self {
        self.block_cache = BlockCache::new(capacity);
        self
    }

    /// Change the block cache capacity at runtime.
    pub fn set_block_cache_capacity(&mut self, n: usize) {
        self.block_cache = BlockCache::new(n);
    }

    /// Returns the current block cache capacity.
    pub fn block_cache_capacity(&self) -> usize {
        self.block_cache.capacity()
    }

    /// Returns the number of blocks currently cached.
    pub fn block_cache_len(&self) -> usize {
        self.block_cache.len()
    }

    pub fn seek_to_byte(&mut self, offset: u64) -> Result<()> {
        if !self.is_pgzf {
            return Err(PgzfError::IndexNotAvailable);
        }
        self.pending_seek = Some(offset);
        self.next_block_index = 0;
        self.current_block = Arc::from([]);
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
        self.next_block_index = 0;
        self.current_block = Arc::from([]);
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
        self.next_block_index = start_block;

        // Read raw blocks sequentially, skipping IDX blocks
        let mut raw_batch: Vec<(Vec<u8>, usize)> = Vec::with_capacity(actual_count);
        let mut blocks_read = 0;
        while blocks_read < actual_count {
            match self.read_one_raw_block()? {
                Some(RawBlock {
                    raw,
                    header_size,
                    block_type,
                    ..
                }) => {
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
        self.current_block = Arc::from([]);
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

            self.current_block = Arc::from([]);
            self.current_pos = 0;
            self.eof = false;
            self.readahead.clear();

            // Check if target block is already cached
            if let Some(cached) = self.block_cache.get(block_idx) {
                self.current_block = cached;
                self.current_pos = skip as usize;
                // Position file cursor at the next block for subsequent reads
                let next_offset = index
                    .compressed_offset(block_idx + 1)
                    .unwrap_or_else(|| index.total_compressed());
                self.inner.seek(SeekFrom::Start(next_offset))?;
                self.next_block_index = block_idx + 1;
            } else {
                // Cache miss — seek to block and fill readahead normally
                let compressed_offset = index.compressed_offset(block_idx).ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, "block not found")
                })?;
                self.inner.seek(SeekFrom::Start(compressed_offset))?;
                self.next_block_index = block_idx;

                self.fill_readahead()?;

                if let Some(buf) = self.readahead.pop_front() {
                    self.current_block = buf.data;
                    self.current_pos = skip as usize;
                } else {
                    self.eof = true;
                }
            }
        }
        Ok(())
    }

    /// Read one raw PGZF block from the current file position.
    ///
    /// Returns the raw gzip member (header + deflate + trailer) without decompressing.
    /// Useful for low-level inspection or custom processing.
    pub fn read_one_raw_block(&mut self) -> Result<Option<RawBlock>> {
        // Read fixed 10-byte gzip header
        let mut header_buf = [0u8; GZIP_FIXED_HEADER_SIZE];
        match self.inner.read_exact(&mut header_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(PgzfError::Io(e)),
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
            if eoff + slen > extra.len() {
                break;
            }
            if tag == TAG_ZC && slen >= 4 {
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

        let block_index = match block_type {
            BlockType::Idx => self.next_block_index,
            _ => {
                let idx = self.next_block_index;
                self.next_block_index += 1;
                idx
            }
        };

        if zc == 0 {
            return Ok(None);
        }

        // Sanity check: ZC shouldn't exceed 1GB
        if zc > 1 << 30 {
            return Err(PgzfError::InvalidFormat(format!(
                "block size too large: {zc} bytes"
            )));
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

        Ok(Some(RawBlock {
            raw,
            header_size,
            block_type,
            block_index,
        }))
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
    /// Cached blocks are skipped at the I/O level — only non-cached blocks are read from disk.
    fn fill_readahead(&mut self) -> std::io::Result<()> {
        if !self.is_pgzf {
            return Ok(());
        }

        if !self.readahead.is_empty() {
            return Ok(());
        }

        // Extract all metadata from index in a scoped block to release the borrow
        // before any mutable self operations.
        let (total_blocks, start, end, ranges_with_offsets, last_end_offset) = {
            let index = match self.index.as_ref() {
                Some(idx) => idx,
                None => return Ok(()),
            };
            let total_blocks = index.block_count();
            let start = self.next_block_index;
            let batch_limit = self.readahead_size;

            if start >= total_blocks {
                self.eof = true;
                return Ok(());
            }

            let end = total_blocks.min(start + batch_limit);

            // Phase 1: check cache, collect non-cached block indices
            let mut non_cached: Vec<usize> = Vec::new();
            for block_idx in start..end {
                if self.block_cache.get(block_idx).is_none() {
                    non_cached.push(block_idx);
                }
            }

            // Group non-cached blocks into contiguous ranges and pre-compute offsets
            let ranges = Self::group_contiguous(&non_cached);
            let ranges_with_offsets: Vec<(std::ops::Range<usize>, u64)> = ranges
                .iter()
                .map(|r| {
                    let offset = index.compressed_offset(r.start).unwrap_or(0);
                    (r.clone(), offset)
                })
                .collect();

            let last_end = ranges.last().map(|r| r.end).unwrap_or(start);
            let last_end_offset = index
                .compressed_offset(last_end)
                .unwrap_or_else(|| index.total_compressed());

            (total_blocks, start, end, ranges_with_offsets, last_end_offset)
        };
        // `index` borrow is dropped here — safe to mutate self

        let count = end - start;

        // Build results vector: cached blocks filled, non-cached left as None
        let mut results: Vec<Option<Arc<[u8]>>> = Vec::with_capacity(count);
        for block_idx in start..end {
            if let Some(cached) = self.block_cache.get(block_idx) {
                results.push(Some(cached));
            } else {
                results.push(None);
            }
        }

        // Phase 2: read and decompress only non-cached blocks
        let hit_eof = if !ranges_with_offsets.is_empty() {
            let mut all_raw: Vec<(usize, Vec<u8>, usize)> = Vec::new();
            let mut eof = false;

            for (range, offset) in &ranges_with_offsets {
                self.inner.seek(SeekFrom::Start(*offset))?;
                self.next_block_index = range.start;

                let range_len = range.end - range.start;
                let mut blocks_read = 0;

                while blocks_read < range_len {
                    match self.read_one_raw_block() {
                        Ok(Some(RawBlock {
                            raw,
                            header_size,
                            block_type,
                            ..
                        })) => {
                            if block_type == BlockType::Idx {
                                continue;
                            }
                            let block_idx = self.next_block_index - 1;
                            all_raw.push((block_idx, raw, header_size));
                            blocks_read += 1;
                        }
                        Ok(None) => {
                            eof = true;
                            break;
                        }
                        Err(e) => return Err(e.into()),
                    }
                }

                if eof {
                    break;
                }
            }

            // Decompress in parallel
            if !all_raw.is_empty() {
                let decompressed: Vec<(usize, Vec<u8>)> = all_raw
                    .par_iter()
                    .map(|(block_idx, raw, header_size)| {
                        let data = Self::decompress_raw_block(raw, *header_size)?;
                        Ok::<_, std::io::Error>((*block_idx, data))
                    })
                    .collect::<std::io::Result<Vec<_>>>()?;

                for (block_idx, data) in decompressed {
                    let arc_data: Arc<[u8]> = Arc::from(data);
                    self.block_cache.insert(block_idx, Arc::clone(&arc_data));
                    results[block_idx - start] = Some(arc_data);
                }
            }

            // Restore file position for sequential continuation
            self.inner.seek(SeekFrom::Start(last_end_offset))?;
            self.next_block_index = ranges_with_offsets.last().map(|(r, _)| r.end).unwrap_or(start);

            eof
        } else {
            // All cached — advance state
            self.next_block_index = end;
            false
        };

        // Phase 3: assemble readahead from results
        for data in results.into_iter().flatten() {
            if !data.is_empty() {
                self.readahead.push_back(BufferedBlock { data });
            }
        }

        if self.readahead.is_empty() && (hit_eof || end >= total_blocks) {
            self.eof = true;
        }

        Ok(())
    }

    /// Group block indices into contiguous ranges for sequential I/O.
    /// E.g. [0, 1, 2, 5, 6, 9] → [0..3, 5..7, 9..10]
    fn group_contiguous(indices: &[usize]) -> Vec<std::ops::Range<usize>> {
        if indices.is_empty() {
            return Vec::new();
        }
        let mut ranges = Vec::new();
        let mut start = indices[0];
        let mut prev = indices[0];

        for &idx in &indices[1..] {
            if idx == prev + 1 {
                prev = idx;
            } else {
                ranges.push(start..prev + 1);
                start = idx;
                prev = idx;
            }
        }
        ranges.push(start..prev + 1);
        ranges
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
                    self.current_block = Arc::from(data);
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
        self.next_block_index = 0;
        self.current_block = Arc::from([]);
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

    #[test]
    fn test_set_readahead_size() {
        let block_size = 64;
        let config = crate::format::PgzfConfig::builder()
            .block_size(block_size)
            .group_blocks(100)
            .build();

        let buf = Vec::new();
        let cursor = Cursor::new(buf);
        let mut writer = crate::writer::PgzfWriter::with_config(cursor, config);
        writer.write_all(&vec![0xABu8; block_size * 20]).unwrap();
        let cursor = writer.finish().unwrap();
        let pgzf_data = cursor.into_inner();

        let mut reader = PgzfReader::new(Cursor::new(&pgzf_data)).unwrap();
        assert_eq!(reader.readahead_size(), 8);

        reader.set_readahead_size(16);
        assert_eq!(reader.readahead_size(), 16);

        // Verify it still reads correctly
        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(output.len(), block_size * 20);
    }

    #[test]
    fn test_block_cache_builder() {
        let block_size = 64;
        let original = vec![0x42u8; block_size * 3];
        let pgzf_data = create_pgzf_data(&original, block_size);

        let reader = PgzfReader::new(Cursor::new(&pgzf_data))
            .unwrap()
            .with_block_cache(32);
        assert_eq!(reader.block_cache_capacity(), 32);
        assert_eq!(reader.block_cache_len(), 0);
    }

    #[test]
    fn test_block_cache_default_enabled() {
        let block_size = 64;
        let original = vec![0x42u8; block_size * 3];
        let pgzf_data = create_pgzf_data(&original, block_size);

        let reader = PgzfReader::new(Cursor::new(&pgzf_data)).unwrap();
        assert_eq!(reader.block_cache_capacity(), 64);
    }

    #[test]
    fn test_block_cache_populated_after_read() {
        let block_size = 64;
        let num_blocks = 5;
        let original: Vec<u8> = (0..num_blocks)
            .flat_map(|i| vec![i as u8; block_size])
            .collect();
        let pgzf_data = create_pgzf_data(&original, block_size);
        let cursor = Cursor::new(pgzf_data);
        let mut reader = PgzfReader::new(cursor).unwrap();

        // Read all blocks
        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(output, original);

        // Cache should have been populated
        assert!(reader.block_cache_len() > 0);
    }

    #[test]
    fn test_block_cache_survives_seek() {
        let block_size = 64;
        let num_blocks = 10;
        let original: Vec<u8> = (0..num_blocks)
            .flat_map(|i| vec![i as u8; block_size])
            .collect();
        let pgzf_data = create_pgzf_data(&original, block_size);
        let cursor = Cursor::new(pgzf_data);
        let mut reader = PgzfReader::new(cursor).unwrap();

        // Read first few blocks to populate cache
        let mut buf = vec![0u8; block_size * 3];
        reader.read_exact(&mut buf).unwrap();
        assert_eq!(buf, original[..block_size * 3]);
        let cache_len_before = reader.block_cache_len();
        assert!(cache_len_before > 0);

        // Seek to a different position
        reader.seek_to_byte(0).unwrap();

        // Cache should survive the seek
        assert_eq!(reader.block_cache_len(), cache_len_before);

        // Re-read should still work correctly
        let mut buf2 = vec![0u8; block_size * 3];
        reader.read_exact(&mut buf2).unwrap();
        assert_eq!(buf2, original[..block_size * 3]);
    }

    #[test]
    fn test_block_cache_hit_on_reseek() {
        let block_size = 64;
        let num_blocks = 10;
        let original: Vec<u8> = (0..num_blocks)
            .flat_map(|i| vec![i as u8; block_size])
            .collect();
        let pgzf_data = create_pgzf_data(&original, block_size);
        let cursor = Cursor::new(pgzf_data);
        let mut reader = PgzfReader::new(cursor).unwrap();

        // Read all blocks to fill cache
        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(output, original);
        let cache_after_read = reader.block_cache_len();
        assert!(cache_after_read > 0);

        // Seek back to beginning
        reader.seek_to_byte(0).unwrap();

        // Cache should still have entries (not cleared by seek)
        assert_eq!(reader.block_cache_len(), cache_after_read);

        // Read again - should serve from cache
        let mut output2 = Vec::new();
        reader.read_to_end(&mut output2).unwrap();
        assert_eq!(output2, original);
    }

    #[test]
    fn test_block_cache_disable() {
        let block_size = 64;
        let num_blocks = 5;
        let original: Vec<u8> = (0..num_blocks)
            .flat_map(|i| vec![i as u8; block_size])
            .collect();
        let pgzf_data = create_pgzf_data(&original, block_size);
        let cursor = Cursor::new(pgzf_data);
        let mut reader = PgzfReader::new(cursor).unwrap();

        // Disable cache
        reader.set_block_cache_capacity(0);
        assert_eq!(reader.block_cache_capacity(), 0);

        // Should still read correctly
        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(output, original);
        assert_eq!(reader.block_cache_len(), 0);
    }
}
