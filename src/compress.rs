use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress};

use crate::error::{PgzfError, Result};

/// Reusable compressor that avoids repeated allocation.
/// The internal state is reset between uses via `reset()`, which is cheaper
/// than constructing a new `Compress` each time.
/// The output buffer is also reused across calls to avoid repeated heap allocation.
pub(crate) struct ReusableCompressor {
    inner: Compress,
    output_buf: Vec<u8>,
}

impl ReusableCompressor {
    pub(crate) fn new(level: u32) -> Self {
        Self {
            inner: Compress::new(Compression::new(level), false),
            output_buf: Vec::new(),
        }
    }

    /// Compress `data` into a new Vec, reusing internal state and output buffer.
    pub(crate) fn compress(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        // Reset is cheaper than re-allocating — it keeps the internal Huffman tables.
        self.inner.reset();

        let upper_bound = data.len() + (data.len() / 1000) + 64;
        // Reuse the output buffer if it's large enough; otherwise resize.
        self.output_buf.clear();
        if self.output_buf.capacity() < upper_bound {
            self.output_buf.reserve(upper_bound);
        }
        self.output_buf.resize(upper_bound, 0);

        let status = self
            .inner
            .compress(data, &mut self.output_buf, FlushCompress::Finish)
            .map_err(|e| PgzfError::Deflate(e.to_string()))?;

        match status {
            flate2::Status::Ok | flate2::Status::StreamEnd => {}
            flate2::Status::BufError => return Err(PgzfError::Deflate("buffer error".into())),
        }

        let compressed_len = self.inner.total_out() as usize;
        // Clone the compressed data out. The output_buf retains its capacity
        // for the next call, avoiding repeated allocation of the workspace.
        let result = self.output_buf[..compressed_len].to_vec();
        Ok(result)
    }
}

/// Reusable decompressor that avoids repeated allocation.
pub(crate) struct ReusableDecompressor {
    inner: Decompress,
}

impl ReusableDecompressor {
    pub(crate) fn new() -> Self {
        Self {
            inner: Decompress::new(false),
        }
    }

    /// Decompress `compressed` into `output`, reusing internal state.
    pub(crate) fn decompress(&mut self, compressed: &[u8], output: &mut [u8]) -> Result<usize> {
        self.inner.reset(false);

        self.inner
            .decompress(compressed, output, FlushDecompress::Finish)
            .map_err(|e| PgzfError::Inflate(e.to_string()))?;
        Ok(self.inner.total_out() as usize)
    }
}

/// One-shot compression (for callers that don't batch).
pub(crate) fn compress_block(data: &[u8], level: u32) -> Result<Vec<u8>> {
    let mut compressor = ReusableCompressor::new(level);
    compressor.compress(data)
}

/// One-shot decompression (for callers that don't batch).
#[allow(dead_code)]
pub(crate) fn decompress_block(compressed: &[u8], output: &mut [u8]) -> Result<usize> {
    let mut decompressor = ReusableDecompressor::new();
    decompressor.decompress(compressed, output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compress_decompress_roundtrip() {
        let data = b"Hello, PGZF! This is a test block for compression.";
        let compressed = compress_block(data, 6).unwrap();
        assert!(!compressed.is_empty());

        let mut output = vec![0u8; data.len() * 2];
        let actual_size = decompress_block(&compressed, &mut output).unwrap();
        assert_eq!(&output[..actual_size], data);
    }

    #[test]
    fn test_empty_compress() {
        let compressed = compress_block(&[], 6).unwrap();
        let mut output = vec![0u8; 16];
        let actual_size = decompress_block(&compressed, &mut output).unwrap();
        assert_eq!(actual_size, 0);
    }

    #[test]
    fn test_reusable_compressor() {
        let mut compressor = ReusableCompressor::new(6);

        let data1 = b"First block of data";
        let compressed1 = compressor.compress(data1).unwrap();

        let data2 = b"Second block of data";
        let compressed2 = compressor.compress(data2).unwrap();

        // Both should decompress correctly
        let mut output1 = vec![0u8; 1024];
        let size1 = decompress_block(&compressed1, &mut output1).unwrap();
        assert_eq!(&output1[..size1], data1);

        let mut output2 = vec![0u8; 1024];
        let size2 = decompress_block(&compressed2, &mut output2).unwrap();
        assert_eq!(&output2[..size2], data2);
    }

    #[test]
    fn test_reusable_decompressor() {
        let data = b"Test data for reusable decompressor";
        let compressed = compress_block(data, 6).unwrap();

        let mut decompressor = ReusableDecompressor::new();

        let mut output1 = vec![0u8; 1024];
        let size1 = decompressor.decompress(&compressed, &mut output1).unwrap();
        assert_eq!(&output1[..size1], data);

        // Decompress again with same decompressor
        let mut output2 = vec![0u8; 1024];
        let size2 = decompressor.decompress(&compressed, &mut output2).unwrap();
        assert_eq!(&output2[..size2], data);
    }
}
