//! # pgzf - Parallel GZip Format
//!
//! PGZF is a blocked compression format that extends the standard GZIP format
//! (RFC 1952). It enables parallel compression/decompression and random access
//! by splitting data into independently compressed blocks organized into indexed groups.
//!
//! ## Quick Start
//!
//! ```no_run
//! use pgzf::{PgzfWriter, PgzfReader, PgzfConfig};
//! use std::io::{Write, Read, Cursor};
//!
//! // Compress
//! let config = PgzfConfig::builder()
//!     .block_size_mb(1)
//!     .group_blocks(8000)
//!     .compression_level(6)
//!     .build();
//!
//! let mut writer = PgzfWriter::with_config(Cursor::new(Vec::new()), config);
//! writer.write_all(b"Hello, PGZF!").unwrap();
//! let cursor = writer.finish().unwrap();
//! let compressed = cursor.into_inner();
//!
//! // Decompress
//! let mut reader = PgzfReader::new(Cursor::new(compressed)).unwrap();
//! let mut output = String::new();
//! reader.read_to_string(&mut output).unwrap();
//! assert_eq!(output, "Hello, PGZF!");
//! ```
//!
//! ## Random Access
//!
//! PGZF supports seeking by byte offset or block index:
//!
//! ```no_run
//! use pgzf::PgzfReader;
//! use std::io::{Read, Seek, SeekFrom, Cursor};
//!
//! # let pgzf_data: Vec<u8> = Vec::new();
//! let mut reader = PgzfReader::new(Cursor::new(pgzf_data)).unwrap();
//!
//! // Seek to byte offset
//! reader.seek_to_byte(1000).unwrap();
//!
//! // Seek to block index
//! reader.seek_to_block(5).unwrap();
//!
//! // Standard Seek trait
//! reader.seek(SeekFrom::Start(500)).unwrap();
//! ```

mod compress;
mod constants;
mod decompress;
mod error;
mod format;
pub mod index;
pub mod reader;
pub mod writer;

// Re-export constants that users might need
pub use constants::{DEFAULT_BLOCK_SIZE, DEFAULT_COMPRESSION_LEVEL, DEFAULT_GROUP_BLOCKS};
pub use error::{PgzfError, Result};
pub use format::{PgzfConfig, PgzfConfigBuilder};
pub use index::{BlockMeta, PgzfIndex};
pub use reader::{PgzfReader, RawBlock};
pub use writer::PgzfWriter;

/// Block type within a PGZF group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockType {
    /// First block of a group - carries group metadata (GC tag).
    Beg,
    /// Regular data block within a group.
    Dat,
    /// Index block at the end of a group - carries block index (IX tag), no user data.
    Idx,
}
