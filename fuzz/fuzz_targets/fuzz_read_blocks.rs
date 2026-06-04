#![no_main]
use libfuzzer_sys::fuzz_target;
use std::io::{Cursor, Read, Write};

use pgzf::{PgzfConfig, PgzfReader, PgzfWriter};

// Fuzz target: test the `read_blocks` API with random start_block and count
// parameters. Verifies that block range reads return the correct data slice.
fuzz_target!(|data: &[u8]| {
    if data.len() < 6 {
        return;
    }

    let block_size = ((data[0] as usize % 4) + 1) * 64; // 64..256
    let group_blocks = (data[1] as usize % 4) + 1; // 1..4
    let compression_level = (data[2] as u32 % 9) + 1;
    let payload = &data[3..];

    if payload.is_empty() {
        return;
    }

    let config = PgzfConfig::builder()
        .block_size(block_size)
        .group_blocks(group_blocks)
        .compression_level(compression_level)
        .build();

    // Write known data
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

    let reader = match PgzfReader::new(Cursor::new(&compressed)) {
        Ok(r) => r,
        Err(_) => return,
    };

    let total_blocks = match reader.block_count() {
        Some(c) if c > 0 => c,
        _ => return,
    };

    // Test various start_block and count combinations
    let test_cases: Vec<(usize, usize)> = vec![
        (0, 0),                                           // zero count
        (0, 1),                                           // single block
        (0, total_blocks),                                // all blocks
        (total_blocks.saturating_sub(1), 1),              // last block
        (total_blocks / 2, total_blocks.saturating_sub(total_blocks / 2)), // second half
        (0, total_blocks * 2),                            // over-request
    ];

    for (start, count) in test_cases {
        let mut reader = match PgzfReader::new(Cursor::new(&compressed)) {
            Ok(r) => r,
            Err(_) => continue,
        };

        if start >= total_blocks {
            // Should return error for out-of-range start
            let result = reader.read_blocks(start, count);
            assert!(result.is_err(), "expected error for start={start}, count={count}");
            continue;
        }

        let data = match reader.read_blocks(start, count) {
            Ok(d) => d,
            Err(_) => continue,
        };

        if count == 0 {
            assert!(data.is_empty(), "zero count should return empty");
            continue;
        }

        // Verify against expected payload slice
        let expected_start = start * block_size;
        let expected_end = (start + count).min(total_blocks) * block_size;
        let expected_end = expected_end.min(payload.len());

        if expected_start < payload.len() {
            let expected = &payload[expected_start..expected_end];
            assert_eq!(
                data, expected,
                "read_blocks({start}, {count}) mismatch"
            );
        }
    }

    // Test: read_blocks followed by sequential read
    {
        let mut reader = match PgzfReader::new(Cursor::new(&compressed)) {
            Ok(r) => r,
            Err(_) => return,
        };
        let read_count = total_blocks.min(2);
        let first_data = match reader.read_blocks(0, read_count) {
            Ok(d) => d,
            Err(_) => return,
        };

        // Read remaining via sequential Read
        let mut rest = Vec::new();
        let _ = reader.read_to_end(&mut rest);

        // Combine should equal full payload
        let mut combined = first_data;
        combined.extend_from_slice(&rest);

        if combined.len() <= payload.len() {
            assert_eq!(
                combined,
                &payload[..combined.len()],
                "read_blocks + sequential read mismatch"
            );
        }
    }
});
