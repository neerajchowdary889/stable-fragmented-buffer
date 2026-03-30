# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo build                   # Debug build
cargo build --release         # Release build
cargo test                    # Run all tests (33 unit + 2 doc-tests)
cargo test lifecycle -- --nocapture  # Run lifecycle tests with print traces
cargo test --release          # Run tests with optimizations
cargo clippy                  # Lint
cargo fmt                     # Format
cargo fmt --check             # Check formatting

# Run benchmarks
cargo run --example shared_lifecycle_benchmark --release
cargo run --example statistical_benchmark --release
```

## Architecture

`stable-fragmented-buffer` is a high-performance, in-memory blob store that guarantees **pointer stability**: once data is written, its memory address never moves. This is achieved by chaining fixed-size pages/chunks rather than reallocating.

### Module Layout

```
src/
├── backend/
│   ├── mod.rs              # StorageBackend trait (pluggable storage interface)
│   ├── segmented.rs        # SegmentedBackend — BTreeMap<u32, Page> heap pages
│   ├── shared.rs           # SharedBackend — /dev/shm cross-process IPC (primary)
│   └── shared_tests.rs     # SharedBackend unit tests (10 tests)
├── types/
│   ├── mod.rs              # Module docs + re-exports
│   ├── types.rs            # BlobHandle, Config, BlobError, now_ms()
│   └── overflow_handle.rs  # OverflowHandle (24B, #[repr(C)], cross-process)
├── page/
│   ├── mod.rs              # Module docs
│   ├── page.rs             # Page struct (heap page with CAS append)
│   └── store.rs            # PinnedBlobStore (public API facade)
├── lifecycle/
│   ├── mod.rs              # BlobStoreLifecycleExt trait + module docs
│   ├── lifecycle.rs        # LifecycleManager (background cleanup thread)
│   └── lifecycle_tests.rs  # Lifecycle tests (17 tests)
├── profiling/
│   └── mod.rs              # Profiler (lock-free atomic counters)
└── lib.rs                  # Crate root + re-exports + 3 integration tests
```

### Layers

**Public API** — `PinnedBlobStore` in `src/page/store.rs`
- Heap mode: `append(&[u8]) -> BlobHandle`, `get(&BlobHandle) -> Option<Vec<u8>>`
- Shared mode: `append_shared(&[u8]) -> OverflowHandle`, `resolve(&OverflowHandle) -> Option<Vec<u8>>`
- `new_shared(config, namespace, chunk_size)` / `attach_shared(config, namespace)` — create or join a cross-process arena
- `new_shared_with_limit(config, namespace, chunk_size, max_chunks)` — with backpressure

**SharedBackend** (`src/backend/shared.rs`) — the production backend:
- Files in `/dev/shm`: one 128-byte control file + N data chunks (default 32 MB each)
- All synchronization via atomics embedded in the mmap region (no OS IPC)
- CAS loop for lock-free space reservation within chunks
- Proactive prefetch: pre-allocates next chunk when active chunk hits 80% usage
- Backpressure via `max_chunks` limit
- Crash recovery via `SharedBackend::cleanup_namespace(namespace)`

**LifecycleManager** (`src/lifecycle/lifecycle.rs`) — background cleanup thread:
- **Scale-up (prefetch)**: next chunk pre-allocated at 80% capacity (configurable)
- **Scale-down (ack + decay)**: chunks freed after all entries acknowledged + `decay_timeout_ms` elapsed
- **TTL expiry**: chunks freed when `now - first_write_ts > default_ttl_ms`, even without acks (handles consumer crashes)
- Freed chunks are unlinked from `/dev/shm` (real memory reclamation, not just header reset)

**Profiler** (`src/profiling/mod.rs`) — lock-free metrics via atomics (Relaxed writes, Acquire reads)

### Key Types

- `OverflowHandle` — 24-byte `#[repr(C)]` cross-process reference: `page_id`, `offset`, `size`, `generation`, `timestamp`. Serializable via `as_bytes()`/`from_bytes()`.
- `BlobHandle` — 32-byte heap-mode reference: `page_id`, `offset`, `size`, `timestamp`, `generation`, `end_page_id`, `total_size`
- `Config` — tunable: `page_size` (1MB), `prefetch_threshold` (0.8), `decay_timeout_ms` (5000), `default_ttl_ms` (30000)
- `BlobError` — `HandleExpired`, `InvalidHandle`, `OutOfMemory`, `DataTooLarge`, `PageFull`

### Shared Backend Memory Layout

**Control file** (`/dev/shm/{ns}_ctrl`, 128 bytes):
- Offsets 0-24: magic (8B), version (4B), chunk_size (4B), write_head (AtomicU32), chunk_count (AtomicU32), generation (AtomicU32)
- Offsets 28-127: reserved

**Chunk header** (first 64 bytes of each `/dev/shm/{ns}_data_N`):
- Offsets 0-24: used (AtomicU32), generation (AtomicU32), entry_count (AtomicU32), ack_count (AtomicU32), empty_since (AtomicU64)
- Offset 24: first_write_ts (AtomicU64) — stamped on first append, used for TTL expiry
- Offsets 32-63: reserved
- Data region starts at byte 64

### Concurrency Model

- Space reservation within chunks: lock-free CAS loop (`AtomicU32`)
- Chunk map: `parking_lot::RwLock<BTreeMap<u32, Arc<SharedChunk>>>`
- Chunk ID allocation: CAS loop on `chunk_count` in the control file (prevents races)
- Generation counters (`AtomicU32`): prevent ABA problems on recycled chunks
- `resolve()` returns `Vec<u8>` (owned copy) + post-copy generation recheck to prevent data corruption from concurrent cleanup

### Lifecycle Cleanup Flow

A non-active chunk is freed when **either**:
1. All entries acknowledged (`ack_count >= entry_count`) AND decay timeout elapsed, **or**
2. TTL expired (`now - first_write_ts > ttl_ms`) regardless of ack state

Freed chunks are removed from the in-memory map AND unlinked from `/dev/shm` (real tmpfs memory reclamation).

### Capacity Limits

- Max chunks: `u32::MAX - 1` (~4 billion), or `max_chunks` config
- Max chunk size: `u32::MAX` (4 GB), stored as u32 in control file
- Max single append: `chunk_size - 64` bytes (shared mode does not span chunks)
- Default: 32 MB chunks = ~33 million bytes per append
- Practical limit: `/dev/shm` size (typically 50% of RAM on Linux)

### Tests

- 33 unit tests: 3 integration (lib.rs), 3 types, 10 shared backend, 17 lifecycle
- 2 doc-tests
- Lifecycle tests cover: ack cleanup, partial ack safety, decay timeout, TTL expiry, prefetch, shm file unlink, background thread, concurrent safety
- Run `cargo test lifecycle -- --nocapture` for traced output showing each step
