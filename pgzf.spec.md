# PGZF Format Specification

**Version**: 1.0
**Author**: Jue Ruan (ruanjue@gmail.com)
**Reference**: https://github.com/ruanjue/pgzf

---

## 1. Overview

PGZF (Parallel GZip Format) is a blocked compression format that extends the standard GZIP format (RFC 1952). It enables parallel compression/decompression and random access by splitting data into independently compressed blocks, organized into indexed groups.

Properties:
- Backward compatible with GZIP (RFC 1952) -- any PGZF file can be decompressed by `gzip -d`
- Each block is an independent gzip member with raw deflate compression
- Blocks are grouped, and each group carries an index for random access
- PGZF is identified by `XFL = 0xAA` in the gzip header

---

## 2. Byte Order

All multi-byte integer fields are stored in **little-endian** byte order.

---

## 3. File Layout

A PGZF file is a concatenation of one or more **groups**:

```
+---------------------------------------------------+
|                    Group 0                         |
|  [BEG Block] [DAT Block] ... [DAT Block] [IDX]    |
+---------------------------------------------------+
|                    Group 1                         |
|  [BEG Block] [DAT Block] ... [DAT Block] [IDX]    |
+---------------------------------------------------+
|                    ...                             |
+---------------------------------------------------+
|                    Group N                         |
|  [BEG Block] [DAT Block] ... [DAT Block] [IDX]    |
+---------------------------------------------------+
```

Each group contains:
- **1 BEG block** -- first block, carries group-level metadata (GC tag)
- **0 to M DAT blocks** -- regular data blocks (M is configurable, default 8000)
- **1 IDX block** -- last block, carries the index (IX tag), contains no user data

The total number of data blocks per group is 1 + M (BEG + DAT). IDX is not counted as a data block.

The last data block in the file may contain fewer uncompressed bytes than the configured block size.

---

## 4. Block Structure

Each block is a valid GZIP member as defined in RFC 1952:

```
+---------------------+
| GZIP Header         |  10 bytes fixed + variable extra field
+---------------------+
| Deflate Stream      |  raw deflate (windowBits=-15), no zlib header
+---------------------+
| GZIP Trailer        |  8 bytes: CRC32(4) + ISIZE(4)
+---------------------+
```

### 4.1 Fixed Header (10 bytes)

All blocks share the same fixed header:

| Offset | Size | Field | Value | Description |
|--------|------|-------|-------|-------------|
| 0 | 1 | ID1 | `0x1f` | GZIP magic |
| 1 | 1 | ID2 | `0x8b` | GZIP magic |
| 2 | 1 | CM | `0x08` | Deflate |
| 3 | 1 | FLG | `0x04` | FEXTRA set |
| 4 | 4 | MTIME | `0` | Modification time |
| 8 | 1 | XFL | `0xAA` | **PGZF marker** |
| 9 | 1 | OS | `0x03` | Unix |

### 4.2 Extra Field

Because FLG bit 2 (FEXTRA) is set, an extra field follows immediately:

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 10 | 2 | XLEN | Total length of extra field data (LE) |
| 12 | XLEN | DATA | Sequence of TLV sub-fields (see Section 5) |

### 4.3 Deflate Stream

Raw DEFLATE compressed data. No zlib header or wrapper. Produced by `deflateInit2(windowBits=-15)`.

### 4.4 Trailer (8 bytes)

| Offset (from end) | Size | Field | Description |
|--------------------|------|-------|-------------|
| -8 | 4 | CRC32 | CRC-32 of uncompressed data (LE) |
| -4 | 4 | ISIZE | Uncompressed size mod 2^32 (LE) |

---

## 5. Extra Field Tags (TLV)

Each sub-field in the extra field:

| Offset | Size | Field | Description |
|--------|------|-------|-------------|
| 0 | 2 | TAG | 2-byte ASCII identifier |
| 2 | 2 | SLEN | Length of VALUE in bytes (LE) |
| 4 | SLEN | VALUE | Tag-specific data |

### 5.1 ZC -- Compressed Block Size

- **TAG**: `5A 43` ("ZC")
- **SLEN**: 4
- **VALUE**: Total size of this gzip member in bytes (header + deflate + trailer), `u32` LE
- **Present in**: BEG, DAT, IDX (all blocks)
- **Purpose**: Reader can `read(ZC bytes)` to obtain the complete gzip member without parsing the deflate stream

### 5.2 GC -- Group Compressed Size

- **TAG**: `47 43` ("GC")
- **SLEN**: 8
- **VALUE**: Total compressed size of the group (from start of BEG to end of last DAT, excluding IDX), `u64` LE
- **Present in**: BEG only
- **Note**: Initially written as 0. After the group is complete, the lower 4 bytes are backpatched at file offset `BEG_offset + 24`. The upper 4 bytes remain 0, effectively limiting group compressed size to ~4 GB.

### 5.3 IX -- Block Index

- **TAG**: `49 58` ("IX")
- **SLEN**: N * 8 (where N = number of data blocks in the group, i.e., BEG + all DAT)
- **VALUE**: Array of N entries, each 8 bytes:

```
For each data block i (0..N-1):
  [0..3]  compressed_size   (u32, LE) -- size of the i-th gzip member
  [4..7]  uncompressed_size (u32, LE) -- uncompressed data size of the i-th block
```

- **Present in**: IDX only

---

## 6. Block Types

### 6.1 BEG Block (Group Begin)

First block of each group.

```
Offset  Size  Content
------  ----  -------
0       10    Fixed GZIP header (XFL=0xAA)
10      2     XLEN = 20
12      4     ZC tag: 'Z','C', SLEN=4
16      4     ZC value: member size (u32 LE)
20      8     GC tag: 'G','C', SLEN=8
24      8     GC value: group size (u64 LE, lower 4 bytes backpatched later)
32      ...   Deflate stream
...     8     Trailer: CRC32 + ISIZE
```

Header size: 32 bytes.

### 6.2 DAT Block (Data)

Regular data blocks within a group.

```
Offset  Size  Content
------  ----  -------
0       10    Fixed GZIP header (XFL=0xAA)
10      2     XLEN = 8
12      4     ZC tag: 'Z','C', SLEN=4
16      4     ZC value: member size (u32 LE)
20      ...   Deflate stream
...     8     Trailer: CRC32 + ISIZE
```

Header size: 20 bytes.

### 6.3 IDX Block (Index)

Last block of each group. Contains no user data.

```
Offset   Size   Content
------   ----   -------
0        10     Fixed GZIP header (XFL=0xAA)
10       2      XLEN = 12 + N*8
12       4      ZC tag: 'Z','C', SLEN=4
16       4      ZC value: member size (u32 LE)
20       2      IX tag: 'I','X'
22       2      IX SLEN: N*8 (u16 LE)
24       N*8    IX data: N entries of (compressed_size:u32, uncompressed_size:u32)
24+N*8   ...    Deflate stream (empty -- compresses zero bytes)
...      8      Trailer: CRC32=0, ISIZE=0
```

Header size: 24 + N*8 bytes.

XLEN breakdown: ZC subfield (2+2+4=8) + IX tag header (2+2=4) + IX data (N*8) = 12 + N*8.

---

## 7. Index Building

To support random access, a reader builds two cumulative-offset arrays by scanning all groups:

```
compressed_offsets[]:   file offset where each data block starts
uncompressed_offsets[]: uncompressed byte offset where each data block starts
```

Both arrays have B+1 entries (B = total data blocks across all groups). Entry B is the end-of-file.

Algorithm:

```
cum_c = 0, cum_u = 0
seek to file offset 0

loop:
  read BEG header at current position
  extract ZC (member size) and GC (group size)
  if GC == 0: break  // end of file

  seek to current_position + GC  // jump to IDX block
  read IDX header, extract IX tag data

  for each 8-byte entry in IX:
    push (cum_c, cum_u) into arrays
    cum_c += entry.compressed_size
    cum_u += entry.uncompressed_size

  // IX tag data ends here; ZC of the IDX block follows in the header
  read ZC from IDX header
  cum_c += ZC  // skip over the IDX block itself

  // now at start of next group
  repeat

push (cum_c, cum_u) as final entry
```

---

## 8. Seeking

### 8.1 By Byte Offset

Given target uncompressed offset T:
1. Binary search `uncompressed_offsets[]` for the largest entry <= T
2. Let block_idx = index of that entry, skip = T - uncompressed_offsets[block_idx]
3. Seek file to `compressed_offsets[block_idx]`
4. Decompress blocks sequentially, discarding the first `skip` bytes

### 8.2 By Block Index

Given target data block index I (0-based, negative counts from end):
1. Resolve negative index: if I < 0, I = B + I
2. Seek file to `compressed_offsets[I]`
3. Decompress from that position

---

## 9. Compatibility

- **PGZF is a superset of GZIP**: Every PGZF file is a valid sequence of gzip members. `gzip -d` decompresses it (each block as a separate stream).
- **PGZF reader reads GZIP**: A PGZF reader detects `XFL == 0xAA` to identify PGZF. Standard gzip files (without PGZF marker) are read sequentially without random access.
- **Detection**: Check `FLG & 0x04` (FEXTRA) and `XFL == 0xAA` in the first gzip member.

---

## 10. XLEN Summary

| Block Type | XLEN | Breakdown |
|------------|------|-----------|
| BEG | 20 | ZC(8) + GC(12) |
| DAT | 8 | ZC(8) |
| IDX | 12 + N*8 | ZC(8) + IX tag header(4) + IX data(N*8) |

Where N = number of data blocks (BEG + DAT) in the group.

---

## 11. Tag Summary

| TAG | ASCII | SLEN | Data | Present In |
|-----|-------|------|------|------------|
| ZC | `5A 43` | 4 | member size, u32 LE | BEG, DAT, IDX |
| GC | `47 43` | 8 | group size, u64 LE (lower 4 backpatched) | BEG |
| IX | `49 58` | N*8 | N * (compressed:u32, uncompressed:u32) LE | IDX |

---

## 12. Binary Example

A minimal PGZF file with 2 data blocks in one group (BEG + DAT + IDX):

```
BEG block (data block 0):
  [1f 8b] [08] [04] [00 00 00 00] [aa] [03]     ; fixed header
  [14 00]                                          ; XLEN = 20
  [5a 43] [04 00] [ZZ ZZ ZZ ZZ]                   ; ZC: member size
  [47 43] [08 00] [GG GG GG GG 00 00 00 00]       ; GC: group size (backpatched)
  [... deflate ...]                                ; compressed data
  [CC CC CC CC] [SS SS SS SS]                      ; CRC32, ISIZE

DAT block (data block 1):
  [1f 8b] [08] [04] [00 00 00 00] [aa] [03]     ; fixed header
  [08 00]                                          ; XLEN = 8
  [5a 43] [04 00] [ZZ ZZ ZZ ZZ]                   ; ZC: member size
  [... deflate ...]                                ; compressed data
  [CC CC CC CC] [SS SS SS SS]                      ; CRC32, ISIZE

IDX block (no user data):
  [1f 8b] [08] [04] [00 00 00 00] [aa] [03]     ; fixed header
  [1c 00]                                          ; XLEN = 28 (12 + 2*8)
  [5a 43] [04 00] [ZZ ZZ ZZ ZZ]                   ; ZC: member size
  [49 58] [10 00]                                  ; IX: 16 bytes (2 entries)
  [C0 C0 C0 C0] [U0 U0 U0 U0]                    ; block 0: comp_size, uncomp_size
  [C1 C1 C1 C1] [U1 U1 U1 U1]                    ; block 1: comp_size, uncomp_size
  [... empty deflate ...]                          ; zero bytes
  [00 00 00 00] [00 00 00 00]                      ; CRC32=0, ISIZE=0
```
