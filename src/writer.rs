use std::io::{Seek, SeekFrom, Write};

use rayon::prelude::*;

use crate::{
    BlockType,
    constants::*,
    error::Result,
    format::{self, IndexEntry, PgzfConfig},
};

struct PendingBlock {
    data: Vec<u8>,
    block_type: BlockType,
    flag: Option<u32>,
}

struct CompressedBlock {
    header: Vec<u8>,
    compressed: Vec<u8>,
    trailer: [u8; PGZF_TAIL_SIZE],
    zc: u32,
    uncompressed_size: u32,
}

pub struct PgzfWriter<W: Write + Seek> {
    inner: Option<W>,
    config: PgzfConfig,
    buffer: Vec<u8>,
    buffer_len: usize,
    pending_blocks: Vec<PendingBlock>,
    blocks_in_group: usize,
    group_start_offset: u64,
    total_uncompressed: u64,
    total_compressed: u64,
    total_blocks: u64,
    gc_pending: bool,
    block_flag: Option<u32>,
}

impl<W: Write + Seek> PgzfWriter<W> {
    pub fn new(inner: W) -> Self {
        Self::with_config(inner, PgzfConfig::default())
    }

    pub fn with_config(mut inner: W, config: PgzfConfig) -> Self {
        let offset = inner.stream_position().unwrap_or(0);
        let buffer = vec![0u8; config.block_size];
        Self {
            inner: Some(inner),
            config,
            buffer,
            buffer_len: 0,
            pending_blocks: Vec::new(),
            blocks_in_group: 0,
            group_start_offset: offset,
            total_uncompressed: 0,
            total_compressed: 0,
            total_blocks: 0,
            gc_pending: false,
            block_flag: None,
        }
    }

    pub fn bytes_written(&self) -> u64 {
        self.total_uncompressed
    }

    pub fn blocks_written(&self) -> u64 {
        self.total_blocks
    }

    fn buffer_block(&mut self) {
        if self.buffer_len == 0 {
            return;
        }

        let block_type = if self.blocks_in_group == 0 {
            BlockType::Beg
        } else {
            BlockType::Dat
        };

        let data = self.buffer[..self.buffer_len].to_vec();
        self.pending_blocks.push(PendingBlock {
            data,
            block_type,
            flag: self.block_flag,
        });

        self.blocks_in_group += 1;
        self.buffer_len = 0;

        if block_type == BlockType::Beg {
            self.gc_pending = true;
        }

        if self.blocks_in_group >= self.config.group_blocks {
            self.flush_group().expect("failed to flush group");
        }
    }

    fn compress_single_block(
        data: &[u8],
        block_type: BlockType,
        level: u32,
        flag: Option<u32>,
    ) -> Result<CompressedBlock> {
        let compressed = crate::compress::compress_block(data, level)?;
        let crc = crc32fast::hash(data);

        let header_size = match (block_type, flag) {
            (BlockType::Beg, Some(_)) => BEG_FLAG_HEADER_SIZE,
            (BlockType::Beg, None) => BEG_HEADER_SIZE,
            (_, Some(_)) => DAT_FLAG_HEADER_SIZE,
            (_, None) => DAT_HEADER_SIZE,
        };
        let zc = (header_size + compressed.len() + PGZF_TAIL_SIZE) as u32;

        let header: Vec<u8> = match (block_type, flag) {
            (BlockType::Beg, Some(fl)) => format::build_beg_header_with_flag(zc, fl).to_vec(),
            (BlockType::Beg, None) => format::build_beg_header(zc).to_vec(),
            (_, Some(fl)) => format::build_dat_header_with_flag(zc, fl).to_vec(),
            (_, None) => format::build_dat_header(zc).to_vec(),
        };

        let mut trailer = [0u8; PGZF_TAIL_SIZE];
        format::write_u32_le(&mut trailer[0..4], crc);
        format::write_u32_le(&mut trailer[4..8], data.len() as u32);

        Ok(CompressedBlock {
            header,
            compressed,
            trailer,
            zc,
            uncompressed_size: data.len() as u32,
        })
    }

    fn flush_group(&mut self) -> Result<()> {
        if self.pending_blocks.is_empty() {
            return Ok(());
        }

        let level = self.config.compression_level;

        // Parallel compression: each block is compressed independently
        let compressed: Vec<CompressedBlock> = self
            .pending_blocks
            .par_iter()
            .map(|block| {
                Self::compress_single_block(&block.data, block.block_type, level, block.flag)
            })
            .collect::<Result<Vec<_>>>()?;

        // Sequential write: maintain block order
        let w = self.inner.as_mut().unwrap();
        let mut block_entries = Vec::with_capacity(compressed.len());

        for cb in &compressed {
            w.write_all(&cb.header)?;
            w.write_all(&cb.compressed)?;
            w.write_all(&cb.trailer)?;

            block_entries.push(IndexEntry {
                compressed_size: cb.zc,
                uncompressed_size: cb.uncompressed_size,
            });

            self.total_compressed += cb.zc as u64;
            self.total_uncompressed += cb.uncompressed_size as u64;
            self.total_blocks += 1;
        }

        // Write IDX block
        let empty_compressed = crate::compress::compress_block(&[], self.config.compression_level)?;
        let idx_header_size = IDX_HEADER_BASE_SIZE + block_entries.len() * 8;
        let idx_zc = (idx_header_size + empty_compressed.len() + PGZF_TAIL_SIZE) as u32;
        let idx_header = format::build_idx_header(idx_zc, &block_entries);
        w.write_all(&idx_header)?;
        w.write_all(&empty_compressed)?;
        w.write_all(&[0u8; PGZF_TAIL_SIZE])?;

        self.total_compressed += idx_zc as u64;

        // Backpatch GC value in BEG header
        if self.gc_pending {
            let gc_value: u32 = block_entries.iter().map(|e| e.compressed_size).sum();
            let gc_file_offset = self.group_start_offset + GC_VALUE_OFFSET as u64;
            let current_pos = w.stream_position()?;
            w.seek(SeekFrom::Start(gc_file_offset))?;
            let mut gc_buf = [0u8; 4];
            format::write_u32_le(&mut gc_buf, gc_value);
            w.write_all(&gc_buf)?;
            w.seek(SeekFrom::Start(current_pos))?;
            self.gc_pending = false;
        }

        // Reset group state
        self.blocks_in_group = 0;
        self.pending_blocks.clear();
        self.group_start_offset = w.stream_position()?;

        Ok(())
    }

    /// Write data with block-aligned boundaries.
    ///
    /// 1. Flushes any buffered data from prior writes into its own blocks.
    /// 2. Writes the given `data`.
    /// 3. Pads so the written data ends on a block boundary.
    ///
    /// Returns `(start_block, block_count)` — the index of the first block
    /// occupied by this data and the total number of blocks it spans.
    ///
    /// Unlike `finish()`, this method does not force a group flush. Pending
    /// blocks are counted in the return value but remain buffered for later
    /// bulk flushing.
    pub fn write_with_pad(&mut self, data: &[u8]) -> Result<(u64, u64)> {
        // Flush previously buffered data so new data starts in a fresh block
        self.buffer_block();

        // Total blocks so far (flushed + pending) = start of new data
        let start_block = self.total_blocks + self.pending_blocks.len() as u64;

        // Write new data
        self.write_all(data)?;

        // Pad: flush any remaining buffered data as a block
        self.buffer_block();

        let block_count = (self.total_blocks + self.pending_blocks.len() as u64) - start_block;

        Ok((start_block, block_count))
    }

    /// Set a flag value that will be embedded in subsequent blocks.
    /// The flag persists until changed or cleared.
    pub fn set_block_flag(&mut self, flag: u32) {
        self.block_flag = Some(flag);
    }

    /// Clear the block flag; subsequent blocks will have no flag.
    pub fn clear_block_flag(&mut self) {
        self.block_flag = None;
    }

    pub fn finish(mut self) -> Result<W> {
        // Buffer any remaining data
        self.buffer_block();
        // Flush the last group (even if incomplete)
        self.flush_group()?;

        let w = self.inner.take().unwrap();
        Ok(w)
    }
}

impl<W: Write + Seek> Write for PgzfWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut consumed = 0;
        while consumed < buf.len() {
            let space = self.config.block_size - self.buffer_len;
            let take = space.min(buf.len() - consumed);
            self.buffer[self.buffer_len..self.buffer_len + take]
                .copy_from_slice(&buf[consumed..consumed + take]);
            self.buffer_len += take;
            consumed += take;

            if self.buffer_len == self.config.block_size {
                self.buffer_block();
            }
        }
        Ok(consumed)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<W: Write + Seek> Drop for PgzfWriter<W> {
    fn drop(&mut self) {
        if self.inner.is_some() && self.buffer_len > 0 {
            eprintln!("warning: PgzfWriter dropped without calling finish()");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Read, Write};

    use super::*;

    #[test]
    fn test_pad_block_separates_data() {
        let block_size = 256;
        let config = PgzfConfig::builder()
            .block_size(block_size)
            .group_blocks(10)
            .build();

        let buf = Vec::new();
        let cursor = Cursor::new(buf);
        let mut writer = PgzfWriter::with_config(cursor, config);

        // Write a partial block of 0xAA
        let data1 = vec![0xAAu8; 100];
        writer.write_all(&data1).unwrap();

        // pad: flush current buffer as a block
        writer.buffer_block();

        // Write more data of 0xBB
        let data2 = vec![0xBBu8; 200];
        writer.write_all(&data2).unwrap();

        let cursor = writer.finish().unwrap();
        let output = cursor.into_inner();

        // Round-trip: must return exact concatenation
        let mut reader = crate::reader::PgzfReader::new(std::io::Cursor::new(output)).unwrap();
        let mut decompressed = Vec::new();
        reader.read_to_end(&mut decompressed).unwrap();

        let mut expected = Vec::new();
        expected.extend_from_slice(&data1);
        expected.extend_from_slice(&data2);
        assert_eq!(decompressed, expected, "pad_block round-trip failed");
    }

    #[test]
    fn test_write_small_data() {
        let buf = Vec::new();
        let cursor = Cursor::new(buf);
        let mut writer = PgzfWriter::new(cursor);

        let data = b"Hello, PGZF!";
        writer.write_all(data).unwrap();
        let cursor = writer.finish().unwrap();
        let output = cursor.into_inner();

        assert!(output.len() > 10);
        assert_eq!(output[0], 0x1f);
        assert_eq!(output[1], 0x8b);
        assert_eq!(output[8], 0xAA);
    }

    #[test]
    fn test_write_one_full_block() {
        let block_size = 1024;
        let config = PgzfConfig::builder()
            .block_size(block_size)
            .group_blocks(2)
            .build();

        let buf = Vec::new();
        let cursor = Cursor::new(buf);
        let mut writer = PgzfWriter::with_config(cursor, config);

        let data = vec![42u8; block_size];
        writer.write_all(&data).unwrap();
        let cursor = writer.finish().unwrap();
        let output = cursor.into_inner();

        assert!(!output.is_empty());
        assert_eq!(output[0], 0x1f);
        assert_eq!(output[1], 0x8b);
        // 1 block written
        assert!(output.len() > PGZF_TAIL_SIZE);
    }

    #[test]
    fn test_write_multiple_blocks_with_group() {
        let block_size = 512;
        let config = PgzfConfig::builder()
            .block_size(block_size)
            .group_blocks(2)
            .build();

        let buf = Vec::new();
        let cursor = Cursor::new(buf);
        let mut writer = PgzfWriter::with_config(cursor, config);

        let data = vec![7u8; block_size * 3];
        writer.write_all(&data).unwrap();
        let cursor = writer.finish().unwrap();
        let output = cursor.into_inner();

        assert_eq!(output[0], 0x1f);
        assert_eq!(output[1], 0x8b);
        assert_eq!(output[8], 0xAA);
        // Output should have at least header + trailer per block
        assert!(output.len() > 3 * (BEG_HEADER_SIZE + PGZF_TAIL_SIZE));
    }

    #[test]
    fn test_parallel_compression_correctness() {
        // Test that parallel compression produces valid PGZF
        let block_size = 256;
        let config = PgzfConfig::builder()
            .block_size(block_size)
            .group_blocks(4)
            .build();

        let buf = Vec::new();
        let cursor = Cursor::new(buf);
        let mut writer = PgzfWriter::with_config(cursor, config);

        // Write different data per block to ensure ordering is correct
        for i in 0u8..10 {
            let data = vec![i; block_size];
            writer.write_all(&data).unwrap();
        }
        let cursor = writer.finish().unwrap();
        let pgzf_data = cursor.into_inner();

        // Verify round-trip
        let mut reader = crate::reader::PgzfReader::new(std::io::Cursor::new(pgzf_data)).unwrap();
        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();

        let mut expected = Vec::new();
        for i in 0u8..10 {
            expected.extend_from_slice(&vec![i; block_size]);
        }
        assert_eq!(output, expected);
    }

    #[test]
    fn test_write_with_pad_returns_correct_block_index_and_count() {
        let block_size = 256;
        let config = PgzfConfig::builder()
            .block_size(block_size)
            .group_blocks(10)
            .build();

        let buf = Vec::new();
        let cursor = Cursor::new(buf);
        let mut writer = PgzfWriter::with_config(cursor, config);

        // Write first chunk
        let data1 = vec![0xAAu8; 100];
        let (start_block, blocks) = writer.write_with_pad(&data1).unwrap();
        assert_eq!(start_block, 0, "first chunk starts at block 0");
        assert_eq!(blocks, 1, "100 bytes should occupy 1 block");

        // Write second chunk
        let data2 = vec![0xBBu8; 300];
        let (start_block, blocks) = writer.write_with_pad(&data2).unwrap();
        assert_eq!(start_block, 1, "second chunk starts at block 1");
        assert_eq!(blocks, 2, "300 bytes should occupy 2 blocks (256 + 44)");

        // Write third chunk
        let data3 = vec![0xCCu8; 512];
        let (start_block, blocks) = writer.write_with_pad(&data3).unwrap();
        assert_eq!(start_block, 3, "third chunk starts at block 3");
        assert_eq!(blocks, 2, "512 bytes should occupy exactly 2 blocks");

        let cursor = writer.finish().unwrap();
        let output = cursor.into_inner();

        // Round-trip: verify data integrity
        let mut reader = crate::reader::PgzfReader::new(std::io::Cursor::new(output)).unwrap();
        let mut decompressed = Vec::new();
        reader.read_to_end(&mut decompressed).unwrap();

        let mut expected = Vec::new();
        expected.extend_from_slice(&data1);
        expected.extend_from_slice(&data2);
        expected.extend_from_slice(&data3);
        assert_eq!(decompressed, expected);
    }

    #[test]
    fn test_write_with_pad_exact_block_multiple() {
        let block_size = 128;
        let config = PgzfConfig::builder()
            .block_size(block_size)
            .group_blocks(10)
            .build();

        let buf = Vec::new();
        let cursor = Cursor::new(buf);
        let mut writer = PgzfWriter::with_config(cursor, config);

        // Write data that aligns exactly to block boundaries
        let data = vec![0xDDu8; block_size * 3]; // exactly 3 blocks
        let (start_block, blocks) = writer.write_with_pad(&data).unwrap();
        assert_eq!(start_block, 0);
        assert_eq!(blocks, 3);

        let cursor = writer.finish().unwrap();
        let output = cursor.into_inner();

        let mut reader = crate::reader::PgzfReader::new(std::io::Cursor::new(output)).unwrap();
        let mut decompressed = Vec::new();
        reader.read_to_end(&mut decompressed).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_write_with_pad_with_prior_buffered_data() {
        let block_size = 256;
        let config = PgzfConfig::builder()
            .block_size(block_size)
            .group_blocks(10)
            .build();

        let buf = Vec::new();
        let cursor = Cursor::new(buf);
        let mut writer = PgzfWriter::with_config(cursor, config);

        // Write some data without padding (stays in buffer)
        let prior = vec![0xEEu8; 50];
        writer.write_all(&prior).unwrap();

        // Now write_with_pad should flush the prior data as its own block first
        let data = vec![0xFFu8; 100];
        let (start_block, blocks) = writer.write_with_pad(&data).unwrap();
        // Prior data gets flushed into block 0, new data starts at block 1
        assert_eq!(start_block, 1, "new data starts at block 1");
        assert_eq!(blocks, 1, "100 bytes should occupy 1 block");

        let cursor = writer.finish().unwrap();
        let output = cursor.into_inner();

        let mut reader = crate::reader::PgzfReader::new(std::io::Cursor::new(output)).unwrap();
        let mut decompressed = Vec::new();
        reader.read_to_end(&mut decompressed).unwrap();

        let mut expected = Vec::new();
        expected.extend_from_slice(&prior);
        expected.extend_from_slice(&data);
        assert_eq!(decompressed, expected);
    }

    #[test]
    fn test_write_with_pad_and_read_blocks_roundtrip() {
        // write_with_pad 和 read_blocks 的联合测试
        let block_size = 64;
        let config = PgzfConfig::builder()
            .block_size(block_size)
            .group_blocks(100)
            .build();

        let buf = Vec::new();
        let cursor = Cursor::new(buf);
        let mut writer = PgzfWriter::with_config(cursor, config);

        // 写入多个数据段，每段都用 write_with_pad 确保 block 边界对齐
        let chunks: &[&[u8]] = &[
            &[0xAAu8; 30],  // < 1 block
            &[0xBBu8; 150], // > 2 blocks (64+64+22)
            &[0xCCu8; 128], // exactly 2 blocks
            &[0xDDu8; 300], // > 4 blocks
        ];

        let mut metas = Vec::new();
        let mut all_data = Vec::new();
        for chunk in chunks {
            let (start_block, block_count) = writer.write_with_pad(chunk).unwrap();
            metas.push((start_block, block_count));
            all_data.extend_from_slice(chunk);
        }

        let cursor = writer.finish().unwrap();
        let output = cursor.into_inner();

        // 用 read_blocks 回读每一段，验证数据正确
        let mut reader = crate::reader::PgzfReader::new(std::io::Cursor::new(&output)).unwrap();

        for (i, (start_block, block_count)) in metas.iter().enumerate() {
            let read_back = reader
                .read_blocks(*start_block as usize, *block_count as usize)
                .unwrap();
            assert_eq!(
                read_back, chunks[i],
                "chunk {i} data mismatch via read_blocks"
            );
        }

        // 验证完整 round-trip
        let mut full = Vec::new();
        let mut reader2 = crate::reader::PgzfReader::new(std::io::Cursor::new(output)).unwrap();
        reader2.read_to_end(&mut full).unwrap();
        assert_eq!(full, all_data);

        // 验证 block 区间不重叠
        for i in 1..metas.len() {
            let (prev_start, prev_count) = metas[i - 1];
            let (curr_start, _) = metas[i];
            assert_eq!(
                curr_start,
                prev_start + prev_count,
                "chunk {i} should start right after chunk {}",
                i - 1
            );
        }
    }

    #[test]
    fn test_block_flag_roundtrip() {
        let block_size = 256;
        let config = PgzfConfig::builder()
            .block_size(block_size)
            .group_blocks(10)
            .build();

        let buf = Vec::new();
        let cursor = Cursor::new(buf);
        let mut writer = PgzfWriter::with_config(cursor, config);

        // Write data with flag=1, then flag=2, then no flag
        writer.set_block_flag(1);
        writer.write_with_pad(b"hello with flag 1").unwrap();

        writer.set_block_flag(2);
        writer.write_with_pad(b"flag 2 data").unwrap();

        writer.clear_block_flag();
        writer.write_with_pad(b"no flag data").unwrap();

        let cursor = writer.finish().unwrap();
        let output = cursor.into_inner();

        // Read block by block and check flags
        let mut reader = crate::reader::PgzfReader::new(std::io::Cursor::new(&output)).unwrap();

        // Block 0: flag=1, data="hello with flag 1"
        let mut chunk = vec![0u8; 17];
        reader.read_exact(&mut chunk).unwrap();
        assert_eq!(&chunk, b"hello with flag 1");
        assert_eq!(
            reader.current_block_flag(),
            Some(1),
            "first chunk should have flag=1"
        );

        // Block 1: flag=2, data="flag 2 data"
        let mut chunk = vec![0u8; 11];
        reader.read_exact(&mut chunk).unwrap();
        assert_eq!(&chunk, b"flag 2 data");
        assert_eq!(
            reader.current_block_flag(),
            Some(2),
            "second chunk should have flag=2"
        );

        // Block 2: no flag, data="no flag data"
        let mut chunk = vec![0u8; 12];
        reader.read_exact(&mut chunk).unwrap();
        assert_eq!(&chunk, b"no flag data");
        assert_eq!(
            reader.current_block_flag(),
            None,
            "third chunk should have no flag"
        );
    }

    #[test]
    fn test_write_with_pad_gzip_compatible() {
        use flate2::read::MultiGzDecoder;

        let config = PgzfConfig::builder()
            .block_size(256)
            .group_blocks(10)
            .build();

        // write_with_pad 单次
        let buf = Vec::new();
        let cursor = Cursor::new(buf);
        let mut writer = PgzfWriter::with_config(cursor, config.clone());
        writer.write_with_pad(b"hello from write_with_pad").unwrap();
        let output = writer.finish().unwrap().into_inner();

        let mut decoder = MultiGzDecoder::new(Cursor::new(&output));
        let mut result = Vec::new();
        decoder.read_to_end(&mut result).unwrap();
        assert_eq!(
            result, b"hello from write_with_pad",
            "single write_with_pad should be gzip compatible"
        );

        // write_with_pad 多次
        let buf = Vec::new();
        let cursor = Cursor::new(buf);
        let mut writer = PgzfWriter::with_config(cursor, config);
        writer.write_with_pad(b"first chunk ").unwrap();
        writer.write_with_pad(b"second chunk").unwrap();
        let output = writer.finish().unwrap().into_inner();

        let mut decoder = MultiGzDecoder::new(Cursor::new(&output));
        let mut result = Vec::new();
        decoder.read_to_end(&mut result).unwrap();
        assert_eq!(
            result, b"first chunk second chunk",
            "multiple write_with_pad should be gzip compatible"
        );
    }
}
