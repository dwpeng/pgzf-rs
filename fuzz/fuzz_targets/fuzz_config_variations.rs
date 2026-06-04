#![no_main]
use libfuzzer_sys::fuzz_target;
use std::io::{Cursor, Read, Write};

use pgzf::{PgzfConfig, PgzfReader, PgzfWriter};

// Fuzz target: test various PgzfConfig parameter combinations including
// different block sizes, group sizes, compression levels, and memory limits.
// Verifies that any valid configuration produces correct roundtrip output.
fuzz_target!(|data: &[u8]| {
    if data.len() < 8 {
        return;
    }

    // Derive config parameters from fuzz input
    let block_size_idx = data[0] as usize % 8;
    let block_size = match block_size_idx {
        0 => 1,       // minimum
        1 => 16,      // very small
        2 => 64,      // small
        3 => 256,     // medium-small
        4 => 1024,    // medium
        5 => 4096,    // medium-large
        6 => 65536,   // large
        _ => 1 << 20, // 1MB default
    };

    let group_blocks_idx = data[1] as usize % 6;
    let group_blocks = match group_blocks_idx {
        0 => 1,   // minimum
        1 => 2,   // small
        2 => 5,   // medium
        3 => 10,  // larger
        4 => 100, // large
        _ => 8000, // default
    };

    let compression_level = (data[2] as u32 % 9) + 1;
    let compression_batch_size = ((data[3] as usize) % 10) + 1;

    // Memory limits: some combinations with, some without
    let cache_limit = match data[4] % 4 {
        0 => None,
        1 => Some(block_size * 2),
        2 => Some(block_size * group_blocks),
        _ => Some(block_size / 2 + 1), // tight limit
    };

    let readahead_limit = match data[5] % 3 {
        0 => None,
        1 => Some(block_size * 4),
        _ => Some(block_size),
    };

    let payload = &data[6..];

    let mut builder = PgzfConfig::builder()
        .block_size(block_size)
        .group_blocks(group_blocks)
        .compression_level(compression_level)
        .compression_batch_size(compression_batch_size);

    if let Some(limit) = cache_limit {
        builder = builder.cache_memory_limit_bytes(limit);
    }
    if let Some(limit) = readahead_limit {
        builder = builder.readahead_memory_limit_bytes(limit);
    }

    let config = builder.build();

    // Write
    let buf = Vec::new();
    let cursor = Cursor::new(buf);
    let mut writer = PgzfWriter::with_config(cursor, config.clone());
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

    // Read with matching config settings
    let mut reader = match PgzfReader::new(Cursor::new(&compressed)) {
        Ok(r) => r,
        Err(_) => return,
    };

    // Apply memory limits to reader too
    if let Some(limit) = config.cache_memory_limit_bytes {
        reader = reader.with_block_cache_memory_limit(
            group_blocks.max(16),
            limit,
        );
    }
    if let Some(limit) = config.readahead_memory_limit_bytes {
        reader = reader.with_readahead_memory_limit(limit);
    }

    let mut output = Vec::new();
    if reader.read_to_end(&mut output).is_err() {
        return;
    }

    assert_eq!(
        output, payload,
        "config variation roundtrip failed: block_size={block_size}, group_blocks={group_blocks}, level={compression_level}"
    );
});
