# `stable-fragmented-buffer` — Dependency Guide for DMXP-MPMC

## What Is It?

A high-performance, in-memory blob storage system with **pointer stability**. The primary production backend is **SharedBackend** — a cross-process arena backed by POSIX shared memory (`/dev/shm`). Multiple processes can share data via lock-free writes coordinated entirely through atomics embedded in the mmap region.

Key properties:
- **Lock-free appends** via CAS on an atomic `used` counter
- **Proactive prefetch** pre-allocates next chunk at 80% usage (eliminates syscall latency on overflow)
- **Dual cleanup** — ack-based (normal) + TTL-based (consumer crash recovery)
- **Real file cleanup** — freed chunks are `shm_unlink`'d (not just header-reset), releasing tmpfs memory
- **Backpressure** via `max_chunks` limit prevents `/dev/shm` exhaustion
- **Crash recovery** via `SharedBackend::cleanup_namespace()` at startup

---

## Adding as a Dependency

In [Cargo.toml](file:///Users/neeraj/Dev/DMXP/DMXP-MPMC/Cargo.toml):

```toml
[dependencies]
sfb = { package = "stable-fragmented-buffer", git = "https://github.com/neerajchowdary889/stable-fragmented-buffer", branch = "main" }
```

Or for local co-development:

```toml
[dependencies]
sfb = { package = "stable-fragmented-buffer", path = "../stable-fragmented-buffer" }
```

> **Note:** SharedBackend is Unix-only (`#[cfg(unix)]`). Uses `libc` for `shm_open` / `mmap`.

---

## Imports

```rust
use sfb::{PinnedBlobStore, Config, BlobError, OverflowHandle};
use sfb::backend::shared::{SharedBackend, DEFAULT_CHUNK_SIZE};
use sfb::lifecycle::BlobStoreLifecycleExt;
use sfb::profiling::{Profiler, ProfileStats};
```

---

## Public API Reference

### Config — Store Configuration

```rust
pub struct Config {
    pub page_size: usize,          // Heap page size (default: 1 MB)
    pub prefetch_threshold: f32,   // Pre-alloc next chunk at this usage (default: 0.8 = 80%)
    pub decay_timeout_ms: u64,     // Grace period before freeing acked chunks (default: 5000 ms)
    pub default_ttl_ms: u64,       // Auto-expire all data after this TTL (default: 30000 ms)
}
```

| Constructor | Page Size | Prefetch | Decay | TTL |
|---|---|---|---|---|
| `Config::default()` | 1 MB | 80% | 5 s | 30 s |
| `Config::performance()` | 2 MB | 80% | 7 s | 30 s |
| `Config::memory_efficient()` | 512 KB | 90% | 1 s | 30 s |

---

### PinnedBlobStore — The Core Store (Shared Mode)

#### Construction

| Method | Signature | Description |
|---|---|---|
| `new_shared` | `fn new_shared(config, namespace, chunk_size) -> Result<Self>` | Create the shared arena (creator process). Allocates control file + chunk 0. |
| `new_shared_with_limit` | `fn new_shared_with_limit(config, namespace, chunk_size, max_chunks) -> Result<Self>` | Same as above with a `max_chunks` backpressure limit. Returns `OutOfMemory` when exhausted. |
| `attach_shared` | `fn attach_shared(config, namespace) -> Result<Self>` | Attach to an existing arena (non-creator). Reads chunk size from control file, eagerly maps all chunks. |

#### Shared-Mode Operations

| Method | Signature | Time | Description |
|---|---|---|---|
| `append_shared` | `fn append_shared(&self, data: &[u8]) -> Result<OverflowHandle>` | O(1) amortised | CAS-loop write. Returns 24-byte handle. Triggers prefetch at 80%. |
| `resolve` | `fn resolve(&self, handle: &OverflowHandle) -> Option<Vec<u8>>` | O(data_size) | Copies data out of mmap. Returns `None` if expired/recycled. Post-copy generation recheck prevents stale reads. |
| `acknowledge_shared` | `fn acknowledge_shared(&self, handle: &OverflowHandle) -> bool` | O(1) | Atomic `fetch_add` on ack counter. Stamps `empty_since` when fully acked. |
| `cleanup_shared` | `fn cleanup_shared(&self) -> usize` | O(chunks) | Sweeps all non-active chunks. Frees those that are fully acked + decayed OR TTL-expired. Calls `shm_unlink` on freed chunks. |
| `is_shared` | `fn is_shared(&self) -> bool` | O(1) | Returns `true` if in shared mode. |
| `debug_chunks` | `fn debug_chunks(&self)` | O(chunks) | Prints chunk state to stderr. |

#### Crash Recovery (Static Method)

| Method | Signature | Description |
|---|---|---|
| `SharedBackend::cleanup_namespace` | `fn cleanup_namespace(namespace: &str) -> Result<()>` | Unlinks all `/dev/shm` files for a namespace. Call at startup to clean up after a previous crash. |

> **Important:** `PinnedBlobStore` is `Send + Sync`. Wrap in `Arc` for multi-threaded use. The **creator** process unlinks all shm files on `Drop`. **Attachers** only `munmap` without unlinking.

---

### OverflowHandle — Cross-Process Data Reference (24 bytes)

Returned by `append_shared()`. ABI-stable (`#[repr(C)]`), safe to embed in ring-buffer slot payloads.

```rust
#[repr(C)]
pub struct OverflowHandle {
    pub page_id:    u32,  // Chunk index -> /dev/shm/{ns}_data_{page_id}
    pub offset:     u32,  // Byte offset within data region (after 64-byte header)
    pub size:       u32,  // Data length in bytes
    pub generation: u32,  // ABA-prevention counter
    pub timestamp:  u64,  // Creation time (ms since UNIX epoch)
}
```

| Method | Time | Description |
|---|---|---|
| `.is_expired(ttl_ms)` | O(1) | Returns `true` if `now - timestamp > ttl_ms` |
| `.age_ms()` | O(1) | Current age in milliseconds |
| `.as_bytes()` | O(1) | Zero-copy `&[u8]` view (24 bytes, for embedding in payloads) |
| `::from_bytes(&[u8])` | O(1) | Deserialise from 24 bytes (returns `None` if wrong length) |

> Derives `Debug, Clone, Copy, PartialEq, Eq, Hash` — safe to send through channels or store in maps.

---

### BlobError — Error Type

```rust
pub enum BlobError {
    HandleExpired,                            // TTL exceeded
    InvalidHandle,                            // Generation mismatch, bad magic, wrong version, or shared mode required
    OutOfMemory,                              // shm_open/mmap failed, or max_chunks reached
    DataTooLarge { size: usize, max: usize }, // Data exceeds chunk capacity, or invalid chunk_size
    PageFull,                                 // Internal: current chunk full (triggers chunk advance)
}
```

---

### LifecycleManager — Background Cleanup

```rust
use sfb::lifecycle::BlobStoreLifecycleExt;

let store = Arc::new(PinnedBlobStore::new_shared(
    Config::default(), "dmxp", DEFAULT_CHUNK_SIZE
)?);

// Background thread: runs cleanup every 100ms, stops when store drops
store.start_cleanup(Duration::from_millis(100));
```

Each `maintenance_cycle()` calls both:
- `cleanup_acknowledged()` — heap page cleanup
- `cleanup_shared()` — shared chunk cleanup (ack-based + TTL-based)

The background thread holds a `Weak<PinnedBlobStore>` and exits when `upgrade()` returns `None` (store dropped).

#### Cleanup Decision Flow

For each non-active chunk:
1. **Ack path**: `ack_count >= entry_count` AND `now - empty_since >= decay_timeout_ms` -> free
2. **TTL path**: `now - first_write_ts > default_ttl_ms` -> free (regardless of ack state)

"Free" means: remove from `BTreeMap`, `munmap`, `close fd`, and `shm_unlink` (truly frees tmpfs memory).

---

### Profiler / ProfileStats — Metrics

```rust
let stats: ProfileStats = store.profiler().stats();
println!("Appends: {}, Bytes: {}, Active pages: {}, Fragmentation: {:.1}%",
    stats.total_appends, stats.total_bytes_written,
    stats.active_pages, stats.fragmentation_ratio * 100.0);
```

Tracked: pages allocated/freed, appends/reads/cleanups, bytes written/read/discarded, capacity, fragmentation ratio, uptime.

---

## Usage Patterns for DMXP-MPMC

### Creator Process

```rust
use sfb::{PinnedBlobStore, Config};
use sfb::backend::shared::DEFAULT_CHUNK_SIZE;

// Clean up any orphaned files from a previous crash
sfb::SharedBackend::cleanup_namespace("dmxp").ok();

let store = PinnedBlobStore::new_shared(
    Config::default(),
    "dmxp",              // namespace -> /dev/shm/dmxp_ctrl, /dev/shm/dmxp_data_0, ...
    DEFAULT_CHUNK_SIZE,  // 32 MB per chunk
)?;

let handle = store.append_shared(b"large payload")?;
// Send `handle` (24 bytes) to consumers via ring buffer, pipe, channel, etc.
```

### Attacher Process

```rust
use sfb::{PinnedBlobStore, Config};

let store = PinnedBlobStore::attach_shared(Config::default(), "dmxp")?;

// Receive OverflowHandle from ring buffer / pipe / etc.
if let Some(data) = store.resolve(&handle) {
    process(&data);
    store.acknowledge_shared(&handle);
}
```

### Multi-Threaded Producer-Consumer

```rust
use sfb::{PinnedBlobStore, Config, OverflowHandle};
use sfb::backend::shared::DEFAULT_CHUNK_SIZE;
use std::sync::{Arc, mpsc};

let store = Arc::new(PinnedBlobStore::new_shared(
    Config::default(), "dmxp", DEFAULT_CHUNK_SIZE
)?);
let (tx, rx) = mpsc::channel::<OverflowHandle>();

// Producer thread
let prod = Arc::clone(&store);
std::thread::spawn(move || {
    let handle = prod.append_shared(b"payload").unwrap();
    tx.send(handle).unwrap(); // send 24-byte handle, not the data
});

// Consumer thread
let cons = Arc::clone(&store);
std::thread::spawn(move || {
    let handle = rx.recv().unwrap();
    if let Some(data) = cons.resolve(&handle) {
        // data is Vec<u8> — safe even if chunk is recycled later
        cons.acknowledge_shared(&handle);
    }
});
```

### With Background Cleanup + Backpressure (Recommended)

```rust
use sfb::{PinnedBlobStore, Config, BlobStoreLifecycleExt};
use sfb::backend::shared::DEFAULT_CHUNK_SIZE;
use std::sync::Arc;
use std::time::Duration;

let store = Arc::new(PinnedBlobStore::new_shared_with_limit(
    Config::performance(),
    "dmxp",
    DEFAULT_CHUNK_SIZE,
    Some(512),  // max 512 chunks = 16 GB at 32 MB each
)?);

// Recycles acked + TTL-expired chunks every 100 ms
store.start_cleanup(Duration::from_millis(100));

let handle = store.append_shared(b"data")?;
```

---

## Memory Layout

### Control File (`/dev/shm/{ns}_ctrl`, 128 bytes)

| Offset | Field | Size | Type | Description |
|--------|-------|------|------|-------------|
| 0 | `magic` | 8 | u64 | `0x444D58505F4F5646` ("DMXP_OVF") |
| 8 | `version` | 4 | u32 | Protocol version (1) |
| 12 | `chunk_size` | 4 | u32 | Bytes per data chunk |
| 16 | `write_head` | 4 | AtomicU32 | Active chunk ID for appends |
| 20 | `chunk_count` | 4 | AtomicU32 | Highest allocated chunk ID + 1 |
| 24 | `generation` | 4 | AtomicU32 | Global generation counter |
| 28 | _(reserved)_ | 100 | - | Pad to 128 bytes |

### Chunk Header (first 64 bytes of each `/dev/shm/{ns}_data_N`)

| Offset | Field | Size | Type | Description |
|--------|-------|------|------|-------------|
| 0 | `used` | 4 | AtomicU32 | Bytes written (CAS target for append) |
| 4 | `generation` | 4 | AtomicU32 | Recycling generation (ABA prevention) |
| 8 | `entry_count` | 4 | AtomicU32 | Total entries appended to this chunk |
| 12 | `ack_count` | 4 | AtomicU32 | Entries acknowledged by consumers |
| 16 | `empty_since` | 8 | AtomicU64 | Timestamp when `ack_count` reached `entry_count` |
| 24 | `first_write_ts` | 8 | AtomicU64 | Timestamp of first append (for TTL expiry) |
| 32 | _(reserved)_ | 32 | - | Pad to 64 bytes |

Data region starts at byte 64. Usable capacity = `chunk_size - 64`.

---

## Capacity Limits

| Constraint | Limit | Notes |
|-----------|-------|-------|
| Max chunks | u32::MAX - 1 (~4 billion) | Or `max_chunks` config |
| Max chunk size | 4 GB (u32::MAX) | Stored as u32 in control file |
| Max single append | chunk_size - 64 bytes | Shared mode doesn't span chunks |
| Default capacity/chunk | ~33.5 MB | 32 MB - 64B header |
| Practical limit | 50% of RAM | `/dev/shm` is tmpfs on Linux |

---

## Performance (Apple M-series, --release)

| Operation | Throughput | p50 | p90 | p99 | max |
|-----------|-----------|-----|-----|-----|-----|
| `append_shared` | 9.2 GB/s | 0.2 us | 0.9 us | 2.0 us | 20.9 us |
| `resolve` | 8.3 GB/s | 0.4 us | 0.5 us | 1.5 us | 2.9 us |
| `acknowledge_shared` | - | 0.0 us | 0.0 us | 0.0 us | 0.5 us |

---

## Architecture Diagram

```
┌─────────────────────────────────────────────────────┐
│           PinnedBlobStore (Public API)               │
│  append_shared() -> OverflowHandle                  │
│  resolve() -> Option<Vec<u8>>                       │
│  acknowledge_shared()    cleanup_shared()           │
├─────────────────────────────────────────────────────┤
│        LifecycleManager ("Elastic Brain")           │
│                                                     │
│  SCALE UP:                                          │
│    append() at 80% usage -> pre-alloc next chunk    │
│    (shm_open + mmap absorbed before overflow)       │
│                                                     │
│  SCALE DOWN (two paths):                            │
│    Path A: ack_count >= entries + decay elapsed      │
│            -> shm_unlink + munmap (real free)       │
│    Path B: now - first_write_ts > ttl_ms            │
│            -> shm_unlink + munmap (crash recovery)  │
├─────────────────────────────────────────────────────┤
│           SharedBackend (/dev/shm)                  │
│                                                     │
│  /dev/shm/{ns}_ctrl     128B control file           │
│    write_head, chunk_count, generation (atomics)    │
│                                                     │
│  /dev/shm/{ns}_data_N   32MB data chunks            │
│    64B header: used, gen, entries, acks, timestamps  │
│    Data region: raw bytes, CAS-reserved             │
│                                                     │
│  All sync via atomics in mmap — no OS IPC           │
│  Chunk ID allocation via CAS on chunk_count         │
│  ABA prevention via generation counters             │
└─────────────────────────────────────────────────────┘
```
