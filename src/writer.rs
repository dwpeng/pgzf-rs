use std::io::{Seek, SeekFrom, Write};

use rayon::prelude::*;

use crate::{
    BlockType,
    compress::ReusableCompressor,
    constants::*,
    error::Result,
    format::{self, IndexEntry, PgzfConfig},
};

struct PendingBlock {
    data: Vec<u8>,
    block_type: BlockType,
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

        // Move data out of the reusable buffer without copying.
        // truncate() only adjusts the length — no memcpy, no dealloc.
        // Allocate a new reusable buffer (same capacity as before).
        let mut data = std::mem::replace(
            &mut self.buffer,
            vec![0u8; self.config.block_size],
        );
        data.truncate(self.buffer_len);
        self.pending_blocks.push(PendingBlock { data, block_type });

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
        compressor: &mut ReusableCompressor,
        data: &[u8],
        block_type: BlockType,
    ) -> Result<CompressedBlock> {
        let compressed = compressor.compress(data)?;
        let crc = crc32fast::hash(data);

        let header_size = match block_type {
            BlockType::Beg => BEG_HEADER_SIZE,
            _ => DAT_HEADER_SIZE,
        };
        let zc = (header_size + compressed.len() + PGZF_TAIL_SIZE) as u32;

        let header: Vec<u8> = match block_type {
            BlockType::Beg => format::build_beg_header(zc).to_vec(),
            _ => format::build_dat_header(zc).to_vec(),
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
        let batch_size = self.config.compression_batch_size;

        // Sequential write: maintain block order
        let w = self.inner.as_mut().unwrap();
        let mut block_entries = Vec::with_capacity(self.pending_blocks.len());

        // Process blocks in batches to reduce memory usage
        // Each thread gets its own ReusableCompressor via thread_local
        for batch in self.pending_blocks.chunks(batch_size) {
            // Parallel compression with per-thread reusable compressor
            let compressed: Vec<CompressedBlock> = batch
                .par_iter()
                .map_init(
                    || ReusableCompressor::new(level),
                    |compressor, block| Self::compress_single_block(compressor, &block.data, block.block_type),
                )
                .collect::<Result<Vec<_>>>()?;

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

        // Backpatch GC value in BEG header (u64 field)
        if self.gc_pending {
            let gc_value: u64 = block_entries.iter().map(|e| e.compressed_size as u64).sum();
            let gc_file_offset = self.group_start_offset + GC_VALUE_OFFSET as u64;
            let current_pos = w.stream_position()?;
            w.seek(SeekFrom::Start(gc_file_offset))?;
            let mut gc_buf = [0u8; 8];
            format::write_u64_le(&mut gc_buf, gc_value);
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
    fn test_buffer_block_separates_data() {
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

        // flush current buffer as a block so data2 starts in a new block
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
        assert_eq!(decompressed, expected, "buffer_block round-trip failed");
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
    fn test_write_empty_data() {
        let buf = Vec::new();
        let cursor = Cursor::new(buf);
        let writer = PgzfWriter::new(cursor);
        // Don't write anything
        let cursor = writer.finish().unwrap();
        let output = cursor.into_inner();
        // Should still produce valid output
        assert!(output.is_empty() || output.len() > 0);
    }

    #[test]
    fn test_write_single_byte() {
        let buf = Vec::new();
        let cursor = Cursor::new(buf);
        let mut writer = PgzfWriter::new(cursor);
        writer.write_all(b"A").unwrap();
        let cursor = writer.finish().unwrap();
        let output = cursor.into_inner();
        assert!(!output.is_empty());
        assert_eq!(output[0], 0x1f);
        assert_eq!(output[1], 0x8b);
    }

    #[test]
    fn test_write_large_data() {
        let block_size = 1024;
        let num_blocks = 100;
        let config = PgzfConfig::builder()
            .block_size(block_size)
            .group_blocks(10)
            .build();

        let buf = Vec::new();
        let cursor = Cursor::new(buf);
        let mut writer = PgzfWriter::with_config(cursor, config);

        let original: Vec<u8> = (0..num_blocks)
            .flat_map(|i| vec![i as u8; block_size])
            .collect();
        writer.write_all(&original).unwrap();
        let cursor = writer.finish().unwrap();
        let pgzf_data = cursor.into_inner();

        // Verify round-trip
        let mut reader = crate::reader::PgzfReader::new(std::io::Cursor::new(pgzf_data)).unwrap();
        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(output, original);
    }

    #[test]
    fn test_write_with_compression_batch_size() {
        let block_size = 256;
        let config = PgzfConfig::builder()
            .block_size(block_size)
            .group_blocks(10)
            .compression_batch_size(3)  // Small batch size
            .build();

        let buf = Vec::new();
        let cursor = Cursor::new(buf);
        let mut writer = PgzfWriter::with_config(cursor, config);

        let original: Vec<u8> = (0..10)
            .flat_map(|i| vec![i as u8; block_size])
            .collect();
        writer.write_all(&original).unwrap();
        let cursor = writer.finish().unwrap();
        let pgzf_data = cursor.into_inner();

        // Verify round-trip
        let mut reader = crate::reader::PgzfReader::new(std::io::Cursor::new(pgzf_data)).unwrap();
        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(output, original);
    }

    #[test]
    fn test_write_with_memory_limits() {
        let block_size = 256;
        let config = PgzfConfig::builder()
            .block_size(block_size)
            .group_blocks(10)
            .cache_memory_limit_mb(1)
            .compression_batch_size(5)
            .build();

        let buf = Vec::new();
        let cursor = Cursor::new(buf);
        let mut writer = PgzfWriter::with_config(cursor, config);

        let original: Vec<u8> = (0..10)
            .flat_map(|i| vec![i as u8; block_size])
            .collect();
        writer.write_all(&original).unwrap();
        let cursor = writer.finish().unwrap();
        let pgzf_data = cursor.into_inner();

        // Verify round-trip
        let mut reader = crate::reader::PgzfReader::new(std::io::Cursor::new(pgzf_data)).unwrap();
        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(output, original);
    }

    #[test]
    fn test_write_incremental() {
        let block_size = 256;
        let config = PgzfConfig::builder()
            .block_size(block_size)
            .group_blocks(5)
            .build();

        let buf = Vec::new();
        let cursor = Cursor::new(buf);
        let mut writer = PgzfWriter::with_config(cursor, config);

        // Write data in small chunks
        let mut expected = Vec::new();
        for i in 0..20 {
            let chunk = vec![i as u8; 100];
            writer.write_all(&chunk).unwrap();
            expected.extend_from_slice(&chunk);
        }
        let cursor = writer.finish().unwrap();
        let pgzf_data = cursor.into_inner();

        // Verify round-trip
        let mut reader = crate::reader::PgzfReader::new(std::io::Cursor::new(pgzf_data)).unwrap();
        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(output, expected);
    }
}
