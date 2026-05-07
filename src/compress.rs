use crate::error::{PgzfError, Result};

pub(crate) fn compress_block(data: &[u8], level: u32) -> Result<Vec<u8>> {
    use flate2::{Compress, Compression, FlushCompress};

    let mut compressor = Compress::new(Compression::new(level), false);
    // Upper bound: source size + 0.1% + 12 bytes (zlib worst case), plus some headroom
    let upper_bound = data.len() + (data.len() / 1000) + 64;
    let mut output = vec![0u8; upper_bound];

    let status = compressor
        .compress(data, &mut output, FlushCompress::Finish)
        .map_err(|e| PgzfError::Deflate(e.to_string()))?;

    match status {
        flate2::Status::Ok | flate2::Status::StreamEnd => {}
        flate2::Status::BufError => return Err(PgzfError::Deflate("buffer error".into())),
    }

    let compressed_len = compressor.total_out() as usize;
    output.truncate(compressed_len);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use flate2::{Decompress, FlushDecompress};

    use super::*;

    #[test]
    fn test_compress_decompress_roundtrip() {
        let data = b"Hello, PGZF! This is a test block for compression.";
        let compressed = compress_block(data, 6).unwrap();
        assert!(!compressed.is_empty());

        // Decompress with raw inflate
        let mut decompressor = Decompress::new(false);
        let mut output = vec![0u8; data.len() * 2];
        decompressor
            .decompress(&compressed, &mut output, FlushDecompress::Finish)
            .unwrap();
        assert_eq!(&output[..decompressor.total_out() as usize], data);
    }

    #[test]
    fn test_empty_compress() {
        let compressed = compress_block(&[], 6).unwrap();
        let mut decompressor = Decompress::new(false);
        let mut output = vec![0u8; 16];
        decompressor
            .decompress(&compressed, &mut output, FlushDecompress::Finish)
            .unwrap();
        assert_eq!(decompressor.total_out(), 0);
    }
}
