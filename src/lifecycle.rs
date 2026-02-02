//! The "Elastic Brain" of the system.
//!
//! Handles lifecycle events like automatic cleanup (decay).

use crate::page::PinnedBlobStore;
use std::sync::{Arc, Weak};
use std::thread;
use std::time::Duration;

/// Manages the lifecycle of a blob store, running background maintenance tasks.
pub struct LifecycleManager {
    store: Weak<PinnedBlobStore>,
}

impl LifecycleManager {
    /// Create a new lifecycle manager for the given store
    pub fn new(store: &Arc<PinnedBlobStore>) -> Self {
        Self {
            store: Arc::downgrade(store),
        }
    }

    /// Run a single maintenance cycle
    ///
    /// Returns the number of pages freed.
    pub fn maintenance_cycle(&self) -> usize {
        if let Some(store) = self.store.upgrade() {
            store.cleanup_acknowledged()
        } else {
            0
        }
    }

    /// Spawn a background thread to run maintenance periodically
    ///
    /// The thread will automatically stop when the store is dropped.
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
