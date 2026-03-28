# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo build                   # Debug build
cargo build --release         # Release build
cargo test                    # Run all tests
cargo test --release          # Run tests with optimizations
cargo clippy                  # Lint
cargo fmt                     # Format
cargo fmt --check             # Check formatting

# Run a specific example
cargo run --example shared_lifecycle_benchmark --release
```

## Architecture

`stable-fragmented-buffer` is a high-performance, in-memory blob store that guarantees **pointer stability**: once data is written, its memory address never moves. This is achieved by chaining fixed-size pages rather than reallocating.

### Layers

**Public API** — `PinnedBlobStore` in `src/page/store.rs`
- `append(&[u8]) -> BlobHandle` — write data, returns a stable handle
- `get(&BlobHandle) -> Option<&[u8]>` — read data by handle
- `acknowledge(&BlobHandle)` — mark data as consumed, enabling cleanup
- `new_shared()` / `attach_shared()` — cross-process shared memory mode

**StorageBackend trait** (`src/backend/mod.rs`) — pluggable storage:
- `SegmentedBackend` (`src/backend/segmented.rs`) — default; uses `BTreeMap<u32, Page>` of heap-allocated fixed-size pages
- `SharedBackend` (`src/backend/shared.rs`) — Unix only; files in `/dev/shm` for cross-process IPC; a control file holds atomics for synchronization, data lives in numbered chunk files

**LifecycleManager** (`src/lifecycle/lifecycle.rs`) — background thread handling:
- **Scale-up**: prefetch next page when current page hits 80% capacity
- **Scale-down**: release empty pages only after `decay_timeout_ms` (default 5s) to avoid thrashing

**Profiler** (`src/profiling/mod.rs`) — lock-free metrics via atomics

### Key Types

- `BlobHandle` — 32-byte opaque reference: `page_id`, `offset`, `size`, `timestamp`, `generation`, `end_page_id`, `total_size`
- `Config` — tunable: `page_size` (default 1MB), `prefetch_threshold` (0.8), `decay_timeout_ms` (5000), `default_ttl_ms` (30000)
- `BlobError` — `HandleExpired`, `InvalidHandle`, `OutOfMemory`, `DataTooLarge`, `PageFull`

### Concurrency Model

`PinnedBlobStore` uses:
- `AtomicU32` / `AtomicUsize` for lock-free space reservation within pages (CAS loop in `Page::try_reserve`)
- `parking_lot::RwLock` around the backend for page allocation/removal
- `Mutex<BinaryHeap<Reverse<u32>>>` for the free-page recycling min-heap
- `generation_counter` (AtomicU32) to prevent ABA problems on recycled page IDs

### Multi-page Blobs

If a blob doesn't fit in the current page's remaining space, the remainder of that page is wasted ("skip-and-pad") and the data starts at the beginning of a new page. Large blobs spanning multiple pages are tracked via `end_page_id` and `total_size` in `BlobHandle`.

### Tests

Three integration tests live in `src/lib.rs`: basic append/get roundtrip, 100 sequential appends, and multi-page overflow with small page sizes.
