#![no_main]
use libfuzzer_sys::fuzz_target;
use std::io::{Cursor, Read, Write};

use pgzf::{PgzfConfig, PgzfReader, PgzfWriter};

// Fuzz target: write random data with varying configurations, then read back
// and verify byte-for-byte correctness. Covers the core write/read roundtrip.
fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }

    // Derive configuration from fuzz input
    let block_size = ((data[0] as usize % 7) + 1) * 64; // 64..512
    let group_blocks = (data[1] as usize % 5) + 1; // 1..5 (small groups for fast fuzzing)
    let compression_level = (data[2] as u32 % 9) + 1; // 1..9
    let payload = &data[3..];

    let config = PgzfConfig::builder()
        .block_size(block_size)
        .group_blocks(group_blocks)
        .compression_level(compression_level)
        .build();

    // Write
    let buf = Vec::new();
    let cursor = Cursor::new(buf);
    let mut writer = PgzfWriter::with_config(cursor, config);
    if writer.write_all(payload).is_err() {
        return;
    }
    let cursor = match writer.finish() {
        Ok(c) => c,
        Err(_) => return,
    };
    let compressed = cursor.into_inner();

    if compressed.is_empty() {
        return;
    }

    // Read back sequentially
    let mut reader = match PgzfReader::new(Cursor::new(&compressed)) {
        Ok(r) => r,
        Err(_) => return,
    };

    let mut output = Vec::new();
    if reader.read_to_end(&mut output).is_err() {
        return;
    }

    // Verify data integrity
    assert_eq!(
        output, payload,
        "roundtrip mismatch: wrote {} bytes, read {} bytes",
        payload.len(),
        output.len()
    );

    // Verify metadata
    assert!(reader.is_pgzf());
    assert!(reader.block_count().is_some());
    assert!(reader.total_uncompressed_size().is_some());
    assert!(reader.total_compressed_size().is_some());
});
