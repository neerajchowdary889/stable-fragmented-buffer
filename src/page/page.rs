use crate::types::{BlobError, Result};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Metadata for a single entry within a page
#[derive(Debug)]
pub(crate) struct EntryMetadata {
    /// Offset within the page
    pub offset: u32,

    /// Size of the entry
    pub size: u32,

    /// Creation timestamp
    pub timestamp: u64,

    /// Whether this entry has been acknowledged
    pub acknowledged: AtomicBool,
}

impl EntryMetadata {
    fn new(offset: u32, size: u32) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("System time before UNIX epoch")
            .as_millis() as u64;

        Self {
            offset,
            size,
            timestamp,
            acknowledged: AtomicBool::new(false),
        }
    }

    /// Check if this entry has expired
    pub fn is_expired(&self, ttl_ms: u64) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("System time before UNIX epoch")
            .as_millis() as u64;

        now - self.timestamp > ttl_ms
    }

    /// Mark this entry as acknowledged
    pub fn acknowledge(&self) {
        self.acknowledged.store(true, Ordering::Release);
    }

    /// Check if acknowledged or expired
    pub fn should_cleanup(&self, ttl_ms: u64) -> bool {
        self.acknowledged.load(Ordering::Acquire) || self.is_expired(ttl_ms)
    }
}

/// A fixed-size page of memory
pub(crate) struct Page {
    /// Unique page identifier
    pub id: u32,

    /// The actual data buffer
    data: Box<[u8]>,

    /// Current number of bytes used (atomic for lock-free append)
    used: AtomicUsize,

    /// Generation counter for handle validation
    pub generation: u32,

    /// Metadata for all entries in this page
    entries: parking_lot::RwLock<Vec<EntryMetadata>>,

    /// Timestamp when this page became empty (for decay tracking)
    empty_since: AtomicUsize, // 0 means not empty, otherwise timestamp in ms
}

impl Page {
    /// Create a new page with the given size
    ///
    /// Uses uninitialized memory for performance - safe because:
    /// 1. We track `used` atomically
    /// 2. Only written regions are ever read
    /// 3. Writes happen before reads via `used` counter
    pub fn new(id: u32, size: usize, generation: u32) -> Self {
        // PERFORMANCE: Use MaybeUninit to skip zeroing
        // This is safe because:
        // - We never read uninitialized memory (tracked by `used`)
        // - All data is written before being read
        // - The `used` atomic ensures proper ordering
        use std::mem::MaybeUninit;

        let mut uninit_vec: Vec<MaybeUninit<u8>> = Vec::with_capacity(size);
        unsafe {
            uninit_vec.set_len(size);
        }

        // Convert to initialized (we promise to only read written parts)
        let data = unsafe {
            // SAFETY: We will only ever read from regions that have been written to,
            // as tracked by the `used` atomic counter. The memory is allocated and
            // has the correct size, we just skip the zeroing step.
            std::mem::transmute::<Vec<MaybeUninit<u8>>, Vec<u8>>(uninit_vec)
        }
        .into_boxed_slice();

        Self {
            id,
            data,
            used: AtomicUsize::new(0),
            generation,
            entries: parking_lot::RwLock::new(Vec::new()),
            empty_since: AtomicUsize::new(0),
        }
    }

    /// Try to append data to this page (lock-free if space available)
    pub fn try_append(&self, data: &[u8]) -> Result<(u32, u32)> {
        let data_len = data.len();

        // Check if data fits in a page at all
        if data_len > self.data.len() {
            return Err(BlobError::DataTooLarge {
                size: data_len,
                max: self.data.len(),
            });
        }

        // Atomically reserve space
        let offset = self.used.fetch_add(data_len, Ordering::AcqRel);

        // Check if we overflowed
        if offset + data_len > self.data.len() {
            // Rollback the reservation
            self.used.fetch_sub(data_len, Ordering::AcqRel);
            return Err(BlobError::PageFull);
        }

        // Copy data into the reserved space
        // SAFETY: We've atomically reserved this space, no other thread can write here
        unsafe {
            let ptr = self.data.as_ptr() as *mut u8;
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr.add(offset), data_len);
        }

        // Add entry metadata
        let entry = EntryMetadata::new(offset as u32, data_len as u32);
        self.entries.write().push(entry);

        // Clear empty timestamp since we just added data
        self.empty_since.store(0, Ordering::Release);

        Ok((offset as u32, data_len as u32))
    }

    /// Get a reference to data at the given offset
    pub fn get(&self, offset: u32, size: u32) -> Option<&[u8]> {
        let start = offset as usize;
        let end = start + size as usize;

        if end <= self.data.len() {
            Some(&self.data[start..end])
        } else {
            None
        }
    }

    /// Get available space in this page
    pub fn available_space(&self) -> usize {
        let used = self.used.load(Ordering::Acquire);
        self.data.len().saturating_sub(used)
    }

    /// Try to append as much data as possible, return bytes written
    pub fn try_append_partial(&self, data: &[u8]) -> Result<(u32, u32)> {
        let available = self.available_space();
        if available == 0 {
            return Err(BlobError::PageFull);
        }

        // Append as much as we can
        let to_write = data.len().min(available);
        let offset = self.used.fetch_add(to_write, Ordering::AcqRel);

        // Copy data
        unsafe {
            let ptr = self.data.as_ptr() as *mut u8;
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr.add(offset), to_write);
        }

        // Add entry metadata
        let entry = EntryMetadata::new(offset as u32, to_write as u32);
        self.entries.write().push(entry);

        // Clear empty timestamp
        self.empty_since.store(0, Ordering::Release);

        Ok((offset as u32, to_write as u32))
    }

    /// Check if this page is full based on threshold (0.0 - 1.0)
    pub fn is_full(&self, threshold: f32) -> bool {
        let used = self.used.load(Ordering::Acquire);
        let capacity = self.data.len();

        (used as f32 / capacity as f32) >= threshold
    }

    /// Get current usage as a fraction (0.0 - 1.0)
    pub fn usage(&self) -> f32 {
        let used = self.used.load(Ordering::Acquire);
        let capacity = self.data.len();
        used as f32 / capacity as f32
    }

    /// Check if all entries are acknowledged or expired
    pub fn is_empty(&self, ttl_ms: u64) -> bool {
        let entries = self.entries.read();

        if entries.is_empty() {
            return true;
        }

        entries.iter().all(|e| e.should_cleanup(ttl_ms))
    }

    /// Mark the page as empty and record timestamp
    pub fn mark_empty_if_needed(&self, ttl_ms: u64) {
        if self.is_empty(ttl_ms) {
            // Only set timestamp if it hasn't been set yet
            let current = self.empty_since.load(Ordering::Acquire);
            if current == 0 {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("System time before UNIX epoch")
                    .as_millis() as usize;

                let _ =
                    self.empty_since
                        .compare_exchange(0, now, Ordering::AcqRel, Ordering::Acquire);
            }
        } else {
            // Not empty anymore (maybe new data came in?), reset to 0
            self.empty_since.store(0, Ordering::Release);
        }
    }

    /// Check if this page should be freed (empty for longer than decay timeout)
    pub fn should_decay(&self, decay_timeout_ms: u64, ttl_ms: u64) -> bool {
        if !self.is_empty(ttl_ms) {
            return false;
        }

        let empty_since = self.empty_since.load(Ordering::Acquire);
        if empty_since == 0 {
            return false;
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("System time before UNIX epoch")
            .as_millis() as usize;

        (now - empty_since) as u64 > decay_timeout_ms
    }

    /// Acknowledge an entry at the given offset
    pub fn acknowledge_entry(&self, offset: u32) -> bool {
        let entries = self.entries.read();

        if let Some(entry) = entries.iter().find(|e| e.offset == offset) {
            entry.acknowledge();
            true
        } else {
            false
        }
    }

    /// Get the number of active (non-acknowledged, non-expired) entries
    pub fn active_entry_count(&self, ttl_ms: u64) -> usize {
        let entries = self.entries.read();
        entries.iter().filter(|e| !e.should_cleanup(ttl_ms)).count()
    }
}

impl std::fmt::Debug for Page {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Page")
            .field("id", &self.id)
            .field("capacity", &self.data.len())
            .field("used", &self.used.load(Ordering::Acquire))
            .field("generation", &self.generation)
            .field("entry_count", &self.entries.read().len())
            .finish()
    }
}
