pub mod lifecycle;

pub use lifecycle::*;

use crate::page::PinnedBlobStore;
use std::sync::Arc;
use std::time::Duration;

/// Extension trait to easily enable automatic background cleanup.
pub trait BlobStoreLifecycleExt {
    /// Start the background cleanup thread ("The Brain").
    ///
    /// Usage:
    /// ```rust
    /// use stable_fragmented_buffer::{PinnedBlobStore, BlobStoreLifecycleExt};
    /// use std::time::Duration;
    ///
    /// let store = PinnedBlobStore::with_defaults().unwrap();
    /// store.start_cleanup(Duration::from_millis(100)); // Elastic Brain activated!
    /// ```
    fn start_cleanup(&self, interval: Duration);
}

impl BlobStoreLifecycleExt for Arc<PinnedBlobStore> {
    fn start_cleanup(&self, interval: Duration) {
        LifecycleManager::new(self).start_background_cleanup(interval);
    }
}
