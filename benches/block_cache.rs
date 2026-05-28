use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use pgzf::{PgzfConfig, PgzfReader, PgzfWriter};
use std::io::{Cursor, Read, Seek, SeekFrom, Write};

/// Generate deterministic data with a repeating pattern.
fn generate_pattern_data(size: usize) -> Vec<u8> {
    let mut data = vec![0u8; size];
    for (i, byte) in data.iter_mut().enumerate() {
        *byte = ((i * 7 + (i >> 3) * 13) & 0xFF) as u8;
    }
    data
}

/// Create a PGZF compressed blob from raw data.
fn create_pgzf(data: &[u8], block_size: usize) -> Vec<u8> {
    let config = PgzfConfig::builder()
        .block_size(block_size)
        .group_blocks(8000)
        .compression_level(3)
        .build();
    let mut writer = PgzfWriter::with_config(Cursor::new(Vec::new()), config);
    writer.write_all(data).unwrap();
    writer.finish().unwrap().into_inner()
}

/// Simple deterministic RNG for reproducible seek targets.
struct Rng {
    state: u64,
}

impl Rng {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        self.state
    }

    fn range(&mut self, max: u64) -> u64 {
        self.next_u64() % max
    }
}

// ---------------------------------------------------------------------------
// Benchmark: random seek + small read, with vs without cache
// ---------------------------------------------------------------------------

fn bench_random_seek(c: &mut Criterion) {
    let mut group = c.benchmark_group("random_seek");

    for &(data_mb, block_kb) in &[(10_usize, 64_usize), (50, 64), (50, 256)] {
        let total = data_mb * 1024 * 1024;
        let bs = block_kb * 1024;
        let label = format!("{data_mb}MB_{block_kb}KB_blk");

        let data = generate_pattern_data(total);
        let pgzf = create_pgzf(&data, bs);

        // Pre-compute seek targets (byte offsets across the whole file)
        let num_seeks = 200;
        let mut rng = Rng::new(42);
        let targets: Vec<u64> = (0..num_seeks).map(|_| rng.range(total as u64)).collect();

        // With cache (default 64 blocks)
        group.bench_with_input(
            BenchmarkId::new("cached", &label),
            &(&pgzf, &targets),
            |b, &(pgzf, targets)| {
                b.iter(|| {
                    let mut reader =
                        PgzfReader::new(Cursor::new(pgzf)).unwrap().with_block_cache(64);
                    let mut buf = vec![0u8; 1024];
                    for &offset in targets {
                        reader.seek(SeekFrom::Start(offset)).unwrap();
                        reader.read_exact(&mut buf).unwrap();
                    }
                    reader
                });
            },
        );

        // Without cache
        group.bench_with_input(
            BenchmarkId::new("no_cache", &label),
            &(&pgzf, &targets),
            |b, &(pgzf, targets)| {
                b.iter(|| {
                    let mut reader =
                        PgzfReader::new(Cursor::new(pgzf)).unwrap().with_block_cache(0);
                    let mut buf = vec![0u8; 1024];
                    for &offset in targets {
                        reader.seek(SeekFrom::Start(offset)).unwrap();
                        reader.read_exact(&mut buf).unwrap();
                    }
                    reader
                });
            },
        );
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: repeated seek to the same block (best case for cache)
// ---------------------------------------------------------------------------

fn bench_repeated_seek(c: &mut Criterion) {
    let mut group = c.benchmark_group("repeated_seek");

    let data_mb = 50;
    let block_kb = 64;
    let total = data_mb * 1024 * 1024;
    let bs = block_kb * 1024;

    let data = generate_pattern_data(total);
    let pgzf = create_pgzf(&data, bs);

    let num_seeks = 500;
    // Always seek to the same offset
    let offset = (total / 2) as u64;

    group.bench_function("cached_500x_same_block", |b| {
        b.iter(|| {
            let mut reader = PgzfReader::new(Cursor::new(&pgzf)).unwrap().with_block_cache(64);
            let mut buf = vec![0u8; 1024];
            for _ in 0..num_seeks {
                reader.seek(SeekFrom::Start(offset)).unwrap();
                reader.read_exact(&mut buf).unwrap();
            }
            reader
        });
    });

    group.bench_function("no_cache_500x_same_block", |b| {
        b.iter(|| {
            let mut reader = PgzfReader::new(Cursor::new(&pgzf)).unwrap().with_block_cache(0);
            let mut buf = vec![0u8; 1024];
            for _ in 0..num_seeks {
                reader.seek(SeekFrom::Start(offset)).unwrap();
                reader.read_exact(&mut buf).unwrap();
            }
            reader
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: sequential read baseline (cache should have no overhead)
// ---------------------------------------------------------------------------

fn bench_sequential_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("sequential_read");

    let data_mb = 50;
    let block_kb = 64;
    let total = data_mb * 1024 * 1024;
    let bs = block_kb * 1024;

    let data = generate_pattern_data(total);
    let pgzf = create_pgzf(&data, bs);

    group.bench_function("cached", |b| {
        b.iter(|| {
            let mut reader = PgzfReader::new(Cursor::new(&pgzf)).unwrap().with_block_cache(64);
            let mut output = Vec::with_capacity(total);
            reader.read_to_end(&mut output).unwrap();
            output
        });
    });

    group.bench_function("no_cache", |b| {
        b.iter(|| {
            let mut reader = PgzfReader::new(Cursor::new(&pgzf)).unwrap().with_block_cache(0);
            let mut output = Vec::with_capacity(total);
            reader.read_to_end(&mut output).unwrap();
            output
        });
    });

    group.finish();
}

criterion_group!(benches, bench_random_seek, bench_repeated_seek, bench_sequential_read);
criterion_main!(benches);
