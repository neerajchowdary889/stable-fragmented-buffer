# `stable-fragmented-buffer` — Dependency Guide for DMXP-MPMC

## What Is It?

A high-performance, in-memory blob storage system with **pointer stability**. The primary production backend is **SharedBackend** — a cross-process arena backed by POSIX shared memory (`/dev/shm`). Multiple processes can share data with zero-copy reads and lock-free writes, coordinated entirely through atomics embedded in the mmap region.

---

## Adding as a Dependency

### Current State

In [Cargo.toml](file:///Users/neeraj/Dev/DMXP/DMXP-MPMC/Cargo.toml), `sfb` is currently a **dev-dependency** (only available in tests/examples):

```toml
[dev-dependencies]
sfb = { package = "stable-fragmented-buffer", git = "https://github.com/neerajchowdary889/stable-fragmented-buffer", branch = "main" }
```

### To Use as a Runtime Dependency

Move/add the entry under `[dependencies]`:

```toml
[dependencies]
sfb = { package = "stable-fragmented-buffer", git = "https://github.com/neerajchowdary889/stable-fragmented-buffer", branch = "main" }
```

Or, for a **local path** dependency (useful during co-development):

```toml
[dependencies]
sfb = { package = "stable-fragmented-buffer", path = "../stable-fragmented-buffer" }
```

> **Note:** SharedBackend is Unix-only (`#[cfg(unix)]`). It uses `libc` for `shm_open` / `mmap`.

---

## Imports

```rust
use sfb::{PinnedBlobStore, Config, BlobError};
use sfb::types::OverflowHandle;

// Lifecycle extension trait
use sfb::lifecycle::BlobStoreLifecycleExt;

// Metrics
use sfb::profiling::{Profiler, ProfileStats};
```

---

## Public API Reference

### [Config](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/types/types.rs#138-151) — Store Configuration

```rust
pub struct Config {
    pub page_size: usize,          // Heap page size in bytes (default: 1 MB)
    pub prefetch_threshold: f32,   // Unused in shared mode; kept for heap mode
    pub decay_timeout_ms: u64,     // How long fully-acked chunks survive before recycling (default: 5000 ms)
    pub default_ttl_ms: u64,       // Handle TTL before expiry (default: 30 000 ms)
}
```

| Constructor | Page Size | Decay | TTL |
|---|---|---|---|
| `Config::default()` | 1 MB | 5 s | 30 s |
| `Config::performance()` | 2 MB | 7 s | 30 s |
| `Config::memory_efficient()` | 512 KB | 1 s | 30 s |

---

### [PinnedBlobStore](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/page/store.rs#19-43) — The Core Store (Shared Mode)

#### Construction

| Method | Signature | Description |
|---|---|---|
| [new_shared](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/page/store.rs#78-91) | `fn new_shared(config, namespace, chunk_size) -> Result<Self>` | Create the shared arena (creator process). Allocates `/dev/shm/{ns}_ctrl` and the first data chunk. |
| [attach_shared](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/page/store.rs#95-108) | `fn attach_shared(config, namespace) -> Result<Self>` | Attach to an existing arena (non-creator processes). Reads chunk size from the control file and eagerly maps all existing chunks. |

#### Shared-Mode Operations

| Method | Signature | Description |
|---|---|---|
| [append_shared](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/page/store.rs#472-477) | `fn append_shared(&self, data: &[u8]) -> Result<OverflowHandle>` | Write data to the active chunk via CAS loop. Returns a 24-byte `OverflowHandle`. Automatically advances to a new chunk when the active one is full. |
| [resolve](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/page/store.rs#486-490) | `fn resolve(&self, handle: &OverflowHandle) -> Option<&[u8]>` | Zero-copy read. Returns a `&[u8]` pointing directly into the mmap region. Returns `None` if expired or generation mismatches. |
| [acknowledge_shared](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/page/store.rs#493-498) | `fn acknowledge_shared(&self, handle: &OverflowHandle) -> bool` | Increment `ack_count` for the chunk. When `ack_count >= entry_count`, records `empty_since` timestamp for later recycling. |
| [cleanup_shared](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/page/store.rs#501-506) | `fn cleanup_shared(&self) -> usize` | Sweep all non-active chunks. Recycle those that are fully acknowledged and have exceeded `decay_timeout_ms`. Returns number of chunks recycled. |
| [is_shared](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/page/store.rs#459-461) | `fn is_shared(&self) -> bool` | Returns `true` if this store was opened in shared mode. |
| [debug_chunks](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/page/store.rs#509-515) | `fn debug_chunks(&self)` | Print chunk state (used/entries/acked/generation) to stderr. |

> **Important:** `PinnedBlobStore` is `Send + Sync`. Wrap in `Arc` for multi-threaded use. The creator process is responsible for cleanup — on `Drop`, it calls `shm_unlink` on all `/dev/shm` files. Attachers simply `munmap` and `close` without unlinking.

---

### [OverflowHandle](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/types/overflow_handle.rs#13-24) — Cross-Process Data Reference (24 bytes)

Returned by `append_shared()`. ABI-stable (`#[repr(C)]`), suitable for embedding in ring-buffer slots.

```rust
#[repr(C)]
pub struct OverflowHandle {
    pub page_id:    u32,  // Chunk index → /dev/shm/{ns}_data_{page_id}
    pub offset:     u32,  // Byte offset within the chunk's data region (after 64-byte header)
    pub size:       u32,  // Data length in bytes
    pub generation: u32,  // ABA-prevention counter
    pub timestamp:  u64,  // Creation time (ms since UNIX epoch)
}
```

| Method | Description |
|---|---|
| `.is_expired(ttl_ms)` | Returns `true` if `now - timestamp > ttl_ms` |
| `.age_ms()` | Current age in milliseconds |
| `.as_bytes()` | Zero-copy `&[u8]` view (for embedding in payloads) |
| `OverflowHandle::from_bytes(&[u8])` | Deserialise from a byte slice (returns `None` if wrong length) |

> `OverflowHandle` derives `Debug, Clone, Copy, PartialEq, Eq, Hash` — safe to send through channels or store in maps.

---

### `BlobError` — Error Type

```rust
pub enum BlobError {
    HandleExpired,                           // TTL exceeded
    InvalidHandle,                           // Generation mismatch, bad magic, or wrong version
    OutOfMemory,                             // shm_open / mmap / ftruncate failed
    DataTooLarge { size: usize, max: usize },// Data exceeds chunk capacity
    PageFull,                                // Internal: current chunk full (triggers chunk advance)
}
```

The crate also provides `pub type Result<T> = std::result::Result<T, BlobError>;`

---

### [LifecycleManager](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/lifecycle/lifecycle.rs#11-14) — Background Cleanup

```rust
// Manual cleanup pass
let manager = LifecycleManager::new(&store_arc);
let recycled = manager.maintenance_cycle(); // calls cleanup_shared internally

// Or spawn a background thread
manager.start_background_cleanup(Duration::from_millis(100));
```

#### Extension Trait Shortcut

```rust
use sfb::lifecycle::BlobStoreLifecycleExt;

let store = Arc::new(PinnedBlobStore::new_shared(
    Config::default(), "dmxp", DEFAULT_CHUNK_SIZE
)?);
store.start_cleanup(Duration::from_millis(100)); // auto-stops when store drops
```

---

### [Profiler](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/profiling/mod.rs#63-66) / [ProfileStats](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/profiling/mod.rs#13-39) — Metrics

```rust
let stats: ProfileStats = store.profiler().stats();

println!("Appends: {}", stats.total_appends);
println!("Bytes written: {}", stats.total_bytes_written);
println!("Active pages: {}", stats.active_pages);
println!("Fragmentation: {:.1}%", stats.fragmentation_ratio * 100.0);
```

---

## Usage Patterns for DMXP-MPMC

### Creator Process

```rust
use sfb::{PinnedBlobStore, Config};
use sfb::backend::shared::DEFAULT_CHUNK_SIZE;

// One process creates the shared arena
let store = PinnedBlobStore::new_shared(
    Config::default(),
    "dmxp",           // namespace → /dev/shm/dmxp_ctrl, /dev/shm/dmxp_data_0, ...
    DEFAULT_CHUNK_SIZE, // 32 MB per chunk
)?;

let handle = store.append_shared(b"large payload")?;
// Send `handle` (24 bytes) to other processes via a ring buffer, pipe, etc.

store.acknowledge_shared(&handle);
// Background cleanup (or call store.cleanup_shared() manually)
```

### Attacher Process

```rust
use sfb::{PinnedBlobStore, Config};

// Any number of processes can attach
let store = PinnedBlobStore::attach_shared(Config::default(), "dmxp")?;

// Receive OverflowHandle from ring buffer / pipe / etc.
// let handle: OverflowHandle = ...;

// Zero-copy read — points directly into the mmap region
if let Some(data) = store.resolve(&handle) {
    process(data); // data is &[u8] pointing into /dev/shm
    store.acknowledge_shared(&handle);
}
```

### Multi-Threaded Producer-Consumer (Shared Mode)

```rust
use sfb::{PinnedBlobStore, Config};
use sfb::backend::shared::DEFAULT_CHUNK_SIZE;
use sfb::types::OverflowHandle;
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
        // process data — zero-copy mmap slice
        cons.acknowledge_shared(&handle);
    }
});
```

### With Background Cleanup (Recommended for Long-Running)

```rust
use sfb::{PinnedBlobStore, Config};
use sfb::backend::shared::DEFAULT_CHUNK_SIZE;
use sfb::lifecycle::BlobStoreLifecycleExt;
use std::sync::Arc;
use std::time::Duration;

let store = Arc::new(PinnedBlobStore::new_shared(
    Config::performance(),
    "dmxp",
    DEFAULT_CHUNK_SIZE,
)?);

// Recycles fully-acknowledged chunks every 100 ms
store.start_cleanup(Duration::from_millis(100));

let handle = store.append_shared(b"data")?;
```

---

## Architecture at a Glance

```
┌─────────────────────────────────────────────┐
│        PinnedBlobStore (Public API)          │
│  append_shared() → OverflowHandle           │
│  resolve() → &[u8]  (zero-copy mmap)        │
│  acknowledge_shared()   cleanup_shared()    │
├─────────────────────────────────────────────┤
│       LifecycleManager ("Elastic Brain")    │
│  Scale-up: new chunk on write-head overflow │
│  Scale-down: recycle after TTL + decay      │
├─────────────────────────────────────────────┤
│         SharedBackend (/dev/shm)            │
│  Control file: magic · write_head · gen     │
│  Data chunks: 64-byte header + data region  │
│  Lock-free CAS append · atomic ack counts  │
│  Chunk recycling via generation counters    │
└─────────────────────────────────────────────┘
```

**Key design properties:**
- All cross-process synchronisation via atomics in the mmap region — no OS IPC on the hot path
- `OverflowHandle` is `#[repr(C)]` — embed directly in ring-buffer slot payloads
- Generation counters prevent ABA problems when chunks are recycled
- Creator process unlinks all `/dev/shm` files on `Drop`; attachers only `munmap`
- Chunk recycling reuses existing files before allocating new ones, keeping `/dev/shm` usage compact
