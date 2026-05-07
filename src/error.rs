use std::fmt;

#[derive(Debug)]
pub enum PgzfError {
    Io(std::io::Error),
    Deflate(String),
    Inflate(String),
    InvalidFormat(String),
    InvalidGzipMagic(u8, u8),
    CrcMismatch { expected: u32, computed: u32 },
    IndexNotAvailable,
    SeekBeyondEnd { target: u64, total: u64 },
}

impl fmt::Display for PgzfError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Deflate(e) => write!(f, "deflate error: {e}"),
            Self::Inflate(e) => write!(f, "inflate error: {e}"),
            Self::InvalidFormat(e) => write!(f, "invalid PGZF format: {e}"),
            Self::InvalidGzipMagic(a, b) => {
                write!(
                    f,
                    "invalid gzip header: expected magic 1f 8b, got {a:#04x} {b:#04x}"
                )
            }
            Self::CrcMismatch { expected, computed } => {
                write!(
                    f,
                    "CRC32 mismatch: expected {expected:#010x}, computed {computed:#010x}"
                )
            }
            Self::IndexNotAvailable => {
                write!(
                    f,
                    "index not available: file is not PGZF or index not yet built"
                )
            }
            Self::SeekBeyondEnd { target, total } => {
                write!(f, "seek target {target} beyond end of data {total}")
            }
        }
    }
}

impl std::error::Error for PgzfError {}

impl From<std::io::Error> for PgzfError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<PgzfError> for std::io::Error {
    fn from(e: PgzfError) -> Self {
        match e {
            PgzfError::Io(io) => io,
            other => std::io::Error::other(other),
        }
    }
}

pub type Result<T> = std::result::Result<T, PgzfError>;
