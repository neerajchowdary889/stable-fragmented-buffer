# stable-fragmented-buffer

A high-performance, in-memory blob storage system for Rust with **pointer stability** and **cross-process shared memory** support.

Unlike `Vec`, which reallocates and moves data on growth, this library chains fixed-size memory chunks — once data is written, its address never changes.

## Features

- **Pointer Stability** — data never moves after being written
- **Cross-Process IPC** — shared memory via `/dev/shm` (POSIX `shm_open` + `mmap`)
- **Lock-Free Writes** — CAS loop on atomic counters, no mutexes on the hot path
- **Elastic Lifecycle** — proactive prefetch at 80% capacity, lazy decay on scale-down
- **TTL-Based Expiry** — automatically reclaims chunks even if consumers crash
- **Backpressure** — configurable `max_chunks` limit prevents `/dev/shm` exhaustion
- **Crash Recovery** — `cleanup_namespace()` removes orphaned shm files at startup

## Quick Start (Shared Mode)

```rust
use stable_fragmented_buffer::{PinnedBlobStore, Config, BlobStoreLifecycleExt};
use stable_fragmented_buffer::backend::shared::DEFAULT_CHUNK_SIZE;
use std::sync::Arc;
use std::time::Duration;

// Creator process
let store = Arc::new(PinnedBlobStore::new_shared(
    Config::default(),
    "myapp",            // namespace -> /dev/shm/myapp_ctrl, /dev/shm/myapp_data_0, ...
    DEFAULT_CHUNK_SIZE, // 32 MB per chunk
)?);

// Start background cleanup (recycles acked + TTL-expired chunks)
store.start_cleanup(Duration::from_millis(100));

// Write data — returns a 24-byte handle (not a pointer)
let handle = store.append_shared(b"hello world")?;

// Read data — returns an owned copy safe from concurrent cleanup
let data = store.resolve(&handle).unwrap();
assert_eq!(data, b"hello world");

// Mark as consumed — enables cleanup
store.acknowledge_shared(&handle);
```

```rust
// Attacher process (any number of processes can attach)
let store = PinnedBlobStore::attach_shared(Config::default(), "myapp")?;

// Resolve the same handle (received via ring buffer, pipe, etc.)
if let Some(data) = store.resolve(&handle) {
    process(&data);
    store.acknowledge_shared(&handle);
}
```

## Architecture

```
┌─────────────────────────────────────────────────────┐
│           PinnedBlobStore (Public API)               │
│  append_shared() -> OverflowHandle                  │
│  resolve() -> Option<Vec<u8>>                       │
│  acknowledge_shared()    cleanup_shared()           │
├─────────────────────────────────────────────────────┤
│        LifecycleManager ("Elastic Brain")           │
│  Prefetch: pre-alloc next chunk at 80% usage        │
│  Ack cleanup: free when all entries acked + decay   │
│  TTL expiry: free when first_write_ts > ttl_ms      │
│  Real cleanup: shm_unlink + munmap (frees tmpfs)    │
├─────────────────────────────────────────────────────┤
│           SharedBackend (/dev/shm)                  │
│  Control file (128B): magic, write_head, chunk_count│
│  Data chunks (default 32MB each):                   │
│    64B header: used, gen, entries, acks, timestamps  │
│    Data region: raw bytes, CAS-reserved             │
│  Sync: all via atomics in mmap (no OS IPC)          │
└─────────────────────────────────────────────────────┘
```

### How Cleanup Works

A chunk is freed (unlinked from `/dev/shm`) when **either**:

1. **All entries acknowledged** (`ack_count >= entry_count`) **AND** the `decay_timeout_ms` grace period has elapsed — this is the normal path.

2. **TTL expired** (`now - first_write_ts > default_ttl_ms`) regardless of acknowledgement state — this handles the case where a consumer crashes and never acks. Without this, the chunk would be stuck forever.

Freed chunks are truly removed: `shm_unlink` is called (freeing tmpfs memory), the mmap is unmapped, and the chunk is removed from the in-memory `BTreeMap`. New data is written to freshly allocated chunks.

### Proactive Prefetch

When an `append_shared()` pushes a chunk past 80% capacity, the **next** chunk is pre-allocated (`shm_open` + `ftruncate` + `mmap`) in the background. This means the writer that eventually overflows finds the chunk already mapped — no syscall on the hot path.

Benchmark impact: max append latency dropped from **103us to 20us**, throughput from **4.2 GB/s to 9.2 GB/s**.

### Crash Recovery

If the creator process is killed (SIGKILL, OOM), `/dev/shm` files are orphaned because `Drop` never runs. Call this at application startup:

```rust
use stable_fragmented_buffer::SharedBackend;

// Unlinks all /dev/shm files for this namespace (safe if they don't exist)
SharedBackend::cleanup_namespace("myapp").unwrap();
```

## Memory Layout

### Control File (`/dev/shm/{ns}_ctrl`, 128 bytes)

| Offset | Field | Size | Type | Description |
|--------|-------|------|------|-------------|
| 0 | `magic` | 8 | u64 | `0x444D58505F4F5646` ("DMXP_OVF") |
| 8 | `version` | 4 | u32 | Protocol version (1) |
| 12 | `chunk_size` | 4 | u32 | Bytes per data chunk |
| 16 | `write_head` | 4 | AtomicU32 | Active chunk ID for appends |
| 20 | `chunk_count` | 4 | AtomicU32 | Highest allocated chunk + 1 |
| 24 | `generation` | 4 | AtomicU32 | Global generation counter |
| 28 | _(reserved)_ | 100 | - | Pad to 128 bytes |

### Chunk Header (first 64 bytes of each `/dev/shm/{ns}_data_N`)

| Offset | Field | Size | Type | Description |
|--------|-------|------|------|-------------|
| 0 | `used` | 4 | AtomicU32 | Bytes written (CAS target for append) |
| 4 | `generation` | 4 | AtomicU32 | Recycling generation (ABA prevention) |
| 8 | `entry_count` | 4 | AtomicU32 | Total entries appended |
| 12 | `ack_count` | 4 | AtomicU32 | Acknowledged entries |
| 16 | `empty_since` | 8 | AtomicU64 | Timestamp when all entries were acked |
| 24 | `first_write_ts` | 8 | AtomicU64 | Timestamp of first append (for TTL expiry) |
| 32 | _(reserved)_ | 32 | - | Pad to 64 bytes |

Data region starts at byte 64.

## Configuration

```rust
pub struct Config {
    pub page_size: usize,        // Heap page size (default: 1 MB)
    pub prefetch_threshold: f32, // Pre-alloc next chunk at this usage (default: 0.8)
    pub decay_timeout_ms: u64,   // Grace period before freeing acked chunks (default: 5000)
    pub default_ttl_ms: u64,     // Auto-expire unacked data after this (default: 30000)
}
```

| Preset | Page Size | Prefetch | Decay | TTL |
|--------|-----------|----------|-------|-----|
| `Config::default()` | 1 MB | 80% | 5 s | 30 s |
| `Config::performance()` | 2 MB | 80% | 7 s | 30 s |
| `Config::memory_efficient()` | 512 KB | 90% | 1 s | 30 s |

SharedBackend-specific: `chunk_size` is passed to `new_shared()` (default 32 MB). `max_chunks` is passed to `new_shared_with_limit()` (default unlimited).

## Capacity Limits

| Constraint | Limit | Notes |
|-----------|-------|-------|
| Max chunks | u32::MAX - 1 (~4B) | Or `max_chunks` config |
| Max chunk size | 4 GB (u32::MAX) | Stored as u32 in control file |
| Max single append | chunk_size - 64 bytes | Shared mode doesn't span chunks |
| Default data per chunk | ~33.5 million bytes | 32 MB - 64B header |
| Practical limit | 50% of RAM | `/dev/shm` is tmpfs on Linux |

## Tests

33 unit tests + 2 doc-tests covering:
- Basic CRUD, multi-chunk overflow, cross-process attach
- Ack-based cleanup, partial ack safety, decay timeout
- TTL-based expiry (consumer crash scenario)
- Proactive prefetch triggering and latency reduction
- Real `/dev/shm` file unlink verification
- Background lifecycle thread integration
- Concurrent append + cleanup safety
- Active chunk corruption protection

```bash
cargo test                              # All tests
cargo test lifecycle -- --nocapture     # Lifecycle tests with step-by-step trace
```

## Performance (Apple M-series, release mode)

| Operation | Throughput | p50 | p99 | max |
|-----------|-----------|-----|-----|-----|
| `append_shared` | 9.2 GB/s | 0.2 us | 2.0 us | 20.9 us |
| `resolve` | 8.3 GB/s | 0.4 us | 1.5 us | 2.9 us |
| `acknowledge_shared` | - | 0.0 us | 0.0 us | 0.5 us |
