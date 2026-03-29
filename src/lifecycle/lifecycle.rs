//! Core lifecycle manager implementation.
//!
//! Drives the background maintenance loop that handles both heap-mode
//! page cleanup and shared-mode chunk recycling.

use crate::page::PinnedBlobStore;
use std::sync::{Arc, Weak};
use std::thread;
use std::time::Duration;

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;

/// Manages the lifecycle of a blob store, running background maintenance tasks.
///
/// Holds a `Weak` reference to the store so it does not prevent the store
/// from being dropped. When the `Arc` is dropped, the next maintenance
/// cycle detects the dangling `Weak` and the background thread exits.
pub struct LifecycleManager {
    store: Weak<PinnedBlobStore>,
}

impl LifecycleManager {
    /// Create a new lifecycle manager for the given store.
    ///
    /// Time: O(1).
    pub fn new(store: &Arc<PinnedBlobStore>) -> Self {
        Self {
            store: Arc::downgrade(store),
        }
    }

    /// Run a single maintenance cycle.
    ///
    /// Performs both:
    /// - **Heap cleanup** (`cleanup_acknowledged`): scans all heap pages,
    ///   frees those where every entry is acknowledged or TTL-expired and
    ///   the decay timeout has elapsed.
    /// - **Shared cleanup** (`cleanup_shared`): sweeps all shared-memory
    ///   chunks, recycling those that are fully acked past the decay window.
    ///
    /// Returns the total number of pages/chunks freed (heap + shared).
    ///
    /// Time: O(p + c) where p = heap pages, c = shared chunks.
    pub fn maintenance_cycle(&self) -> usize {
        if let Some(store) = self.store.upgrade() {
            let heap_freed = store.cleanup_acknowledged();
            let shared_freed = store.cleanup_shared();
            heap_freed + shared_freed
        } else {
            0
        }
    }

    /// Spawn a background thread to run maintenance periodically.
    ///
    /// The thread will automatically stop when the store is dropped
    /// (detected via `Weak::upgrade` returning `None`).
    ///
    /// Time: O(1) to spawn. Each cycle is O(p + c).
    pub fn start_background_cleanup(self, interval: Duration) {
        thread::spawn(move || {
            loop {
                thread::sleep(interval);

                // If the store has been dropped, stop the background thread
                if self.store.upgrade().is_none() {
                    break;
                }

                // Run maintenance
                self.maintenance_cycle();
            }
        });
    }
}
