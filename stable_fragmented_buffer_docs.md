# `stable-fragmented-buffer` — Dependency Guide for DMXP-MPMC

## What Is It?

A high-performance, in-memory blob storage system with **pointer stability**. Unlike `Vec`, it grows by chaining fixed-size pages — once data is written, its address never changes. Designed for lock-free appends, TTL-based cleanup, and elastic memory scaling with background page recycling.

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

---

## Imports

All public types are re-exported from the crate root:

```rust
use sfb::{PinnedBlobStore, Config, BlobHandle, BlobError, BlobStats};
use sfb::{LifecycleManager};
use sfb::profiling::{Profiler, ProfileStats};

// Extension trait for Arc<PinnedBlobStore>::start_cleanup()
use sfb::lifecycle::BlobStoreLifecycleExt;
```

---

## Public API Reference

### [Config](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/types/types.rs#138-151) — Store Configuration

```rust
pub struct Config {
    pub page_size: usize,          // Size of each page in bytes (default: 1 MB)
    pub prefetch_threshold: f32,   // When to prefetch next page (default: 0.8 = 80%)
    pub decay_timeout_ms: u64,     // How long empty pages survive before freeing (default: 5000 ms)
    pub default_ttl_ms: u64,       // Data TTL before handle expiry (default: 30000 ms)
}
```

| Constructor | Page Size | Prefetch | Decay | TTL |
|---|---|---|---|---|
| `Config::default()` | 1 MB | 80% | 5 s | 30 s |
| `Config::performance()` | 2 MB | 80% | 7 s | 30 s |
| `Config::memory_efficient()` | 512 KB | 90% | 1 s | 30 s |

You can also construct a custom [Config](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/types/types.rs#138-151) directly using struct literal syntax with `..Default::default()`.

---

### [PinnedBlobStore](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/page/store.rs#13-35) — The Core Store

| Method | Signature | Description |
|---|---|---|
| [new](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/backend/segmented.rs#14-20) | `fn new(config: Config) -> Result<Self>` | Create a store with custom config |
| [with_defaults](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/page/store.rs#58-62) | `fn with_defaults() -> Result<Self>` | Create with `Config::default()` |
| [append](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/page/store.rs#97-171) | `fn append(&self, data: &[u8]) -> Result<BlobHandle>` | Write data, get a handle back. Auto-spans pages for large blobs |
| [get](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/page/store.rs#231-266) | `fn get(&self, handle: &BlobHandle) -> Option<Vec<u8>>` | Read data by handle. Returns `None` if expired or invalid |
| [acknowledge](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/page/store.rs#312-322) | `fn acknowledge(&self, handle: &BlobHandle) -> bool` | Mark data as consumed (enables cleanup) |
| [cleanup_acknowledged](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/page/store.rs#323-392) | `fn cleanup_acknowledged(&self) -> usize` | Free pages with all-acknowledged/expired entries. Returns pages freed |
| [stats](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/page/store.rs#398-409) | `fn stats(&self) -> BlobStats` | Get `{ page_count, current_page_id }` |
| [profiler](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/page/store.rs#393-397) | `fn profiler(&self) -> &Profiler` | Access the profiler for detailed metrics |

> [!IMPORTANT]
> [PinnedBlobStore](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/page/store.rs#13-35) is `Send + Sync`. Wrap in `Arc` for multi-threaded use.

---

### [BlobHandle](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/types/types.rs#8-30) — Data Reference (32 bytes)

Returned by [append()](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/page/store.rs#97-171). A lightweight, `Copy`-able handle to stored data.

| Getter | Returns | Description |
|---|---|---|
| `.page_id()` | `u32` | Starting page ID |
| `.offset()` | `u32` | Byte offset within starting page |
| `.size()` | `u32` | Data size (single-page compatible) |
| `.total_size()` | `u64` | Total size across all pages |
| `.end_page_id()` | `u32` | Ending page ID (same as [page_id](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/types/types.rs#99-104) for single-page) |
| `.generation()` | `u32` | ABA-prevention generation counter |
| `.timestamp()` | `u64` | Creation time (ms since epoch) |
| `.age_ms()` | `u64` | Current age in milliseconds |

> [!NOTE]
> [BlobHandle](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/types/types.rs#8-30) derives `Debug, Clone, Copy, PartialEq, Eq, Hash` — safe to send through channels, store in maps, etc.

---

### `BlobError` — Error Type

```rust
pub enum BlobError {
    HandleExpired,                          // TTL exceeded
    InvalidHandle,                          // Generation mismatch or bad page ID
    OutOfMemory,                            // Page allocation failed
    DataTooLarge { size: usize, max: usize }, // Data exceeds page size (single-page path)
    PageFull,                               // Current page is full
}
```

The crate also provides `pub type Result<T> = std::result::Result<T, BlobError>;`

---

### [LifecycleManager](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/lifecycle/lifecycle.rs#11-14) — Background Cleanup

```rust
// Manual lifecycle management
let manager = LifecycleManager::new(&store_arc);
let freed = manager.maintenance_cycle(); // Run one cleanup pass

// Or spawn a background thread
manager.start_background_cleanup(Duration::from_millis(100));
```

#### Extension Trait Shortcut

```rust
use sfb::lifecycle::BlobStoreLifecycleExt;

let store = Arc::new(PinnedBlobStore::with_defaults()?);
store.start_cleanup(Duration::from_millis(100)); // Background thread auto-stops when store drops
```

---

### [Profiler](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/profiling/mod.rs#63-66) / [ProfileStats](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/profiling/mod.rs#13-39) — Metrics

```rust
let stats: ProfileStats = store.profiler().stats();

println!("Appends: {}", stats.total_appends);
println!("Bytes written: {}", stats.total_bytes_written);
println!("Active pages: {}", stats.active_pages);
println!("Fragmentation: {:.1}%", stats.fragmentation_ratio * 100.0);
println!("Avg append size: {} bytes", stats.avg_append_size());
```

Key [ProfileStats](file:///Users/neeraj/Dev/DMXP/stable-fragmented-buffer/src/profiling/mod.rs#13-39) fields: `total_pages_allocated`, `total_pages_freed`, `total_appends`, `total_reads`, `total_cleanups`, `multi_page_spans`, `total_bytes_written`, `total_bytes_read`, `active_pages`, `active_capacity_bytes`, `fragmentation_ratio`, `uptime_secs`.

---

## Usage Patterns for DMXP-MPMC

### Basic: Single-Threaded Append/Read

```rust
use sfb::{PinnedBlobStore, Config};

let store = PinnedBlobStore::with_defaults()?;

let handle = store.append(b"payload data")?;
let data = store.get(&handle).unwrap();
assert_eq!(data, b"payload data");

store.acknowledge(&handle); // Mark for cleanup
```

### Producer-Consumer with Channels

```rust
use sfb::{PinnedBlobStore, Config, BlobHandle};
use std::sync::{Arc, mpsc};
use std::thread;

let store = Arc::new(PinnedBlobStore::new(Config::default())?);
let (tx, rx) = mpsc::channel::<BlobHandle>();

// Producer
let prod_store = Arc::clone(&store);
thread::spawn(move || {
    let handle = prod_store.append(b"large payload...").unwrap();
    tx.send(handle).unwrap(); // Send 32-byte handle, not the data
});

// Consumer
let cons_store = Arc::clone(&store);
thread::spawn(move || {
    let handle = rx.recv().unwrap();
    if let Some(data) = cons_store.get(&handle) {
        // process data...
        cons_store.acknowledge(&handle);
    }
});
```

### With Background Cleanup (Recommended for Long-Running)

```rust
use sfb::{PinnedBlobStore, Config};
use sfb::lifecycle::BlobStoreLifecycleExt;
use std::sync::Arc;
use std::time::Duration;

let store = Arc::new(PinnedBlobStore::new(Config::performance())?);

// Elastic Brain: auto-frees acknowledged/expired pages every 100ms
store.start_cleanup(Duration::from_millis(100));

// Use store normally — cleanup happens in the background
let handle = store.append(b"data")?;
```

### Custom Page Size for Buffer Integration

```rust
let config = Config {
    page_size: 64 * 1024,      // 64 KB pages (match your buffer slot size)
    prefetch_threshold: 0.8,
    decay_timeout_ms: 2000,
    default_ttl_ms: 10_000,     // 10 second TTL
};
let store = PinnedBlobStore::new(config)?;
```

---

## Architecture at a Glance

```
┌─────────────────────────────────────────────┐
│        PinnedBlobStore (Public API)          │
│  append() → BlobHandle    get() → Vec<u8>   │
│  acknowledge()   cleanup_acknowledged()     │
├─────────────────────────────────────────────┤
│       LifecycleManager ("Elastic Brain")    │
│  GrowthController (prefetch at 80%)         │
│  DecayManager (free empty pages after TTL)  │
├─────────────────────────────────────────────┤
│         SegmentedBackend (HashMap<Page>)     │
│  Pages: Box<[u8]> with MaybeUninit          │
│  Lock-free atomic append within pages       │
└─────────────────────────────────────────────┘
```

**Key design properties:**
- Pages are heap-allocated fixed-size `Box<[u8]>` — never moved after allocation
- Atomic `fetch_add` for lock-free space reservation within a page
- `RwLock` on the backend map; `Mutex` on the free-pages recycle heap
- Freed page IDs are recycled via a min-heap to keep the address space compact
- Multi-page writes allocate contiguous page IDs from the high-water mark
