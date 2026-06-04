#![no_main]
use libfuzzer_sys::fuzz_target;
use std::io::{Cursor, Read, Seek, SeekFrom, Write};

use pgzf::{PgzfConfig, PgzfReader, PgzfWriter};

// Fuzz target: perform random seek operations on PGZF data and verify
// that reads after seeks return the correct bytes. Tests seek_to_byte,
// seek_to_block, and standard Seek trait implementations.
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

    let total_size = payload.len() as u64;

    // Test 1: SeekToStart then read all
    {
        let mut reader = match PgzfReader::new(Cursor::new(&compressed)) {
            Ok(r) => r,
            Err(_) => return,
        };
        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(output, payload, "SeekStart(0) + read_all mismatch");
    }

    // Test 2: seek_to_byte at various offsets
    if payload.len() > 1 {
        let test_offsets: Vec<u64> = vec![
            0,
            1,
            (payload.len() / 2) as u64,
            payload.len().saturating_sub(1) as u64,
        ];

        for &offset in &test_offsets {
            if offset >= total_size {
                continue;
            }
            let mut reader = match PgzfReader::new(Cursor::new(&compressed)) {
                Ok(r) => r,
                Err(_) => continue,
            };
            reader.seek_to_byte(offset).unwrap();
            let mut output = vec![0u8; (total_size - offset).min(64) as usize];
            let n = reader.read(&mut output).unwrap();
            let expected = &payload[offset as usize..offset as usize + n];
            assert_eq!(
                &output[..n],
                expected,
                "seek_to_byte({offset}) mismatch"
            );
        }
    }

    // Test 3: seek_to_block
    {
        let block_count = match PgzfReader::new(Cursor::new(&compressed)) {
            Ok(r) => r.block_count().unwrap_or(0),
            Err(_) => 0,
        };
        if block_count > 0 {
            let test_blocks: Vec<i64> = vec![
                0,
                (block_count / 2) as i64,
                (block_count.saturating_sub(1)) as i64,
            ];

            for &block_idx in &test_blocks {
                if block_idx >= block_count as i64 {
                    continue;
                }
                let mut reader = match PgzfReader::new(Cursor::new(&compressed)) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                reader.seek_to_block(block_idx).unwrap();
                let mut output = Vec::new();
                reader.read_to_end(&mut output).unwrap();
                // Should read from block_idx to end
                let expected_start = (block_idx as usize) * block_size;
                if expected_start < payload.len() {
                    assert!(
                        output.len() > 0,
                        "seek_to_block({block_idx}) produced empty output"
                    );
                    assert_eq!(
                        output,
                        &payload[expected_start..],
                        "seek_to_block({block_idx}) data mismatch"
                    );
                }
            }
        }
    }

    // Test 4: SeekFrom::Current
    {
        let mut reader = match PgzfReader::new(Cursor::new(&compressed)) {
            Ok(r) => r,
            Err(_) => return,
        };
        // Read first few bytes
        let read_size = payload.len().min(block_size);
        let mut buf = vec![0u8; read_size];
        let n = reader.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], &payload[..n]);

        // Seek back to start via Current
        reader.seek(SeekFrom::Current(-(n as i64))).unwrap();
        let mut buf2 = vec![0u8; n];
        let n2 = reader.read(&mut buf2).unwrap();
        assert_eq!(&buf2[..n2], &payload[..n2], "SeekFrom::Current roundtrip mismatch");
    }

    // Test 5: SeekFrom::End
    {
        let mut reader = match PgzfReader::new(Cursor::new(&compressed)) {
            Ok(r) => r,
            Err(_) => return,
        };
        if payload.len() > 1 {
            let seek_back = (payload.len() as i64).min(16);
            reader.seek(SeekFrom::End(-seek_back)).unwrap();
            let mut output = Vec::new();
            reader.read_to_end(&mut output).unwrap();
            let expected_start = payload.len() - seek_back as usize;
            assert_eq!(
                output,
                &payload[expected_start..],
                "SeekFrom::End mismatch"
            );
        }
    }
});
