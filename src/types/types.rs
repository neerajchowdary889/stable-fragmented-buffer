use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

/// Reference to data stored in the blob store.
/// Supports both single-page and multi-page data.
/// Total size: 32 bytes (still well under 1KB message queue limit)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlobHandle {
    /// Starting page ID
    pub(crate) page_id: u32,

    /// Offset within the starting page (in bytes)
    pub(crate) offset: u32,

    /// Size of the stored data (in bytes) - for single-page compatibility
    pub(crate) size: u32,

    /// Creation timestamp (milliseconds since UNIX epoch)
    pub(crate) timestamp: u64,

    /// Generation counter for ABA problem prevention
    pub(crate) generation: u32,

    /// Ending page ID (for multi-page data, same as page_id for single-page)
    pub(crate) end_page_id: u32,

    /// Total size across all pages (u64 for large files)
    pub(crate) total_size: u64,
}

impl BlobHandle {
    /// Create a new blob handle for single-page data
    pub(crate) fn new(page_id: u32, offset: u32, size: u32, generation: u32) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("System time before UNIX epoch")
            .as_millis() as u64;

        Self {
            page_id,
            offset,
            size,
            timestamp,
            generation,
            end_page_id: page_id, // Same page for single-page data
            total_size: size as u64,
        }
    }

    /// Create a new blob handle for multi-page data
    pub(crate) fn new_multi_page(
        start_page_id: u32,
        start_offset: u32,
        end_page_id: u32,
        total_size: u64,
        generation: u32,
    ) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("System time before UNIX epoch")
            .as_millis() as u64;

        Self {
            page_id: start_page_id,
            offset: start_offset,
            size: total_size.min(u32::MAX as u64) as u32, // For compatibility
            timestamp,
            generation,
            end_page_id,
            total_size,
        }
    }

    /// Check if this handle spans multiple pages
    pub(crate) fn is_multi_page(&self) -> bool {
        self.end_page_id != self.page_id
    }

    /// Check if this handle has expired based on TTL
    pub(crate) fn is_expired(&self, ttl_ms: u64) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("System time before UNIX epoch")
            .as_millis() as u64;

        now - self.timestamp > ttl_ms
    }

    /// Get the age of this handle in milliseconds
    pub fn age_ms(&self) -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("System time before UNIX epoch")
            .as_millis() as u64;

        now - self.timestamp
    }

    // Public getter methods for external access
    /// Get the starting page ID
    pub fn page_id(&self) -> u32 {
        self.page_id
    }

    /// Get the offset within the starting page (in bytes)
    pub fn offset(&self) -> u32 {
        self.offset
    }

    /// Get the size of the stored data (in bytes)
    pub fn size(&self) -> u32 {
        self.size
    }

    /// Get the generation counter
    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// Get the ending page ID
    pub fn end_page_id(&self) -> u32 {
        self.end_page_id
    }

    /// Get the total size across all pages
    pub fn total_size(&self) -> u64 {
        self.total_size
    }

    /// Get the creation timestamp (milliseconds since UNIX epoch)
    pub fn timestamp(&self) -> u64 {
        self.timestamp
    }
}

/// Configuration for the blob store
#[derive(Debug, Clone)]
pub struct Config {
    /// Size of each page in bytes (default: 64KB)
    pub page_size: usize,

    /// Threshold for prefetching next page (0.0 - 1.0, default: 0.8)
    pub prefetch_threshold: f32,

    /// How long to keep empty pages before freeing (milliseconds, default: 5000)
    pub decay_timeout_ms: u64,

    /// Default TTL for stored data (milliseconds, default: 30000)
    pub default_ttl_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            page_size: 1 * 1024 * 1024, // 1MB
            prefetch_threshold: 0.8,    // 80%
            decay_timeout_ms: 5000,     // 5 seconds
            default_ttl_ms: 30000,      // 30 seconds
        }
    }
}

impl Config {
    /// Create a performance-optimized configuration
    pub fn performance() -> Self {
        Self {
            page_size: 2 * 1024 * 1024, // 2MB (huge pages)
            prefetch_threshold: 0.8,
            decay_timeout_ms: 7000,
            default_ttl_ms: 30000,
        }
    }

    /// Create a memory-optimized configuration
    pub fn memory_efficient() -> Self {
        Self {
            page_size: 512 * 1024,    // 512KB
            prefetch_threshold: 0.90, // 90% - less aggressive prefetch
            decay_timeout_ms: 1000,   // 1 second - faster cleanup
            default_ttl_ms: 30000,
        }
    }
}

/// Errors that can occur in the blob store
#[derive(Error, Debug)]
pub enum BlobError {
    #[error("Handle has expired (TTL exceeded)")]
    HandleExpired,

    #[error("Invalid handle (generation mismatch or bad page ID)")]
    InvalidHandle,

    #[error("Out of memory (failed to allocate page)")]
    OutOfMemory,

    #[error("Data too large (size: {size}, max: {max})")]
    DataTooLarge { size: usize, max: usize },

    #[error("Page is full")]
    PageFull,
}

pub type Result<T> = std::result::Result<T, BlobError>;
