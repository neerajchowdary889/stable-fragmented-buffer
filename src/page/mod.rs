//! # Page Storage and Public API
//!
//! This module contains the core data structures:
//!
//! - [`Page`] — A fixed-size heap-allocated memory block with lock-free
//!   append (CAS loop on an atomic `used` counter). Pages are the building
//!   blocks of the segmented backend.
//! - [`PinnedBlobStore`] — The top-level public API that orchestrates
//!   backends, page allocation, recycling, and profiling.
//!
//! ## Pointer Stability
//!
//! Unlike `Vec`, which reallocates on growth, `PinnedBlobStore` chains
//! independent fixed-size pages. Once data is written to a page, its
//! physical address never moves — this is the core invariant.
//!
//! ## Concurrency Model
//!
//! - Space reservation within a page: lock-free CAS loop (`AtomicUsize`)
//! - Page map access: `parking_lot::RwLock` (read-heavy, write-rare)
//! - Free-page recycling: `Mutex<BinaryHeap<Reverse<u32>>>` (min-heap)
//! - Generation counter: `AtomicU32` (prevents ABA on recycled page IDs)

pub(crate) mod page;
mod store;

pub use store::*;
