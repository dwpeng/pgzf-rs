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
# Compress (file -> file.gz)
pgzf input.txt

# Decompress (file.gz -> file)
pgzf -d input.txt.gz

# Stdin/stdout
echo "hello" | pgzf > out.gz
pgzf -d < out.gz

# Keep original files
pgzf -k input.txt

# Write to stdout
pgzf -c input.txt

# Compression level 9 with 4 threads
pgzf -l 9 -t 4 input.txt

# Random access: read 100 bytes at offset 1000
pgzf -d -s 1000 -q 100 input.txt.gz

# Inspect file info
pgzf -i input.txt.gz
```

### CLI Options

```
Usage: pgzf [OPTIONS] [FILE]...

Options:
  -d                  Decompress
  -c                  Write to stdout, keep original files
  -k                  Keep input files
  -f                  Force overwrite
  -o <OUTPUT>         Output file
  -t <THREADS>        Number of threads [default: 8]
  -b <BLOCK_SIZE_MB>  Block size in MB (1-256) [default: 1]
  -g <GROUP_BLOCKS>   Number of blocks per group [default: 8000]
  -s <SEEK_BYTE>      Seek to byte offset (decompress only)
  -q <LIMIT>          Limit output bytes (decompress only)
  -l <LEVEL>          Compression level (1-9) [default: 6]
  -i                  Inspect compressed file info
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