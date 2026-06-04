#![no_main]
use libfuzzer_sys::fuzz_target;
use std::io::{Cursor, Read, Write};

use pgzf::{PgzfConfig, PgzfReader, PgzfWriter};

// Fuzz target: test block cache behavior with various cache configurations.
// Verifies that cache hit/miss, eviction, memory limits, and seek-surviving
// cache all work correctly under random access patterns.
fuzz_target!(|data: &[u8]| {
    if data.len() < 6 {
        return;
    }

    let block_size = ((data[0] as usize % 3) + 1) * 64; // 64..192
    let group_blocks = (data[1] as usize % 3) + 1; // 1..3
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

    // Test 1: Default cache (64 blocks)
    {
        let mut reader = match PgzfReader::new(Cursor::new(&compressed)) {
            Ok(r) => r,
            Err(_) => return,
        };
        assert_eq!(reader.block_cache_capacity(), 64);

        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(output, payload, "default cache read mismatch");

        // Cache should have entries after full read
        if reader.block_count().unwrap_or(0) > 0 {
            assert!(
                reader.block_cache_len() > 0,
                "cache should be populated after read"
            );
        }
    }

    // Test 2: Small cache (1 block) - forces eviction
    {
        let mut reader = match PgzfReader::new(Cursor::new(&compressed)) {
            Ok(r) => r.with_block_cache(1),
            Err(_) => return,
        };
        assert_eq!(reader.block_cache_capacity(), 1);

        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(output, payload, "small cache read mismatch");
        assert!(
            reader.block_cache_len() <= 1,
            "small cache should have at most 1 entry"
        );
    }

    // Test 3: Disabled cache (capacity 0)
    {
        let mut reader = match PgzfReader::new(Cursor::new(&compressed)) {
            Ok(r) => r.with_block_cache(0),
            Err(_) => return,
        };
        assert_eq!(reader.block_cache_capacity(), 0);

        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(output, payload, "disabled cache read mismatch");
        assert_eq!(reader.block_cache_len(), 0, "disabled cache should have 0 entries");
    }

    // Test 4: Large cache - should cache everything
    {
        let block_count = match PgzfReader::new(Cursor::new(&compressed)) {
            Ok(r) => r.block_count().unwrap_or(0),
            Err(_) => return,
        };
        if block_count > 0 {
            let mut reader = match PgzfReader::new(Cursor::new(&compressed)) {
                Ok(r) => r.with_block_cache(block_count * 2),
                Err(_) => return,
            };

            let mut output = Vec::new();
            reader.read_to_end(&mut output).unwrap();
            assert_eq!(output, payload, "large cache read mismatch");

            // Should have cached all blocks
            assert_eq!(
                reader.block_cache_len(),
                block_count,
                "large cache should have all {block_count} blocks, got {}",
                reader.block_cache_len()
            );
        }
    }

    // Test 5: Cache with memory limit
    {
        let mut reader = match PgzfReader::new(Cursor::new(&compressed)) {
            Ok(r) => r.with_block_cache_memory_limit(100, block_size * 2),
            Err(_) => return,
        };

        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(output, payload, "memory-limited cache read mismatch");

        // Memory usage should respect limit
        assert!(
            reader.block_cache_memory_usage() <= block_size * 2,
            "cache memory {} exceeded limit {}",
            reader.block_cache_memory_usage(),
            block_size * 2
        );
    }

    // Test 6: Cache survives seek and re-read
    {
        if payload.len() > block_size {
            let mut reader = match PgzfReader::new(Cursor::new(&compressed)) {
                Ok(r) => r,
                Err(_) => return,
            };

            // Read some data to populate cache
            let mut first_read = vec![0u8; block_size];
            let n = reader.read(&mut first_read).unwrap();
            assert_eq!(&first_read[..n], &payload[..n]);

            let cache_before = reader.block_cache_len();
            assert!(cache_before > 0, "cache should have entries after initial read");

            // Seek to beginning
            reader.seek_to_byte(0).unwrap();

            // Cache should survive
            assert_eq!(
                reader.block_cache_len(),
                cache_before,
                "cache should survive seek"
            );

            // Re-read should produce correct data
            let mut output = Vec::new();
            reader.read_to_end(&mut output).unwrap();
            assert_eq!(output, payload, "re-read after seek mismatch");
        }
    }

    // Test 7: Memory tracking consistency
    {
        let mut reader = match PgzfReader::new(Cursor::new(&compressed)) {
            Ok(r) => r,
            Err(_) => return,
        };

        // Initial memory should be 0
        assert_eq!(reader.block_cache_memory_usage(), 0);
        assert_eq!(reader.readahead_memory_usage(), 0);
        assert_eq!(reader.total_memory_usage(), 0);

        // Read all data
        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(output, payload);

        // After full read, total memory should be >= cache memory
        let cache_mem = reader.block_cache_memory_usage();
        let total_mem = reader.total_memory_usage();
        assert!(
            total_mem >= cache_mem,
            "total memory {total_mem} should be >= cache memory {cache_mem}"
        );
    }

    // Test 8: Rendahead memory limit
    {
        let mut reader = match PgzfReader::new(Cursor::new(&compressed)) {
            Ok(r) => r.with_readahead_memory_limit(block_size * 2),
            Err(_) => return,
        };

        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(output, payload, "readahead memory-limited read mismatch");
    }
});
