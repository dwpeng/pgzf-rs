use std::io::{Read, Seek};

use crate::{
    BlockType,
    constants::*,
    error::{PgzfError, Result},
    format::{
        determine_block_type, is_pgzf_member, parse_extra_field, read_u32_le, validate_gzip_header,
    },
};

pub(crate) fn decompress_block(compressed: &[u8], output: &mut [u8]) -> Result<usize> {
    use flate2::{Decompress, FlushDecompress};

    let mut decompressor = Decompress::new(false);
    decompressor
        .decompress(compressed, output, FlushDecompress::Finish)
        .map_err(|e| PgzfError::Inflate(e.to_string()))?;
    Ok(decompressor.total_out() as usize)
}

pub(crate) fn read_pgzf_block<R: Read + Seek>(
    reader: &mut R,
) -> Result<Option<(Vec<u8>, BlockType, u32)>> {
    let mut header_buf = [0u8; GZIP_FIXED_HEADER_SIZE];
    match reader.read_exact(&mut header_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }

    let (flg, xfl) = validate_gzip_header(&header_buf)?;
    let is_pgzf = is_pgzf_member(flg, xfl);

    // Read XLEN and extra field
    let mut xlen_buf = [0u8; 2];
    reader.read_exact(&mut xlen_buf)?;
    let xlen = read_u16_le_from(&xlen_buf);

    let mut extra = vec![0u8; xlen as usize];
    reader.read_exact(&mut extra)?;

    // Build full buffer for parsing
    let mut full_extra = vec![0u8; 2 + xlen as usize];
    full_extra[..2].copy_from_slice(&xlen_buf);
    full_extra[2..].copy_from_slice(&extra);

    let (zc, gc, ix, _fl) = parse_extra_field(&full_extra, is_pgzf)?;
    let block_type = determine_block_type(gc, &ix);

    let header_size = match block_type {
        BlockType::Beg => BEG_HEADER_SIZE,
        BlockType::Dat => DAT_HEADER_SIZE,
        BlockType::Idx => IDX_HEADER_BASE_SIZE + ix.as_ref().map_or(0, |e| e.len() * 8),
    };

    // Skip any remaining extra field bytes beyond what we parsed
    // (header_size already accounts for fixed header + XLEN + parsed tags)

    let deflate_size = (zc as usize)
        .checked_sub(header_size + PGZF_TAIL_SIZE)
        .ok_or_else(|| PgzfError::InvalidFormat("ZC too small".into()))?;

    let mut deflate_data = vec![0u8; deflate_size];
    reader.read_exact(&mut deflate_data)?;

    let mut trailer = [0u8; PGZF_TAIL_SIZE];
    reader.read_exact(&mut trailer)?;
    let expected_crc = read_u32_le(&trailer[0..4]);
    let expected_size = read_u32_le(&trailer[4..8]) as usize;

    if block_type == BlockType::Idx {
        // IDX block has no user data
        return Ok(Some((Vec::new(), block_type, zc)));
    }

    // Decompress
    let mut output = vec![0u8; expected_size.max(1)];
    let actual_size = decompress_block(&deflate_data, &mut output)?;
    output.truncate(actual_size);

    // Verify CRC
    let actual_crc = crc32fast::hash(&output);
    if actual_crc != expected_crc {
        return Err(PgzfError::CrcMismatch {
            expected: expected_crc,
            computed: actual_crc,
        });
    }

    Ok(Some((output, block_type, zc)))
}

fn read_u16_le_from(buf: &[u8; 2]) -> u16 {
    u16::from_le_bytes(*buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compress::compress_block;

    #[test]
    fn test_decompress_block() {
        let data = b"Test data for decompression";
        let compressed = compress_block(data, 6).unwrap();
        let mut output = vec![0u8; 1024];
        let size = decompress_block(&compressed, &mut output).unwrap();
        assert_eq!(&output[..size], data);
    }
}
