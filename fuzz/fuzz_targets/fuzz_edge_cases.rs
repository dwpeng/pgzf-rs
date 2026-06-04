#![no_main]
use libfuzzer_sys::fuzz_target;
use std::io::{Cursor, Read, Seek, SeekFrom, Write};

use pgzf::{PgzfConfig, PgzfReader, PgzfWriter};

// Fuzz target: test edge cases including empty data, single byte, data exactly
// filling one block, data crossing group boundaries, and various boundary
// conditions that are prone to off-by-one errors.
fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }

    // Choose a scenario based on the first byte
    let scenario = data[0] % 8;
    let payload = &data[1..];

    match scenario {
        // Scenario 0: empty payload
        0 => {
            let config = PgzfConfig::builder()
                .block_size(256)
                .group_blocks(4)
                .build();

            let buf = Vec::new();
            let cursor = Cursor::new(buf);
            let writer = PgzfWriter::with_config(cursor, config);
            // Don't write anything
            let cursor = match writer.finish() {
                Ok(c) => c,
                Err(_) => return,
            };
            let compressed = cursor.into_inner();
            if compressed.is_empty() {
                return;
            }

            let mut reader = match PgzfReader::new(Cursor::new(&compressed)) {
                Ok(r) => r,
                Err(_) => return,
            };
            let mut output = Vec::new();
            let _ = reader.read_to_end(&mut output);
            assert!(output.is_empty(), "empty payload should decompress to empty");
        }

        // Scenario 1: single byte
        1 => {
            if payload.is_empty() {
                return;
            }
            let byte_val = [payload[0]];
            roundtrip_test(&byte_val, 256, 4, 6);
        }

        // Scenario 2: payload exactly fills one block
        2 => {
            let block_size = 256;
            let exact_data = vec![0x42u8; block_size];
            roundtrip_test(&exact_data, block_size, 4, 6);
        }

        // Scenario 3: payload fills one block minus one byte
        3 => {
            let block_size = 256;
            let near_data = vec![0x42u8; block_size - 1];
            roundtrip_test(&near_data, block_size, 4, 6);
        }

        // Scenario 4: payload fills one block plus one byte (crosses boundary)
        4 => {
            let block_size = 256;
            let cross_data = vec![0x42u8; block_size + 1];
            roundtrip_test(&cross_data, block_size, 4, 6);
        }

        // Scenario 5: payload exactly fills one group
        5 => {
            let block_size = 128;
            let group_blocks = 3;
            let group_data = vec![0xABu8; block_size * group_blocks];
            roundtrip_test(&group_data, block_size, group_blocks, 6);
        }

        // Scenario 6: payload fills one group plus one block (crosses group boundary)
        6 => {
            let block_size = 128;
            let group_blocks = 3;
            let cross_group_data = vec![0xCDu8; block_size * group_blocks + 42];
            roundtrip_test(&cross_group_data, block_size, group_blocks, 6);
        }

        // Scenario 7: multiple groups
        7 => {
            let block_size = 64;
            let group_blocks = 2;
            let multi_group_data = vec![0xEFu8; block_size * group_blocks * 5 + 10];
            roundtrip_test(&multi_group_data, block_size, group_blocks, 6);
        }

        _ => unreachable!(),
    }
});

fn roundtrip_test(data: &[u8], block_size: usize, group_blocks: usize, level: u32) {
    let config = PgzfConfig::builder()
        .block_size(block_size)
        .group_blocks(group_blocks)
        .compression_level(level)
        .build();

    // Write
    let buf = Vec::new();
    let cursor = Cursor::new(buf);
    let mut writer = PgzfWriter::with_config(cursor, config);
    if writer.write_all(data).is_err() {
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

    // Test 1: Sequential read
    {
        let mut reader = match PgzfReader::new(Cursor::new(&compressed)) {
            Ok(r) => r,
            Err(_) => return,
        };
        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        assert_eq!(output, data, "sequential read mismatch");
    }

    // Test 2: Byte-by-byte read
    {
        let mut reader = match PgzfReader::new(Cursor::new(&compressed)) {
            Ok(r) => r,
            Err(_) => return,
        };
        let mut output = Vec::new();
        let mut byte_buf = [0u8; 1];
        loop {
            match reader.read(&mut byte_buf) {
                Ok(0) => break,
                Ok(n) => output.extend_from_slice(&byte_buf[..n]),
                Err(_) => break,
            }
        }
        assert_eq!(output, data, "byte-by-byte read mismatch");
    }

    // Test 3: Large buffer read (buffer larger than data)
    {
        let mut reader = match PgzfReader::new(Cursor::new(&compressed)) {
            Ok(r) => r,
            Err(_) => return,
        };
        let mut output = vec![0u8; data.len() + 1024];
        let n = reader.read(&mut output).unwrap();
        if n < data.len() {
            // Need to read more
            let mut remaining = Vec::new();
            reader.read_to_end(&mut remaining).unwrap();
            output[n..n + remaining.len()].copy_from_slice(&remaining);
            let total = n + remaining.len();
            assert_eq!(&output[..total], data, "large buffer read mismatch");
        } else {
            assert_eq!(&output[..n], data, "large buffer read mismatch");
        }
    }

    // Test 4: Seek to middle, read, seek back, read all
    if data.len() > 2 {
        let mut reader = match PgzfReader::new(Cursor::new(&compressed)) {
            Ok(r) => r,
            Err(_) => return,
        };
        let mid = data.len() / 2;
        reader.seek(SeekFrom::Start(mid as u64)).unwrap();
        let mut partial = vec![0u8; data.len() - mid];
        let n = reader.read(&mut partial).unwrap();
        assert_eq!(&partial[..n], &data[mid..], "mid-seek read mismatch");

        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut full = Vec::new();
        reader.read_to_end(&mut full).unwrap();
        assert_eq!(full, data, "re-seek read_all mismatch");
    }
}
