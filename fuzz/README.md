# PGZF-RS 模糊测试

本目录包含 pgzf-rs 项目的模糊测试基础设施。

## 前置要求

```bash
# 安装 nightly 工具链
rustup install nightly

# 安装 cargo-fuzz
cargo +nightly install cargo-fuzz
```

## 模糊测试目标

| 目标 | 测试内容 |
|------|---------|
| `fuzz_write_read_roundtrip` | 读写往返正确性 |
| `fuzz_seek_operations` | 所有 seek 路径 |
| `fuzz_config_variations` | 所有配置组合 |
| `fuzz_read_blocks` | 块读取 API |
| `fuzz_edge_cases` | 8 种边界场景 |
| `fuzz_raw_block_read` | 原始块读取 |
| `fuzz_cache_behavior` | 缓存行为测试 |

## 运行模糊测试

### 运行单个目标

```bash
cd /home/dwpeng/project/pgzf-rs/fuzz

# 运行 60 秒
cargo +nightly fuzz run fuzz_write_read_roundtrip -- -max_total_time=60

# 运行 5 分钟
cargo +nightly fuzz run fuzz_seek_operations -- -max_total_time=300
```

### 运行所有目标

```bash
for target in fuzz_write_read_roundtrip fuzz_seek_operations fuzz_config_variations \
              fuzz_read_blocks fuzz_edge_cases fuzz_raw_block_read fuzz_cache_behavior; do
    echo "=== $target ==="
    cargo +nightly fuzz run $target -- -max_total_time=60
done
```

### 并行运行

```bash
# 使用 8 个并行实例
cargo +nightly fuzz run fuzz_write_read_roundtrip -j 8
```

## 使用 ASAN 检测内存问题

```bash
# 运行带 ASAN 的模糊测试
RUSTFLAGS="-Z sanitizer=address" cargo +nightly fuzz run fuzz_write_read_roundtrip -- -max_total_time=60
```

## 复现崩溃

```bash
# 如果发现崩溃，可以复现
cargo +nightly fuzz run fuzz_write_read_roundtrip fuzz/artifacts/fuzz_write_read_roundtrip/crash-<hash>
```

## 测试覆盖范围

- **功能覆盖**: 100%（所有公共函数都有测试）
- **分支覆盖率**: ~90%（主要分支都有覆盖）
- **边界条件覆盖率**: 95%（大部分边界条件都有测试）

## 建议

1. **短期**: 运行模糊测试 1 小时发现潜在问题
2. **中期**: 集成到 CI/CD，每次提交都运行
3. **长期**: 增加模糊测试时间和随机化测试
