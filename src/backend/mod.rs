//! # Storage Backends
//!
//! Pluggable storage layer for `PinnedBlobStore`. All backends implement
//! the [`StorageBackend`] trait which provides page allocation, lookup,
//! and removal.
//!
//! ## Backends
//!
//! | Backend | Use case | Storage | Cross-process |
//! |---------|----------|---------|---------------|
//! | [`segmented::SegmentedBackend`] | Single-process heap storage | `BTreeMap<u32, Page>` | No |
//! | [`shared::SharedBackend`] | Cross-process IPC via `/dev/shm` | POSIX shared memory (mmap) | Yes |
//!
//! ## Choosing a backend
//!
//! - Use **SegmentedBackend** (default) for single-process workloads where
//!   pointer stability matters but cross-process access is not needed.
//! - Use **SharedBackend** for producer/consumer patterns across processes.
//!   Data is written to `/dev/shm` chunks and referenced via 24-byte
//!   `OverflowHandle`s that can be sent through ring buffers or pipes.

use crate::page::page::Page;
use crate::types::Result;

/// Trait for storage backend implementations.
///
/// Each method documents its time complexity in terms of `n` (number of pages).
pub(crate) trait StorageBackend: Send + Sync {
    /// Allocate a new page with the given size.
    ///
    /// Time: O(log n) for BTreeMap insertion.
    fn allocate_page(&mut self, id: u32, size: usize, generation: u32) -> Result<()>;

    /// Get an immutable reference to a page.
    ///
    /// Time: O(log n) for BTreeMap lookup.
    fn get_page(&self, id: u32) -> Option<&Page>;

    /// Get the total number of pages.
    ///
    /// Time: O(1).
    fn page_count(&self) -> usize;

    /// Remove a page (for decay/cleanup).
    ///
    /// Time: O(log n) for BTreeMap removal.
    fn remove_page(&mut self, id: u32) -> bool;

    /// Get list of all currently active page IDs.
    ///
    /// Time: O(n) — iterates all pages.
    fn active_page_ids(&self) -> Vec<u32>;
}

pub mod segmented;
pub mod shared;
