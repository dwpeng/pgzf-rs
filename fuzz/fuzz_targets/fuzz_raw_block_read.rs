#![no_main]
use libfuzzer_sys::fuzz_target;
use std::io::{Cursor, Read, Write};

use pgzf::{BlockType, PgzfConfig, PgzfReader, PgzfWriter};

// Fuzz target: test `read_one_raw_block` API to verify raw block reading
// works correctly. Iterates through all blocks and checks metadata.
fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }

    let block_size = ((data[0] as usize % 4) + 1) * 64;
    let group_blocks = (data[1] as usize % 4) + 1;
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

    // Read raw blocks
    let mut reader = match PgzfReader::new(Cursor::new(&compressed)) {
        Ok(r) => r,
        Err(_) => return,
    };

    let expected_block_count = reader.block_count().unwrap_or(0);
    let mut data_block_count = 0usize;
    let mut idx_block_count = 0usize;
    let mut beg_block_count = 0usize;
    let mut total_raw_size = 0usize;

    loop {
        match reader.read_one_raw_block() {
            Ok(Some(raw_block)) => {
                total_raw_size += raw_block.raw.len();

                // Header size should be reasonable
                assert!(
                    raw_block.header_size <= raw_block.raw.len(),
                    "header_size {} > raw len {}",
                    raw_block.header_size,
                    raw_block.raw.len()
                );

                match raw_block.block_type {
                    BlockType::Beg => {
                        beg_block_count += 1;
                        // BEG should be first block in each group
                        // header_size should be BEG_HEADER_SIZE (32)
                        assert_eq!(raw_block.header_size, 32, "BEG header size mismatch");
                    }
                    BlockType::Dat => {
                        data_block_count += 1;
                        // header_size should be DAT_HEADER_SIZE (20)
                        assert_eq!(raw_block.header_size, 20, "DAT header size mismatch");
                    }
                    BlockType::Idx => {
                        idx_block_count += 1;
                        // IDX blocks don't carry user data; just continue
                    }
                }

                // Raw block should start with gzip magic
                assert!(
                    raw_block.raw.len() >= 2,
                    "raw block too short: {} bytes",
                    raw_block.raw.len()
                );
                assert_eq!(raw_block.raw[0], 0x1f, "bad gzip magic byte 0");
                assert_eq!(raw_block.raw[1], 0x8b, "bad gzip magic byte 1");
            }
            Ok(None) => break,
            Err(_) => return,
        }
    }

    // Verify block structure
    // Number of BEG blocks should equal number of groups
    // Number of IDX blocks should equal number of groups
    // Number of DAT blocks + BEG blocks should equal total data blocks
    assert_eq!(
        beg_block_count, idx_block_count,
        "BEG count ({beg_block_count}) != IDX count ({idx_block_count})"
    );

    let total_data_blocks = beg_block_count + data_block_count;
    assert_eq!(
        total_data_blocks, expected_block_count,
        "data block count mismatch: got {total_data_blocks}, expected {expected_block_count}"
    );

    // Total raw size should equal compressed size
    assert_eq!(
        total_raw_size,
        compressed.len(),
        "total raw size ({total_raw_size}) != compressed len ({compressed_len})",
        compressed_len = compressed.len()
    );

    // Also verify sequential read produces the same data
    {
        let mut reader2 = match PgzfReader::new(Cursor::new(&compressed)) {
            Ok(r) => r,
            Err(_) => return,
        };
        let mut output = Vec::new();
        reader2.read_to_end(&mut output).unwrap();
        assert_eq!(output, payload, "sequential read after raw block iteration mismatch");
    }
});
