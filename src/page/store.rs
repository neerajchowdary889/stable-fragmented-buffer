use parking_lot::Mutex;
use parking_lot::RwLock;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use crate::backend::{segmented::SegmentedBackend, StorageBackend};
use crate::profiling::Profiler;
use crate::types::{BlobError, BlobHandle, Config, Result};

/// The main blob store providing pointer-stable storage
pub struct PinnedBlobStore {
    /// Storage backend (behind RwLock for thread safety)
    backend: Arc<RwLock<Box<dyn StorageBackend>>>,

    /// Configuration
    config: Config,

    /// Current active page ID (The "Hot Head")
    current_page: AtomicU32,

    /// Highest page ID ever allocated (High Water Mark)
    high_water_mark: AtomicU32,

    /// Min-Heap of recycled page IDs (prioritize filling holes)
    free_pages: Mutex<BinaryHeap<Reverse<u32>>>,

    /// Global generation counter
    generation_counter: AtomicU32,

    /// Profiler for tracking metrics
    profiler: Profiler,
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
            high_water_mark: AtomicU32::new(0),
            free_pages: Mutex::new(BinaryHeap::new()),
            generation_counter: AtomicU32::new(0),
            profiler: Profiler::new(),
        };

        // Allocate the first page
        store.allocate_page(0)?; // This sets up Page 0

        Ok(store)
    }

    /// Create a new blob store with default configuration
    pub fn with_defaults() -> Result<Self> {
        Self::new(Config::default())
    }

    /// Allocate a specific page ID (internal low-level alloc)
    fn allocate_page(&self, page_id: u32) -> Result<()> {
        let generation = self.generation_counter.fetch_add(1, Ordering::AcqRel);
        let mut backend = self.backend.write();
        let result = backend.allocate_page(page_id, self.config.page_size, generation);
        if result.is_ok() {
            self.profiler.record_page_allocated(self.config.page_size);
        }
        result
    }

    /// Find and allocate the next appropriate page
    ///
    /// Strategy:
    /// 1. Prefer picking a recycled page from `free_pages` (fill holes).
    /// 2. If none, increment `high_water_mark` and allocate new space.
    fn allocate_next_available_page(&self) -> Result<u32> {
        let mut free_pages = self.free_pages.lock();

        if let Some(Reverse(recycled_id)) = free_pages.pop() {
            // Found a hole! Recycle it.
            self.allocate_page(recycled_id)?;
            return Ok(recycled_id);
        }

        // No holes, expand to new space
        // fetch_add returns OLD value. So if HWM is 0, we get 0.
        // But HWM tracks *Highest Allocated*.
        // If current is 0. Next should be 1.
        let next_id = self.high_water_mark.fetch_add(1, Ordering::AcqRel) + 1;
        self.allocate_page(next_id)?;
        Ok(next_id)
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

                        // Record append operation
                        self.profiler.record_append(data.len());

                        // Prefetch Check: If full, perform proactive allocation
                        // Currently, for recycled implementation, proactive prefetch is tricky because "Next" isn't strictly +1.
                        // We skip prefetch for now to ensure simple recycling logic (Lazy Allocation).

                        return Ok(handle);
                    }
                    Err(BlobError::PageFull) => {
                        // Page is full, move to next available page
                        drop(backend); // Release read lock

                        // Allocate ANY free page (recycled or new)
                        let next_page_id = self.allocate_next_available_page()?;

                        // Try to update current page pointer
                        let _ = self.current_page.compare_exchange(
                            current_page_id,
                            next_page_id,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );

                        // Loop again to try appending to new (or updated) page
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            } else {
                // Page doesn't exist (maybe initialization race), allocate it
                drop(backend);
                // Safe fallback: try to re-allocate current if missing, or move next
                // Just moving to next is safer
                let next_page_id = self.allocate_next_available_page()?;
                let _ = self.current_page.compare_exchange(
                    current_page_id,
                    next_page_id,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                continue;
            }
        }
    }

    /// Append large data spanning multiple pages
    fn append_multi_page(&self, data: &[u8]) -> Result<BlobHandle> {
        // Multi-page strategy:
        // We CANNOT easily span across random recycled fragments (Swiss Cheese).
        // Solution: Always allocate a fresh CONTIGUOUS block at the High Water Mark.

        let chunk_size = self.config.page_size;
        let num_pages = (data.len() + chunk_size - 1) / chunk_size;

        // Reserve N contiguous IDs from High Water Mark
        let start_page_id = self
            .high_water_mark
            .fetch_add(num_pages as u32, Ordering::AcqRel)
            + 1;
        let end_page_id = start_page_id + (num_pages as u32) - 1;

        // Allocate all pages in the range
        // Note: This bypasses `free_pages`. Large blobs always consume new address space (until wrap-around).
        for i in 0..num_pages {
            self.allocate_page(start_page_id + i as u32)?;
        }

        // Write data
        let mut remaining = data;
        let mut start_offset = None;
        let mut first_generation = 0;

        for (i, page_id) in (start_page_id..=end_page_id).enumerate() {
            let backend = self.backend.read();
            let page = backend.get_page(page_id).ok_or(BlobError::PageFull)?; // Should exist

            if i == 0 {
                first_generation = page.generation;
            }

            let (offset, written) = page.try_append_partial(remaining)?;

            if i == 0 {
                start_offset = Some(offset);
            }

            remaining = &remaining[written as usize..];
            // We just allocated these fresh, so they should accept data.
            // If they are somehow full (impossible), we error out.
        }

        // Record metrics
        self.profiler.record_append(data.len());
        self.profiler.record_multi_page_span();

        Ok(BlobHandle::new_multi_page(
            start_page_id,
            start_offset.unwrap_or(0),
            end_page_id,
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
        let result = page
            .get(handle.offset, handle.size)
            .map(|slice| slice.to_vec());

        if result.is_some() {
            self.profiler.record_read(handle.size as usize);
        }

        result
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

        // Record multi-page read
        if !result.is_empty() {
            self.profiler.record_read(result.len());
            self.profiler.record_multi_page_span();
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

    /// Clean up acknowledged and expired entries
    ///
    /// This method scans all pages and marks entries that have been acknowledged
    /// or expired for cleanup. Pages that are empty, successfully decayed,
    /// AND strictly not equal to the current active page are removed from memory.
    ///
    /// Returns the number of pages that were freed.
    pub fn cleanup_acknowledged(&self) -> usize {
        // We need a write lock to remove pages
        let mut backend = self.backend.write();

        // Use the new active_page_ids method to iterate efficiently over sparse recycling IDs
        let active_ids = backend.active_page_ids();

        let mut freed_pages = 0;

        // Safety: Snapshot current page to ensure we never delete the active write head
        let current_active_page = self.current_page.load(Ordering::Acquire);

        // Scan all active pages
        for page_id in active_ids {
            // SAFETY RULE: Never touch the current active page
            if page_id == current_active_page {
                continue;
            }

            // We must first get a reference to check status
            let (should_remove, used_bytes) = if let Some(page) = backend.get_page(page_id) {
                // 1. Mark entries as empty/acknowledged
                page.mark_empty_if_needed(self.config.default_ttl_ms);

                // 2. Check if page is ready to decay (empty for > timeout)
                let decay =
                    page.should_decay(self.config.decay_timeout_ms, self.config.default_ttl_ms);

                // Capture usage statistics before the page is dropped
                let usage_ratio = page.usage();
                let used_approx = (usage_ratio * self.config.page_size as f32) as usize;

                (decay, used_approx)
            } else {
                (false, 0)
            };

            if should_remove {
                // 3. Actually remove the page from memory
                if backend.remove_page(page_id) {
                    freed_pages += 1;

                    // RECYCLING LOGIC:
                    // Instead of just dropping the ID forever, we return it to the free_pages heap.
                    // This allows lower IDs (0, 1, 2...) to be reused, keeping the active set compact.
                    // We use Mutex lock scope tightly here.
                    self.free_pages.lock().push(Reverse(page_id));

                    // Record actual memory freed (approximate based on page size)
                    // We use full page size because the entire allocation is dropped
                    self.profiler
                        .record_page_cleanup(self.config.page_size, used_bytes);
                }
            }
        }

        if freed_pages > 0 {
            self.profiler.record_cleanup();
        }

        freed_pages
    }

    /// Get access to the profiler for metrics
    pub fn profiler(&self) -> &Profiler {
        &self.profiler
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
            .field(
                "high_water_mark",
                &self.high_water_mark.load(Ordering::Acquire),
            )
            .field("page_count", &self.backend.read().page_count())
            .field("free_pages_count", &self.free_pages.lock().len())
            .finish()
    }
}

// Thread safety: PinnedBlobStore can be safely shared across threads
unsafe impl Send for PinnedBlobStore {}
unsafe impl Sync for PinnedBlobStore {}
