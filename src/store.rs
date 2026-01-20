use parking_lot::RwLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use crate::backend::{segmented::SegmentedBackend, StorageBackend};
use crate::types::{BlobError, BlobHandle, Config, Result};

/// The main blob store providing pointer-stable storage
pub struct PinnedBlobStore {
    /// Storage backend (behind RwLock for thread safety)
    backend: Arc<RwLock<Box<dyn StorageBackend>>>,

    /// Configuration
    config: Config,

    /// Current active page ID
    current_page: AtomicU32,

    /// Global generation counter
    generation_counter: AtomicU32,
}

impl PinnedBlobStore {
    /// Create a new blob store with the given configuration
    pub fn new(config: Config) -> Result<Self> {
        // Use Segmented backend (heap-allocated pages with MaybeUninit optimization)
        let backend: Box<dyn StorageBackend> = Box::new(SegmentedBackend::new());

        let store = Self {
            backend: Arc::new(RwLock::new(backend)),
            config,
            current_page: AtomicU32::new(0),
            generation_counter: AtomicU32::new(0),
        };

        // Allocate the first page
        store.allocate_page(0)?;

        Ok(store)
    }

    /// Create a new blob store with default configuration
    pub fn with_defaults() -> Result<Self> {
        Self::new(Config::default())
    }

    /// Allocate a new page
    fn allocate_page(&self, page_id: u32) -> Result<()> {
        let generation = self.generation_counter.fetch_add(1, Ordering::AcqRel);
        let mut backend = self.backend.write();
        backend.allocate_page(page_id, self.config.page_size, generation)
    }

    /// Append data to the blob store and return a stable handle
    ///
    /// For data larger than page size, automatically spans multiple pages.
    pub fn append(&self, data: &[u8]) -> Result<BlobHandle> {
        if data.is_empty() {
            return Err(BlobError::DataTooLarge {
                size: 0,
                max: self.config.page_size,
            });
        }

        // For data larger than page size, split across multiple pages
        if data.len() > self.config.page_size {
            return self.append_multi_page(data);
        }

        // Fast path: data fits in a single page
        loop {
            let current_page_id = self.current_page.load(Ordering::Acquire);

            // Try to append to current page
            let backend = self.backend.read();
            if let Some(page) = backend.get_page(current_page_id) {
                match page.try_append(data) {
                    Ok((offset, size)) => {
                        // Success! Create handle
                        let handle =
                            BlobHandle::new(current_page_id, offset, size, page.generation);

                        // Check if we need to prefetch next page
                        if page.is_full(self.config.prefetch_threshold) {
                            drop(backend); // Release read lock
                            let next_page_id = current_page_id + 1;

                            // Check if next page exists
                            let backend_read = self.backend.read();
                            if backend_read.get_page(next_page_id).is_none() {
                                drop(backend_read);
                                // Prefetch next page
                                self.allocate_page(next_page_id)?;
                            }
                        }

                        return Ok(handle);
                    }
                    Err(BlobError::PageFull) => {
                        // Page is full, move to next page
                        drop(backend); // Release read lock

                        let next_page_id = current_page_id + 1;

                        // Allocate next page if it doesn't exist
                        let backend_read = self.backend.read();
                        if backend_read.get_page(next_page_id).is_none() {
                            drop(backend_read);
                            self.allocate_page(next_page_id)?;
                        } else {
                            drop(backend_read);
                        }

                        // Try to update current page pointer
                        let _ = self.current_page.compare_exchange(
                            current_page_id,
                            next_page_id,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );

                        // Loop again to try appending to new page
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            } else {
                // Page doesn't exist, allocate it
                drop(backend);
                self.allocate_page(current_page_id)?;
                continue;
            }
        }
    }

    /// Append large data spanning multiple pages
    fn append_multi_page(&self, data: &[u8]) -> Result<BlobHandle> {
        let mut remaining = data;
        let mut start_page_id = None;
        let mut start_offset = None;
        let mut current_page_id = self.current_page.load(Ordering::Acquire);
        let first_generation = self.generation_counter.load(Ordering::Acquire);

        while !remaining.is_empty() {
            // Ensure current page exists
            {
                let backend = self.backend.read();
                if backend.get_page(current_page_id).is_none() {
                    drop(backend);
                    self.allocate_page(current_page_id)?;
                }
            }

            // Try to append as much as possible to current page
            let backend = self.backend.read();
            if let Some(page) = backend.get_page(current_page_id) {
                match page.try_append_partial(remaining) {
                    Ok((offset, bytes_written)) => {
                        // Record start position
                        if start_page_id.is_none() {
                            start_page_id = Some(current_page_id);
                            start_offset = Some(offset);
                        }

                        // Move to next chunk
                        remaining = &remaining[bytes_written as usize..];

                        // If page is full and we have more data, move to next page
                        if !remaining.is_empty() && page.available_space() == 0 {
                            drop(backend);
                            current_page_id += 1;

                            // Update current_page pointer
                            self.current_page.store(current_page_id, Ordering::Release);
                        } else {
                            drop(backend);
                        }
                    }
                    Err(BlobError::PageFull) => {
                        drop(backend);
                        current_page_id += 1;
                        self.current_page.store(current_page_id, Ordering::Release);
                    }
                    Err(e) => return Err(e),
                }
            }
        }

        // Create multi-page handle
        Ok(BlobHandle::new_multi_page(
            start_page_id.unwrap(),
            start_offset.unwrap(),
            current_page_id,
            data.len() as u64,
            first_generation,
        ))
    }

    /// Get a copy of data using a handle
    /// Returns None if handle is invalid or expired
    ///
    /// Supports both single-page and multi-page data.
    pub fn get(&self, handle: &BlobHandle) -> Option<Vec<u8>> {
        // Check TTL
        if handle.is_expired(self.config.default_ttl_ms) {
            return None;
        }

        // Handle multi-page data
        if handle.is_multi_page() {
            return self.get_multi_page(handle);
        }

        // Single-page fast path
        let backend = self.backend.read();
        let page = backend.get_page(handle.page_id)?;

        // Validate generation
        if page.generation != handle.generation {
            return None;
        }

        // Get data and return owned copy
        page.get(handle.offset, handle.size)
            .map(|slice| slice.to_vec())
    }

    /// Get multi-page data
    fn get_multi_page(&self, handle: &BlobHandle) -> Option<Vec<u8>> {
        let mut result = Vec::with_capacity(handle.total_size as usize);
        let backend = self.backend.read();

        for page_id in handle.page_id..=handle.end_page_id {
            let page = backend.get_page(page_id)?;

            if page_id == handle.page_id {
                // First page: from start_offset to end
                let available = page.available_space();
                let page_capacity = self.config.page_size;
                let used = page_capacity - available;
                let to_read = used - handle.offset as usize;

                if let Some(data) = page.get(handle.offset, to_read as u32) {
                    result.extend_from_slice(data);
                }
            } else if page_id == handle.end_page_id {
                // Last page: from 0 to whatever is needed
                let remaining = handle.total_size as usize - result.len();
                if let Some(data) = page.get(0, remaining as u32) {
                    result.extend_from_slice(data);
                }
            } else {
                // Middle pages: all data
                let available = page.available_space();
                let page_capacity = self.config.page_size;
                let used = page_capacity - available;

                if let Some(data) = page.get(0, used as u32) {
                    result.extend_from_slice(data);
                }
            }
        }

        Some(result)
    }

    /// Acknowledge that data has been processed and can be cleaned up
    pub fn acknowledge(&self, handle: &BlobHandle) -> bool {
        let backend = self.backend.read();
        if let Some(page) = backend.get_page(handle.page_id) {
            if page.generation == handle.generation {
                return page.acknowledge_entry(handle.offset);
            }
        }
        false
    }

    /// Get statistics about the blob store
    pub fn stats(&self) -> BlobStats {
        let backend = self.backend.read();
        let page_count = backend.page_count();
        let current_page = self.current_page.load(Ordering::Acquire);

        BlobStats {
            page_count,
            current_page_id: current_page,
        }
    }
}

/// Statistics about the blob store
#[derive(Debug, Clone)]
pub struct BlobStats {
    pub page_count: usize,
    pub current_page_id: u32,
}

impl std::fmt::Debug for PinnedBlobStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedBlobStore")
            .field("config", &self.config)
            .field("current_page", &self.current_page.load(Ordering::Acquire))
            .field("page_count", &self.backend.read().page_count())
            .finish()
    }
}

// Thread safety: PinnedBlobStore can be safely shared across threads
unsafe impl Send for PinnedBlobStore {}
unsafe impl Sync for PinnedBlobStore {}
