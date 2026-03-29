use super::types::now_ms;

/// Cross-process overflow handle.
/// Stored directly in the ring buffer slot payload when `overflow == 1`.
///
/// Uses relative `page_id` + `offset` addressing so any process can resolve
/// the handle against its own mmap of the shared arena — no absolute pointers.
///
/// # Layout
/// Total size: 24 bytes. ABI-stable across processes.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OverflowHandle {
    /// Chunk index in the shared arena (0..∞, maps to `/dev/shm/{ns}_data_{page_id}`)
    pub page_id: u32,
    /// Byte offset within the chunk's data region
    pub offset: u32,
    /// Size of the stored data in bytes
    pub size: u32,
    /// Generation counter — prevents ABA problems when a chunk is recycled
    pub generation: u32,
    /// Creation timestamp (ms since UNIX epoch) — used for TTL expiry
    pub timestamp: u64,
}

impl OverflowHandle {
    /// Create a new overflow handle.
    ///
    /// Time: O(1).
    pub fn new(page_id: u32, offset: u32, size: u32, generation: u32) -> Self {
        let timestamp = now_ms();

        Self {
            page_id,
            offset,
            size,
            generation,
            timestamp,
        }
    }

    /// Check if this handle has expired based on the given TTL (in milliseconds).
    ///
    /// Time: O(1).
    pub fn is_expired(&self, ttl_ms: u64) -> bool {
        now_ms().saturating_sub(self.timestamp) > ttl_ms
    }

    /// Get the age of this handle in milliseconds.
    pub fn age_ms(&self) -> u64 {
        now_ms().saturating_sub(self.timestamp)
    }

    /// Serialize this handle to a byte slice (zero-copy view).
    ///
    /// # Safety
    /// Safe because `OverflowHandle` is `#[repr(C)]` with no padding ambiguity.
    ///
    /// Time: O(1) — pointer cast, no copy.
    pub fn as_bytes(&self) -> &[u8] {
        unsafe {
            std::slice::from_raw_parts(
                self as *const Self as *const u8,
                std::mem::size_of::<Self>(),
            )
        }
    }

    /// Deserialize an `OverflowHandle` from a byte slice.
    ///
    /// Returns `None` if the slice is not exactly `size_of::<OverflowHandle>()` bytes.
    ///
    /// Time: O(1) — 24-byte memcpy.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != std::mem::size_of::<Self>() {
            return None;
        }

        let mut handle = std::mem::MaybeUninit::<Self>::uninit();
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                handle.as_mut_ptr() as *mut u8,
                std::mem::size_of::<Self>(),
            );
            Some(handle.assume_init())
        }
    }
}

/// Selects which storage backend `PinnedBlobStore` uses.
#[derive(Debug, Clone)]
pub enum BackendMode {
    /// Process-private heap pages (original behaviour).
    Heap,
    /// Cross-process shared memory via `/dev/shm` chunked files.
    Shared {
        /// Namespace prefix for shm file names (e.g. `"dmxp"` → `/dev/shm/dmxp_ctrl`).
        namespace: String,
        /// Size of each data chunk in bytes (default: 32 MB).
        chunk_size: usize,
    },
}

impl Default for BackendMode {
    fn default() -> Self {
        BackendMode::Heap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_overflow_handle_size() {
        assert_eq!(std::mem::size_of::<OverflowHandle>(), 24);
    }

    #[test]
    fn test_overflow_handle_roundtrip() {
        let handle = OverflowHandle::new(42, 1024, 512, 7);
        let bytes = handle.as_bytes();
        let restored = OverflowHandle::from_bytes(bytes).unwrap();
        assert_eq!(handle, restored);
    }

    #[test]
    fn test_overflow_handle_ttl() {
        let handle = OverflowHandle::new(0, 0, 100, 1);
        // Should not be expired with a generous TTL
        assert!(!handle.is_expired(30_000));
        // Sleep 2ms so the handle ages past the 0 TTL
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert!(handle.is_expired(0));
    }
}
