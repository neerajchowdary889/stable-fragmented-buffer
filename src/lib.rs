//! # Stable Fragmented Buffer
//!
//! A high-performance, in-memory blob storage system with pointer stability.
//!
//! ## Features
//!
//! - **Pointer Stability**: References never invalidate, even as the store grows
//! - **Lock-Free Append**: High-throughput concurrent writes
//! - **TTL-Based Cleanup**: Automatic memory reclamation
//! - **Elastic Scaling**: Prefetching prevents allocation latency
//!
//! ## Example
//!
//! ```rust
//! use stable_fragmented_buffer::{PinnedBlobStore, Config};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // Create a blob store
//! let store = PinnedBlobStore::with_defaults()?;
//!
//! // Append data and get a handle
//! let data = b"Hello, World!";
//! let handle = store.append(data)?;
//!
//! // Retrieve data using the handle
//! if let Some(retrieved) = store.get(&handle) {
//!     assert_eq!(retrieved, data);
//! }
//!
//! // Acknowledge when done processing
//! store.acknowledge(&handle);
//! # Ok(())
//! # }
//! ```

pub mod backend;
pub mod lifecycle;
pub mod page;
pub mod profiling;
pub mod types;

pub use page::{BlobStats, PinnedBlobStore};
pub use types::{BlobError, BlobHandle, Config};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_append_and_get() {
        let store = PinnedBlobStore::with_defaults().unwrap();

        let data = b"Hello, World!";
        let handle = store.append(data).unwrap();

        let retrieved = store.get(&handle).unwrap();
        assert_eq!(retrieved, data);
    }

    #[test]
    fn test_multiple_appends() {
        let store = PinnedBlobStore::with_defaults().unwrap();

        let mut handles = Vec::new();
        for i in 0..100 {
            let data = format!("Message {}", i);
            let handle = store.append(data.as_bytes()).unwrap();
            handles.push((handle, data));
        }

        // Verify all data
        for (handle, expected) in handles {
            let retrieved = store.get(&handle).unwrap();
            assert_eq!(retrieved, expected.as_bytes());
        }
    }

    // #[test]
    // fn test_acknowledgment() {
    //     let store = PinnedBlobStore::with_defaults().unwrap();

    //     let data = b"Test data";
    //     let handle = store.append(data).unwrap();

    //     // Should be able to acknowledge
    //     assert!(store.acknowledge(&handle));
    // }

    #[test]
    fn test_page_overflow() {
        // Create store with small pages
        let config = Config {
            page_size: 1024, // 1KB pages
            ..Default::default()
        };
        let store = PinnedBlobStore::new(config).unwrap();

        // Append data larger than one page
        let large_data = vec![0u8; 512];

        // Should create multiple pages
        for _ in 0..10 {
            let handle = store.append(&large_data).unwrap();
            let retrieved = store.get(&handle).unwrap();
            assert_eq!(retrieved.len(), 512);
        }

        let stats = store.stats();
        assert!(stats.page_count > 1);
    }
}
