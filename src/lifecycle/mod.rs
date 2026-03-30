//! # Lifecycle Management (The "Elastic Brain")
//!
//! Background maintenance for `PinnedBlobStore`. Handles two scaling strategies:
//!
//! - **Rapid scale-up**: When the active page/chunk nears capacity (80%),
//!   the next one is prefetched so allocation latency never hits the hot path.
//! - **Slow scale-down (decay)**: Empty pages/chunks are not freed immediately.
//!   They are kept alive for `decay_timeout_ms` (default 5 s) to absorb
//!   bursty workloads without thrashing.
//!
//! ## Usage
//!
//! The simplest way is via the [`BlobStoreLifecycleExt`] extension trait:
//!
//! ```rust
//! use stable_fragmented_buffer::{PinnedBlobStore, BlobStoreLifecycleExt};
//! use std::sync::Arc;
//! use std::time::Duration;
//!
//! let store = Arc::new(PinnedBlobStore::with_defaults().unwrap());
//! store.start_cleanup(Duration::from_millis(100)); // Elastic Brain activated!
//! ```
//!
//! The background thread runs `maintenance_cycle()` at the given interval,
//! calling both `cleanup_acknowledged()` (heap pages) and `cleanup_shared()`
//! (shared-memory chunks). It stops automatically when the `Arc<PinnedBlobStore>`
//! is dropped (detected via `Weak::upgrade` failure).
//!
//! ## For shared-mode specifically
//!
//! `cleanup_shared()` sweeps all non-active chunks and recycles those where:
//! 1. `ack_count >= entry_count` (all entries consumed)
//! 2. `empty_since` exceeds `decay_timeout_ms` (grace period elapsed)
//!
//! Recycled chunks get a new generation counter and their header is reset
//! atomically, so concurrent readers see a clean generation mismatch rather
//! than corrupted data.

pub mod lifecycle;

pub use lifecycle::*;

use crate::page::PinnedBlobStore;
use std::sync::Arc;
use std::time::Duration;

/// Extension trait to easily enable automatic background cleanup.
pub trait BlobStoreLifecycleExt {
    /// Start the background cleanup thread ("The Brain").
    ///
    /// The thread will sleep for `interval`, then run one maintenance cycle,
    /// and repeat until the store is dropped.
    ///
    /// Time: O(1) to spawn; each cycle is O(p + c) where p = heap pages, c = shared chunks.
    fn start_cleanup(&self, interval: Duration);
}

impl BlobStoreLifecycleExt for Arc<PinnedBlobStore> {
    fn start_cleanup(&self, interval: Duration) {
        LifecycleManager::new(self).start_background_cleanup(interval);
    }
}
