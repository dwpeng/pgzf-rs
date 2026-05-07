# pgzf-rs

A Rust implementation of [PGZF (Parallel GZip Format)](https://github.com/ruanjue/pgzf), a blocked compression format that extends standard GZIP (RFC 1952) with parallel compression/decompression and random access support.

**PGZF format was designed and created by [Jue Ruan](https://github.com/ruanjue) (ruanjue@gmail.com).** This crate is a Rust reimplementation of his original [C implementation](https://github.com/ruanjue/pgzf). All credit for the format design belongs to the original author.

> If you use PGZF, please cite:
> Parallel random access GZIP format file. Jue Ruan. https://github.com/ruanjue/pgzf

## Features

- **Parallel compression** -- blocks within a group are compressed concurrently via [rayon](https://github.com/rayon-rs/rayon)
- **Parallel decompression** -- read-ahead buffer with batch parallel decompression
- **Random access** -- seek by byte offset or block index using the built-in index
- **GZIP compatible** -- every PGZF file is a valid sequence of gzip members; `gzip -d` can decompress it
- **Streaming API** -- implements `std::io::Write` (compressor) and `std::io::Read` + `std::io::Seek` (decompressor)
- **Auto-detection** -- reader automatically detects PGZF vs standard gzip files

## Install

```toml
[dependencies]
pgzf = "0.1"
```

## CLI Usage

```bash
# Compress
pgzf compress -i input.txt -o output.gz
pgzf compress -b 2 -l 9 -t 4 -i input.txt -o output.gz

# Decompress
pgzf decompress -i output.gz -o decoded.txt
pgzf decompress -p 1000 -q 100 -i output.gz   # random access: read 100 bytes at offset 1000

# Inspect
pgzf inspect output.gz
```

### CLI Options

```
Compress:
  -i, --input <FILE>          Input file (default: stdin)
  -o, --output <FILE>         Output file (default: stdout)
  -b, --block-size-mb <MB>    Block size in MB, 1-256 [default: 1]
  -g, --group-blocks <N>      Blocks per group [default: 8000]
  -l, --level <1-9>           Compression level [default: 6]
  -t, --threads <N>           Parallel threads [default: 8]

Decompress:
  -i, --input <FILE>          Input file (default: stdin)
  -o, --output <FILE>         Output file (default: stdout)
  -p, --seek-byte <OFFSET>    Seek to byte offset before reading
  -q, --limit <N>             Limit output to N bytes
  -t, --threads <N>           Parallel threads [default: 8]
```

## Library Usage

### Compress

```rust
use pgzf::{PgzfWriter, PgzfConfig};
use std::io::{Write, Cursor};

let config = PgzfConfig::builder()
    .block_size_mb(1)
    .group_blocks(8000)
    .compression_level(6)
    .build();

let mut writer = PgzfWriter::with_config(Cursor::new(Vec::new()), config);
writer.write_all(b"Hello, PGZF!").unwrap();
let cursor = writer.finish().unwrap();
let compressed = cursor.into_inner();
```

### Decompress

```rust
use pgzf::PgzfReader;
use std::io::Read;

let mut reader = PgzfReader::new(std::io::Cursor::new(compressed)).unwrap();
let mut output = String::new();
reader.read_to_string(&mut output).unwrap();
assert_eq!(output, "Hello, PGZF!");
```

### Random Access

```rust
use pgzf::PgzfReader;
use std::io::{Read, Seek, SeekFrom};

let mut reader = PgzfReader::new(file).unwrap();

// Seek by byte offset
reader.seek_to_byte(1000).unwrap();
let mut buf = [0u8; 100];
reader.read(&mut buf).unwrap();

// Seek by block index
reader.seek_to_block(5).unwrap();

// Standard Seek trait
reader.seek(SeekFrom::Start(500)).unwrap();
```

### Inspect Index

```rust
use pgzf::PgzfIndex;
use std::fs::File;

let mut file = File::open("output.gz").unwrap();
let index = PgzfIndex::build(&mut file).unwrap();

println!("Groups: {}", index.group_count());
println!("Data blocks: {}", index.block_count());
println!("Uncompressed size: {} bytes", index.total_uncompressed());
println!("Compressed size: {} bytes", index.total_compressed());
```

## Specification

- [PGZF Format Specification](./pgzf.spec.md)