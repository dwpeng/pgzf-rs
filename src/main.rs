use std::{
    fs::File,
    io::{self, Read, Seek, Write},
    path::{Path, PathBuf},
};

use clap::Parser;

#[derive(Parser)]
#[command(
    name = "pgzf",
    about = "Parallel GZip Format compression/decompression"
)]
struct Cli {
    /// Decompress
    #[arg(short = 'd')]
    decompress: bool,

    /// Write to stdout, keep original files
    #[arg(short = 'c')]
    stdout: bool,

    /// Keep input files
    #[arg(short = 'k', short_alias = 'K')]
    keep: bool,

    /// Force overwrite
    #[arg(short = 'f')]
    force: bool,

    /// Output file
    #[arg(short = 'o')]
    output: Option<PathBuf>,

    /// Number of threads
    #[arg(short = 't', default_value_t = 8)]
    threads: usize,

    /// Block size in MB (1-256)
    #[arg(short = 'b', default_value_t = 1)]
    block_size_mb: usize,

    /// Number of blocks per group
    #[arg(short = 'g', default_value_t = 8000)]
    group_blocks: usize,

    /// Seek to byte offset (decompress only)
    #[arg(short = 's')]
    seek_byte: Option<u64>,

    /// Limit output bytes (decompress only)
    #[arg(short = 'q')]
    limit: Option<u64>,

    /// Compression level (1-9)
    #[arg(short = 'l', default_value_t = 6)]
    level: u32,

    /// Input files
    #[arg(value_name = "FILE")]
    files: Vec<PathBuf>,

    /// Inspect compressed file info
    #[arg(short = 'i')]
    inspect: bool,
}

fn init_rayon_pool(threads: usize) {
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build_global();
}

fn gz_extension(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".gz");
    PathBuf::from(s)
}

fn strip_gz_extension(path: &Path) -> Option<PathBuf> {
    let s = path.to_str()?;
    s.strip_suffix(".gz").map(PathBuf::from)
}

fn decompress_stream<R: Read + Seek>(
    mut reader: pgzf::PgzfReader<R>,
    output: &Option<PathBuf>,
    seek_byte: Option<u64>,
    limit: Option<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(offset) = seek_byte {
        reader.seek_to_byte(offset)?;
    }

    let mut writer: Box<dyn Write> = match output {
        Some(path) => Box::new(File::create(path)?),
        None => Box::new(io::stdout()),
    };

    if let Some(limit) = limit {
        let mut remaining = limit;
        let mut buf = vec![0u8; 64 * 1024];
        while remaining > 0 {
            let to_read = (remaining as usize).min(buf.len());
            let n = reader.read(&mut buf[..to_read])?;
            if n == 0 {
                break;
            }
            writer.write_all(&buf[..n])?;
            remaining -= n as u64;
        }
    } else {
        io::copy(&mut reader, &mut writer)?;
    }
    Ok(())
}

fn compress_file(
    input: &Path,
    output: &Path,
    config: pgzf::PgzfConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut reader = File::open(input)?;
    let writer = File::create(output)?;
    let mut pgzf_writer = pgzf::PgzfWriter::with_config(writer, config);
    io::copy(&mut reader, &mut pgzf_writer)?;
    pgzf_writer.finish()?;
    Ok(())
}

fn compress_stdin(
    output: &Option<PathBuf>,
    config: pgzf::PgzfConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let cursor = std::io::Cursor::new(Vec::new());
    let mut pgzf_writer = pgzf::PgzfWriter::with_config(cursor, config);
    io::copy(&mut io::stdin(), &mut pgzf_writer)?;
    let cursor = pgzf_writer.finish()?;
    let data = cursor.into_inner();
    match output {
        Some(path) => std::fs::write(path, data)?,
        None => io::stdout().write_all(&data)?,
    }
    Ok(())
}

fn decompress_file(
    input: &Path,
    output: &Option<PathBuf>,
    seek_byte: Option<u64>,
    limit: Option<u64>,
    threads: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open(input)?;
    let reader = pgzf::PgzfReader::new(file)?.with_readahead(threads);
    decompress_stream(reader, output, seek_byte, limit)?;
    Ok(())
}

fn decompress_stdin(
    output: &Option<PathBuf>,
    seek_byte: Option<u64>,
    limit: Option<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut buf = Vec::new();
    io::stdin().read_to_end(&mut buf)?;
    let cursor = std::io::Cursor::new(buf);
    let reader = pgzf::PgzfReader::new(cursor)?;
    decompress_stream(reader, output, seek_byte, limit)?;
    Ok(())
}

fn inspect_gzip(input: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut file = File::open(input)?;
    let file_len = file.metadata()?.len();

    // Read gzip header
    let mut header = [0u8; 10];
    file.read_exact(&mut header)?;
    if header[0] != 0x1f || header[1] != 0x8b {
        return Err("not a gzip file".into());
    }

    let cm = header[2];
    let flg = header[3];
    let mtime = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);

    // Skip extra field
    if flg & 0x04 != 0 {
        let mut xlen_buf = [0u8; 2];
        file.read_exact(&mut xlen_buf)?;
        let xlen = u16::from_le_bytes(xlen_buf) as u64;
        file.seek(io::SeekFrom::Current(xlen as i64))?;
    }

    // Skip filename
    if flg & 0x08 != 0 {
        let mut b = [0u8; 1];
        loop {
            file.read_exact(&mut b)?;
            if b[0] == 0 {
                break;
            }
        }
    }

    // Skip comment
    if flg & 0x10 != 0 {
        let mut b = [0u8; 1];
        loop {
            file.read_exact(&mut b)?;
            if b[0] == 0 {
                break;
            }
        }
    }

    // Read footer: CRC32 (4) + ISIZE (4)
    file.seek(io::SeekFrom::End(-8))?;
    let mut footer = [0u8; 8];
    file.read_exact(&mut footer)?;
    let isize = u32::from_le_bytes([footer[4], footer[5], footer[6], footer[7]]);

    let methods = match cm {
        0 => "store",
        8 => "deflate",
        _ => "unknown",
    };

    println!("File: {}", input.display());
    println!("PGZF: no");
    println!("Method: {}", methods);
    println!("Modified: {}", mtime);
    println!("Compressed size: {} bytes", file_len);
    println!("Uncompressed size: {} bytes", isize);
    let ratio = if isize > 0 {
        file_len as f64 / isize as f64
    } else {
        0.0
    };
    println!("Compression ratio: {:.2}%", ratio * 100.0);
    Ok(())
}

fn inspect_file(input: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut file = File::open(input)?;

    match pgzf::PgzfIndex::build(&mut file) {
        Ok(index) => {
            println!("File: {}", input.display());
            println!("PGZF: yes");
            println!("Groups: {}", index.group_count());
            println!("Data blocks: {}", index.block_count());
            println!("Total uncompressed: {} bytes", index.total_uncompressed());
            println!("Total compressed: {} bytes", index.total_compressed());
            let ratio = if index.total_uncompressed() > 0 {
                index.total_compressed() as f64 / index.total_uncompressed() as f64
            } else {
                0.0
            };
            println!("Compression ratio: {:.2}%", ratio * 100.0);
        }
        Err(_) => {
            inspect_gzip(input)?;
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    init_rayon_pool(cli.threads);

    let config = pgzf::PgzfConfig::builder()
        .block_size_mb(cli.block_size_mb)
        .group_blocks(cli.group_blocks)
        .compression_level(cli.level)
        .build();

    if cli.inspect {
        // Inspect mode -i
        if cli.files.is_empty() {
            return Err("pgzf -i requires at least one file".into());
        }
        for file in &cli.files {
            inspect_file(file)?;
        }
        return Ok(());
    }

    if cli.decompress {
        // Decompress mode: -d
        if cli.files.is_empty() {
            decompress_stdin(&cli.output, cli.seek_byte, cli.limit)?;
        } else {
            for file in &cli.files {
                let output = match &cli.output {
                    Some(o) => Some(o.clone()),
                    None if cli.stdout => None, // stdout
                    None => Some(strip_gz_extension(file).unwrap_or_else(|| {
                        let mut s = file.as_os_str().to_owned();
                        s.push(".out");
                        PathBuf::from(s)
                    })),
                };

                decompress_file(file, &output, cli.seek_byte, cli.limit, cli.threads)?;

                // Delete input file if not keeping and not writing to stdout
                if !cli.keep && !cli.stdout && output.is_some() {
                    let _ = std::fs::remove_file(file);
                }
            }
        }
    } else {
        // Compress mode (default)
        if cli.files.is_empty() {
            compress_stdin(&cli.output, config.clone())?;
        } else {
            for file in &cli.files {
                let output = match &cli.output {
                    Some(o) => Some(o.clone()),
                    None if cli.stdout => None, // stdout
                    None => Some(gz_extension(file)),
                };

                if cli.stdout {
                    // Buffer to Cursor (PgzfWriter needs Seek for backpatching)
                    let mut reader = File::open(file)?;
                    let cursor = std::io::Cursor::new(Vec::new());
                    let mut pgzf_writer = pgzf::PgzfWriter::with_config(cursor, config.clone());
                    io::copy(&mut reader, &mut pgzf_writer)?;
                    let cursor = pgzf_writer.finish()?;
                    io::stdout().write_all(&cursor.into_inner())?;
                } else {
                    let out = output.as_ref().unwrap();
                    if !cli.force && out.exists() {
                        return Err(
                            format!("{}: file exists, use -f to overwrite", out.display()).into(),
                        );
                    }
                    compress_file(file, out, config.clone())?;

                    // Delete input file if not keeping
                    if !cli.keep {
                        let _ = std::fs::remove_file(file);
                    }
                }
            }
        }
    }

    Ok(())
}
