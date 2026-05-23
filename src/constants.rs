// GZIP magic bytes
pub(crate) const GZIP_ID1: u8 = 0x1f;
pub(crate) const GZIP_ID2: u8 = 0x8b;
pub(crate) const GZIP_CM_DEFLATE: u8 = 0x08;
pub(crate) const GZIP_FLG_FEXTRA: u8 = 0x04;
pub(crate) const GZIP_OS_UNIX: u8 = 0x03;

// PGZF identification
pub(crate) const PGZF_XFL_MARKER: u8 = 0xAA;

// Header sizes
pub(crate) const GZIP_FIXED_HEADER_SIZE: usize = 10;
pub(crate) const BEG_HEADER_SIZE: usize = 32;
pub(crate) const DAT_HEADER_SIZE: usize = 20;
pub(crate) const IDX_HEADER_BASE_SIZE: usize = 24;
pub(crate) const PGZF_TAIL_SIZE: usize = 8;

// Offsets within headers
pub(crate) const ZC_VALUE_OFFSET: usize = 16;
pub(crate) const GC_VALUE_OFFSET: usize = 24;

// Tag identifiers (2-byte ASCII)
pub(crate) const TAG_ZC: [u8; 2] = [0x5A, 0x43];
pub(crate) const TAG_GC: [u8; 2] = [0x47, 0x43];
pub(crate) const TAG_IX: [u8; 2] = [0x49, 0x58];

// Defaults (public for users who want to reference them)
pub const DEFAULT_BLOCK_SIZE: usize = 1 << 20;
pub const DEFAULT_GROUP_BLOCKS: usize = 8000;
pub const DEFAULT_COMPRESSION_LEVEL: u32 = 6;
