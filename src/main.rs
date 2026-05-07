use std::{
    fs::File,
    io::{self, Read, Seek, Write},
    path::PathBuf,
};

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "pgzf",
    about = "Parallel GZip Format compression/decompression"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Compress input to PGZF format
    Compress {
        /// Input file (default: stdin)
        #[arg(short, long)]
        input: Option<PathBuf>,

        /// Output file (default: stdout)
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Block size in MB (1-256, default: 1)
        #[arg(short = 'b', long, default_value_t = 1)]
        block_size_mb: usize,

        /// Number of blocks per group
        #[arg(short = 'g', long, default_value_t = 8000)]
        group_blocks: usize,

        /// Compression level (1-9, default: 6)
        #[arg(short = 'l', long, default_value_t = 6)]
        level: u32,

        /// Number of threads for parallel compression
        #[arg(short = 't', long, default_value_t = 8)]
        threads: usize,
    },

    /// Decompress PGZF or gzip input
    Decompress {
        /// Input file (default: stdin)
        #[arg(short, long)]
        input: Option<PathBuf>,

        /// Output file (default: stdout)
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Seek to byte offset before decompressing
        #[arg(short = 'p', long)]
        seek_byte: Option<u64>,

        /// Limit output to N bytes
        #[arg(short = 'q', long)]
        limit: Option<u64>,

        /// Number of threads for parallel decompression
        #[arg(short = 't', long, default_value_t = 8)]
        threads: usize,
    },

    /// Inspect PGZF file structure and index
    Inspect {
        /// Input PGZF file
        input: PathBuf,
    },
}

fn init_rayon_pool(threads: usize) {
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build_global();
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Compress {
            input,
            output,
            block_size_mb,
            group_blocks,
            level,
            threads,
        } => {
            init_rayon_pool(threads);

            let config = pgzf::PgzfConfig::builder()
                .block_size_mb(block_size_mb)
                .group_blocks(group_blocks)
                .compression_level(level)
                .threads(threads)
                .build();

            let mut reader: Box<dyn Read> = match &input {
                Some(path) => Box::new(File::open(path)?),
                None => Box::new(io::stdin()),
            };

            match output {
                Some(path) => {
                    let writer = File::create(path)?;
                    let mut pgzf_writer = pgzf::PgzfWriter::with_config(writer, config);
                    io::copy(&mut reader, &mut pgzf_writer)?;
                    pgzf_writer.finish()?;
                }
                None => {
                    let mut buf = Vec::new();
                    reader.read_to_end(&mut buf)?;
                    let cursor = std::io::Cursor::new(Vec::new());
                    let mut pgzf_writer = pgzf::PgzfWriter::with_config(cursor, config);
                    pgzf_writer.write_all(&buf)?;
                    let cursor = pgzf_writer.finish()?;
                    io::stdout().write_all(&cursor.into_inner())?;
                }
            };
        }

        Commands::Decompress {
            input,
            output,
            seek_byte,
            limit,
            threads,
        } => {
            init_rayon_pool(threads);

            match input {
                Some(path) => {
                    let file = File::open(path)?;
                    let reader = pgzf::PgzfReader::new(file)?;
                    decompress_stream(reader, &output, seek_byte, limit)?;
                }
                None => {
                    let mut buf = Vec::new();
                    io::stdin().read_to_end(&mut buf)?;
                    let cursor = std::io::Cursor::new(buf);
                    let reader = pgzf::PgzfReader::new(cursor)?;
                    decompress_stream(reader, &output, seek_byte, limit)?;
                }
            };
        }

        Commands::Inspect { input } => {
            let mut file = File::open(&input)?;
            let index = pgzf::PgzfIndex::build(&mut file)?;

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
    }

    Ok(())
}
