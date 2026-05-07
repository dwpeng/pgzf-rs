use std::io::{Read, Seek, SeekFrom};

use crate::{
    constants::*,
    error::{PgzfError, Result},
    format::{is_pgzf_member, read_u32_le, read_u64_le, validate_gzip_header},
};

#[derive(Debug, Clone)]
pub struct BlockMeta {
    pub group_index: u32,
    pub block_in_group: u32,
    pub compressed_size: u32,
    pub uncompressed_size: u32,
}

#[derive(Debug, Clone)]
pub struct PgzfIndex {
    blocks: Vec<BlockMeta>,
    compressed_offsets: Vec<u64>,
    uncompressed_offsets: Vec<u64>,
    group_count: u64,
}

impl PgzfIndex {
    pub fn build(reader: &mut (impl Read + Seek)) -> Result<Self> {
        let mut blocks = Vec::new();
        let mut compressed_offsets = Vec::new();
        let mut uncompressed_offsets = Vec::new();

        let mut cum_c: u64 = 0;
        let mut cum_u: u64 = 0;
        let mut group_count: u64 = 0;

        reader.seek(SeekFrom::Start(0))?;

        while let Ok(group_start) = reader.stream_position() {
            // Read BEG header
            let mut beg_buf = [0u8; BEG_HEADER_SIZE];
            match reader.read_exact(&mut beg_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            }

            let (flg, xfl) = validate_gzip_header(&beg_buf)?;
            if !is_pgzf_member(flg, xfl) {
                return Err(PgzfError::InvalidFormat("not a PGZF file".into()));
            }

            let _zc = read_u32_le(&beg_buf[ZC_VALUE_OFFSET..ZC_VALUE_OFFSET + 4]);
            let gc = read_u64_le(&beg_buf[GC_VALUE_OFFSET..GC_VALUE_OFFSET + 8]);

            if gc == 0 {
                break;
            }

            // Seek to IDX block
            reader.seek(SeekFrom::Start(group_start + gc))?;

            // Read IDX fixed header
            let mut idx_fixed = [0u8; IDX_HEADER_BASE_SIZE];
            reader.read_exact(&mut idx_fixed)?;

            let (_, idx_xfl) = validate_gzip_header(&idx_fixed)?;
            if idx_xfl != PGZF_XFL_MARKER {
                return Err(PgzfError::InvalidFormat("IDX block not PGZF".into()));
            }

            // Read IX data size from XLEN
            let idx_xlen = crate::format::read_u16_le(&idx_fixed[10..12]) as usize;
            // XLEN = 8 (ZC subfield) + 4 (IX tag header) + ix_data_size
            let ix_data_size = idx_xlen
                .checked_sub(8 + 4)
                .ok_or_else(|| PgzfError::InvalidFormat("invalid IDX XLEN".into()))?;
            let num_entries = ix_data_size / 8;

            // Read IX data
            let mut ix_data = vec![0u8; ix_data_size];
            reader.read_exact(&mut ix_data)?;

            // Parse IX entries
            for i in 0..num_entries {
                let off = i * 8;
                let comp_size = read_u32_le(&ix_data[off..off + 4]);
                let uncomp_size = read_u32_le(&ix_data[off + 4..off + 8]);

                compressed_offsets.push(cum_c);
                uncompressed_offsets.push(cum_u);
                blocks.push(BlockMeta {
                    group_index: group_count as u32,
                    block_in_group: i as u32,
                    compressed_size: comp_size,
                    uncompressed_size: uncomp_size,
                });

                cum_c += comp_size as u64;
                cum_u += uncomp_size as u64;
            }

            // IDX block's own ZC
            let idx_zc = read_u32_le(&idx_fixed[ZC_VALUE_OFFSET..ZC_VALUE_OFFSET + 4]);
            cum_c += idx_zc as u64;

            group_count += 1;

            // Advance to next group
            reader.seek(SeekFrom::Start(group_start + gc + idx_zc as u64))?;
        }

        // Final entry: end-of-file
        compressed_offsets.push(cum_c);
        uncompressed_offsets.push(cum_u);

        Ok(Self {
            blocks,
            compressed_offsets,
            uncompressed_offsets,
            group_count,
        })
    }

    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    pub fn group_count(&self) -> u64 {
        self.group_count
    }

    pub fn total_uncompressed(&self) -> u64 {
        *self.uncompressed_offsets.last().unwrap_or(&0)
    }

    pub fn total_compressed(&self) -> u64 {
        *self.compressed_offsets.last().unwrap_or(&0)
    }

    pub fn compressed_offset(&self, block_index: usize) -> Option<u64> {
        self.compressed_offsets.get(block_index).copied()
    }

    pub fn block_meta(&self, index: usize) -> Option<&BlockMeta> {
        self.blocks.get(index)
    }

    pub fn seek_byte(&self, offset: u64) -> Result<(usize, u64)> {
        if offset > self.total_uncompressed() {
            return Err(PgzfError::SeekBeyondEnd {
                target: offset,
                total: self.total_uncompressed(),
            });
        }
        // Binary search for the block containing this offset
        let block_idx = match self.uncompressed_offsets.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => {
                if i > 0 {
                    i - 1
                } else {
                    0
                }
            }
        };
        let skip = offset.saturating_sub(self.uncompressed_offsets[block_idx]);
        Ok((block_idx, skip))
    }

    pub fn seek_block(&self, index: i64) -> Result<u64> {
        let actual = if index < 0 {
            let abs = (-index) as usize;
            if abs > self.blocks.len() {
                return Err(PgzfError::SeekBeyondEnd {
                    target: index as u64,
                    total: self.blocks.len() as u64,
                });
            }
            self.blocks.len() - abs
        } else {
            index as usize
        };
        if actual >= self.blocks.len() {
            return Err(PgzfError::SeekBeyondEnd {
                target: actual as u64,
                total: self.blocks.len() as u64,
            });
        }
        Ok(self.compressed_offsets[actual])
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use super::*;
    use crate::{format::PgzfConfig, writer::PgzfWriter};

    #[test]
    fn test_index_build_and_seek() {
        let block_size = 256;
        let config = PgzfConfig::builder()
            .block_size(block_size)
            .group_blocks(3)
            .build();

        // Write PGZF
        let buf = Vec::new();
        let cursor = Cursor::new(buf);
        let mut writer = PgzfWriter::with_config(cursor, config);
        let data = vec![0xAB_u8; block_size * 4]; // 4 blocks -> triggers group finalization
        writer.write_all(&data).unwrap();
        let cursor = writer.finish().unwrap();
        let pgzf_data = cursor.into_inner();

        // Build index
        let mut reader = Cursor::new(pgzf_data);
        let index = PgzfIndex::build(&mut reader).unwrap();

        assert_eq!(index.block_count(), 4);
        assert_eq!(index.total_uncompressed(), (block_size * 4) as u64);
        assert!(index.total_compressed() > 0);

        // Seek by byte
        let (block_idx, skip) = index.seek_byte(300).unwrap();
        assert_eq!(block_idx, 1); // 300 / 256 = 1
        assert_eq!(skip, 44); // 300 - 256

        // Seek by block
        let offset = index.seek_block(0).unwrap();
        assert_eq!(offset, 0);

        let offset = index.seek_block(-1).unwrap();
        assert!(offset > 0);
    }
}
