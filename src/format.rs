use crate::{
    BlockType,
    constants::*,
    error::{PgzfError, Result},
};

#[derive(Debug, Clone, Copy)]
pub(crate) struct IndexEntry {
    pub(crate) compressed_size: u32,
    pub(crate) uncompressed_size: u32,
}

// --- Byte order helpers ---

#[inline]
pub(crate) fn read_u16_le(buf: &[u8]) -> u16 {
    u16::from_le_bytes([buf[0], buf[1]])
}

#[inline]
pub(crate) fn read_u32_le(buf: &[u8]) -> u32 {
    u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]])
}

#[inline]
pub(crate) fn read_u64_le(buf: &[u8]) -> u64 {
    u64::from_le_bytes([
        buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
    ])
}

#[inline]
pub(crate) fn write_u16_le(buf: &mut [u8], val: u16) {
    buf[..2].copy_from_slice(&val.to_le_bytes());
}

#[inline]
pub(crate) fn write_u32_le(buf: &mut [u8], val: u32) {
    buf[..4].copy_from_slice(&val.to_le_bytes());
}

#[inline]
pub(crate) fn write_u64_le(buf: &mut [u8], val: u64) {
    buf[..8].copy_from_slice(&val.to_le_bytes());
}

// --- Header validation ---

pub(crate) fn validate_gzip_header(buf: &[u8]) -> Result<(u8, u8)> {
    if buf.len() < GZIP_FIXED_HEADER_SIZE {
        return Err(PgzfError::InvalidFormat("header too short".into()));
    }
    if buf[0] != GZIP_ID1 || buf[1] != GZIP_ID2 {
        return Err(PgzfError::InvalidGzipMagic(buf[0], buf[1]));
    }
    if buf[2] != GZIP_CM_DEFLATE {
        return Err(PgzfError::InvalidFormat(format!(
            "unsupported CM: {:#04x}",
            buf[2]
        )));
    }
    Ok((buf[3], buf[8]))
}

pub(crate) fn is_pgzf_member(flg: u8, xfl: u8) -> bool {
    (flg & GZIP_FLG_FEXTRA) != 0 && xfl == PGZF_XFL_MARKER
}

// --- Extra field parsing ---

pub(crate) fn parse_extra_field(
    buf: &[u8],
    is_pgzf: bool,
) -> Result<(u32, Option<u64>, Option<Vec<IndexEntry>>)> {
    if buf.len() < 2 {
        return Err(PgzfError::InvalidFormat("extra field too short".into()));
    }
    let xlen = read_u16_le(buf) as usize;
    if buf.len() < 2 + xlen {
        return Err(PgzfError::InvalidFormat(format!(
            "extra field truncated: need {} bytes, have {}",
            2 + xlen,
            buf.len()
        )));
    }

    let extra = &buf[2..2 + xlen];
    let mut zc: u32 = 0;
    let mut gc: Option<u64> = None;
    let mut ix: Option<Vec<IndexEntry>> = None;

    if !is_pgzf {
        return Ok((zc, gc, ix));
    }

    let mut off = 0;
    while off + 4 <= extra.len() {
        let tag = [extra[off], extra[off + 1]];
        let slen = read_u16_le(&extra[off + 2..]) as usize;
        off += 4;
        if off + slen > extra.len() {
            break;
        }
        let data = &extra[off..off + slen];

        if tag == TAG_ZC && slen >= 4 {
            zc = read_u32_le(data);
        } else if tag == TAG_GC && slen >= 8 {
            gc = Some(read_u64_le(data));
        } else if tag == TAG_IX && slen >= 8 && slen.is_multiple_of(8) {
            let mut entries = Vec::with_capacity(slen / 8);
            let mut i = 0;
            while i + 8 <= slen {
                entries.push(IndexEntry {
                    compressed_size: read_u32_le(&data[i..i + 4]),
                    uncompressed_size: read_u32_le(&data[i + 4..i + 8]),
                });
                i += 8;
            }
            ix = Some(entries);
        }

        off += slen;
    }

    Ok((zc, gc, ix))
}

pub(crate) fn determine_block_type(gc: Option<u64>, ix: &Option<Vec<IndexEntry>>) -> BlockType {
    if ix.is_some() {
        BlockType::Idx
    } else if gc.is_some() {
        BlockType::Beg
    } else {
        BlockType::Dat
    }
}

// --- Header building ---

fn build_gzip_base(buf: &mut [u8]) {
    buf[0] = GZIP_ID1;
    buf[1] = GZIP_ID2;
    buf[2] = GZIP_CM_DEFLATE;
    buf[3] = GZIP_FLG_FEXTRA;
    buf[4..8].fill(0); // MTIME
    buf[8] = PGZF_XFL_MARKER;
    buf[9] = GZIP_OS_UNIX;
}

pub(crate) fn build_beg_header(zc: u32) -> [u8; BEG_HEADER_SIZE] {
    let mut buf = [0u8; BEG_HEADER_SIZE];
    build_gzip_base(&mut buf);
    write_u16_le(&mut buf[10..], 20); // XLEN = ZC(8) + GC(12)
    // ZC tag
    buf[12] = TAG_ZC[0];
    buf[13] = TAG_ZC[1];
    write_u16_le(&mut buf[14..], 4);
    write_u32_le(&mut buf[16..], zc);
    // GC tag
    buf[20] = TAG_GC[0];
    buf[21] = TAG_GC[1];
    write_u16_le(&mut buf[22..], 8);
    write_u64_le(&mut buf[24..], 0); // placeholder, backpatched later
    buf
}

pub(crate) fn build_dat_header(zc: u32) -> [u8; DAT_HEADER_SIZE] {
    let mut buf = [0u8; DAT_HEADER_SIZE];
    build_gzip_base(&mut buf);
    write_u16_le(&mut buf[10..], 8); // XLEN = 4+4
    // ZC tag
    buf[12] = TAG_ZC[0];
    buf[13] = TAG_ZC[1];
    write_u16_le(&mut buf[14..], 4);
    write_u32_le(&mut buf[16..], zc);
    buf
}

pub(crate) fn build_idx_header(zc: u32, entries: &[IndexEntry]) -> Vec<u8> {
    let ix_data_size = entries.len() * 8;
    let xlen = 8 + 4 + ix_data_size; // ZC subfield(8) + IX tag header(4) + IX data
    let total_header = IDX_HEADER_BASE_SIZE + ix_data_size;
    let mut buf = vec![0u8; total_header];
    build_gzip_base(&mut buf);
    write_u16_le(&mut buf[10..], xlen as u16);
    // ZC tag
    buf[12] = TAG_ZC[0];
    buf[13] = TAG_ZC[1];
    write_u16_le(&mut buf[14..], 4);
    write_u32_le(&mut buf[16..], zc);
    // IX tag
    buf[20] = TAG_IX[0];
    buf[21] = TAG_IX[1];
    write_u16_le(&mut buf[22..], ix_data_size as u16);
    // IX data
    let mut off = 24;
    for entry in entries {
        write_u32_le(&mut buf[off..], entry.compressed_size);
        write_u32_le(&mut buf[off + 4..], entry.uncompressed_size);
        off += 8;
    }
    buf
}

// --- Configuration ---

#[derive(Debug, Clone)]
pub struct PgzfConfig {
    pub block_size: usize,
    pub group_blocks: usize,
    pub compression_level: u32,
    /// Maximum memory for block cache in bytes (None = unlimited, default: None)
    pub cache_memory_limit_bytes: Option<usize>,
    /// Maximum memory for readahead buffer in bytes (None = unlimited, default: None)
    pub readahead_memory_limit_bytes: Option<usize>,
    /// Number of blocks to compress in parallel per batch (default: 1000)
    /// Lower values reduce memory usage but may increase compression time
    pub compression_batch_size: usize,
}

impl Default for PgzfConfig {
    fn default() -> Self {
        Self {
            block_size: DEFAULT_BLOCK_SIZE,
            group_blocks: DEFAULT_GROUP_BLOCKS,
            compression_level: DEFAULT_COMPRESSION_LEVEL,
            cache_memory_limit_bytes: None,
            readahead_memory_limit_bytes: None,
            compression_batch_size: 1000,
        }
    }
}

impl PgzfConfig {
    pub fn builder() -> PgzfConfigBuilder {
        PgzfConfigBuilder::default()
    }
}

#[derive(Debug, Default)]
pub struct PgzfConfigBuilder {
    config: PgzfConfig,
}

impl PgzfConfigBuilder {
    pub fn block_size(mut self, size: usize) -> Self {
        self.config.block_size = size;
        self
    }

    pub fn block_size_mb(mut self, mb: usize) -> Self {
        self.config.block_size = mb * (1 << 20);
        self
    }

    pub fn group_blocks(mut self, n: usize) -> Self {
        self.config.group_blocks = n;
        self
    }

    pub fn compression_level(mut self, level: u32) -> Self {
        self.config.compression_level = level.clamp(1, 9);
        self
    }

    /// Set maximum memory for block cache in megabytes.
    pub fn cache_memory_limit_mb(mut self, mb: usize) -> Self {
        self.config.cache_memory_limit_bytes = Some(mb * (1 << 20));
        self
    }

    /// Set maximum memory for block cache in bytes.
    pub fn cache_memory_limit_bytes(mut self, bytes: usize) -> Self {
        self.config.cache_memory_limit_bytes = Some(bytes);
        self
    }

    /// Set maximum memory for readahead buffer in megabytes.
    pub fn readahead_memory_limit_mb(mut self, mb: usize) -> Self {
        self.config.readahead_memory_limit_bytes = Some(mb * (1 << 20));
        self
    }

    /// Set maximum memory for readahead buffer in bytes.
    pub fn readahead_memory_limit_bytes(mut self, bytes: usize) -> Self {
        self.config.readahead_memory_limit_bytes = Some(bytes);
        self
    }

    /// Set the number of blocks to compress in parallel per batch.
    /// Lower values reduce memory usage but may increase compression time.
    pub fn compression_batch_size(mut self, size: usize) -> Self {
        self.config.compression_batch_size = size.max(1);
        self
    }

    pub fn build(self) -> PgzfConfig {
        self.config
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_byte_order_roundtrip() {
        let mut buf = [0u8; 8];
        write_u16_le(&mut buf, 0x1234);
        assert_eq!(read_u16_le(&buf), 0x1234);

        write_u32_le(&mut buf, 0xDEADBEEF);
        assert_eq!(read_u32_le(&buf), 0xDEADBEEF);

        write_u64_le(&mut buf, 0x123456789ABCDEF0);
        assert_eq!(read_u64_le(&buf), 0x123456789ABCDEF0);
    }

    #[test]
    fn test_build_beg_header() {
        let header = build_beg_header(100);
        assert_eq!(header[0], GZIP_ID1);
        assert_eq!(header[1], GZIP_ID2);
        assert_eq!(header[8], PGZF_XFL_MARKER);
        assert_eq!(read_u16_le(&header[10..]), 20); // XLEN
        assert_eq!(header[12], TAG_ZC[0]);
        assert_eq!(header[13], TAG_ZC[1]);
        assert_eq!(read_u32_le(&header[16..]), 100); // ZC
        assert_eq!(header[20], TAG_GC[0]);
        assert_eq!(header[21], TAG_GC[1]);
    }

    #[test]
    fn test_build_dat_header() {
        let header = build_dat_header(200);
        assert_eq!(header.len(), DAT_HEADER_SIZE);
        assert_eq!(read_u16_le(&header[10..]), 8); // XLEN
        assert_eq!(read_u32_le(&header[16..]), 200); // ZC
    }

    #[test]
    fn test_build_idx_header() {
        let entries = vec![
            IndexEntry {
                compressed_size: 100,
                uncompressed_size: 1000,
            },
            IndexEntry {
                compressed_size: 200,
                uncompressed_size: 2000,
            },
        ];
        let header = build_idx_header(500, &entries);
        assert_eq!(header.len(), IDX_HEADER_BASE_SIZE + 16); // 24 + 2*8
        assert_eq!(read_u32_le(&header[16..]), 500); // ZC
        assert_eq!(header[20], TAG_IX[0]);
        assert_eq!(read_u16_le(&header[22..]), 16); // IX SLEN
        assert_eq!(read_u32_le(&header[24..]), 100);
        assert_eq!(read_u32_le(&header[28..]), 1000);
        assert_eq!(read_u32_le(&header[32..]), 200);
        assert_eq!(read_u32_le(&header[36..]), 2000);
    }

    #[test]
    fn test_validate_gzip_header() {
        let mut buf = [0u8; 10];
        buf[0] = GZIP_ID1;
        buf[1] = GZIP_ID2;
        buf[2] = GZIP_CM_DEFLATE;
        buf[3] = GZIP_FLG_FEXTRA;
        buf[8] = PGZF_XFL_MARKER;
        let (flg, xfl) = validate_gzip_header(&buf).unwrap();
        assert!(is_pgzf_member(flg, xfl));
    }

    #[test]
    fn test_parse_extra_field_roundtrip() {
        let entries = vec![
            IndexEntry {
                compressed_size: 100,
                uncompressed_size: 1000,
            },
            IndexEntry {
                compressed_size: 200,
                uncompressed_size: 2000,
            },
        ];
        let header = build_idx_header(500, &entries);
        // The header starts at offset 10 for XLEN
        let xlen = read_u16_le(&header[10..]) as usize;
        let extra = &header[10..10 + 2 + xlen];
        let (zc, gc, ix) = parse_extra_field(extra, true).unwrap();
        assert_eq!(zc, 500);
        assert!(gc.is_none());
        let ix = ix.unwrap();
        assert_eq!(ix.len(), 2);
        assert_eq!(ix[0].compressed_size, 100);
        assert_eq!(ix[1].uncompressed_size, 2000);
    }

    #[test]
    fn test_config_builder() {
        let config = PgzfConfig::builder()
            .block_size_mb(2)
            .group_blocks(4000)
            .compression_level(9)
            .build();
        assert_eq!(config.block_size, 2 * 1024 * 1024);
        assert_eq!(config.group_blocks, 4000);
        assert_eq!(config.compression_level, 9);
    }

    #[test]
    fn test_config_builder_memory_limits() {
        let config = PgzfConfig::builder()
            .block_size_mb(1)
            .cache_memory_limit_mb(64)
            .readahead_memory_limit_mb(16)
            .build();
        assert_eq!(config.cache_memory_limit_bytes, Some(64 * 1024 * 1024));
        assert_eq!(config.readahead_memory_limit_bytes, Some(16 * 1024 * 1024));
    }

    #[test]
    fn test_config_builder_default_memory_limits() {
        let config = PgzfConfig::builder().build();
        assert_eq!(config.cache_memory_limit_bytes, None);
        assert_eq!(config.readahead_memory_limit_bytes, None);
    }
}
