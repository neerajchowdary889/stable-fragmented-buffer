//! # Profiling — Lock-Free Metrics
//!
//! Lightweight, lock-free metric tracking via atomic counters.
//! Zero historical data — just fire-and-forget increments on the write path
//! and a consistent snapshot via [`Profiler::stats()`] on the read path.
//!
//! ## Tracked metrics
//!
//! - **Pages**: allocated, freed, active (derived)
//! - **Operations**: appends, reads, cleanups, multi-page spans
//! - **Data volume**: bytes written, read, discarded
//! - **Capacity**: allocated, freed, fragmentation ratio
//! - **Uptime**: seconds since profiler creation
//!
//! ## Ordering
//!
//! - Writes use `Relaxed` — fire-and-forget, no synchronisation needed.
//! - Reads use `Acquire` — snapshot is consistent enough for derived
//!   values (e.g. `active_pages = allocated - freed`) to never underflow.
//!   `saturating_sub` is used as a safety net.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Profiling statistics snapshot
/// Contains both raw counters and derived metrics
#[derive(Debug, Clone)]
pub struct ProfileStats {
    // Basic Counters
    pub total_pages_allocated: usize,
    pub total_pages_freed: usize,
    pub total_appends: u64,
    pub total_reads: u64,
    pub total_cleanups: u64,
    pub multi_page_spans: u64, // Added back

    // Data Volume
    pub total_bytes_written: u64,
    pub total_bytes_read: u64,
    pub total_bytes_discarded: u64, // Data in freed pages

    // Capacity Volume
    pub total_capacity_allocated: u64,
    pub total_capacity_freed: u64,

    // Derived State (Snapshots)
    pub active_pages: usize,
    pub active_capacity_bytes: u64, // Total size of active pages
    pub active_data_bytes: u64,     // Actual data stored in active pages
    pub free_space_bytes: u64,      // Capacity - Data
    pub fragmentation_ratio: f64,   // Free / Capacity
    pub uptime_secs: u64,
}

impl ProfileStats {
    /// Average append size in bytes
    #[inline]
    pub fn avg_append_size(&self) -> u64 {
        if self.total_appends > 0 {
            self.total_bytes_written / self.total_appends
        } else {
            0
        }
    }

    /// Average read size in bytes
    #[inline]
    pub fn avg_read_size(&self) -> u64 {
        if self.total_reads > 0 {
            self.total_bytes_read / self.total_reads
        } else {
            0
        }
    }
}

/// Profiler - lightweight, lock-free metric tracking
pub struct Profiler {
    state: Arc<ProfilerState>,
}

/// Internal state with atomic counters
struct ProfilerState {
    // Pages
    total_pages_allocated: AtomicUsize,
    total_pages_freed: AtomicUsize,

    // Ops
    total_appends: AtomicU64,
    total_reads: AtomicU64,
    total_cleanups: AtomicU64,
    multi_page_spans: AtomicU64,

    // Bytes
    total_bytes_written: AtomicU64,
    total_bytes_read: AtomicU64,
    total_bytes_discarded: AtomicU64,

    // Capacity
    total_capacity_allocated: AtomicU64,
    total_capacity_freed: AtomicU64,

    start_time: Instant,
}

impl Profiler {
    /// Create a new profiler with all counters at zero.
    ///
    /// Time: O(1).
    pub fn new() -> Self {
        Self {
            state: Arc::new(ProfilerState {
                total_pages_allocated: AtomicUsize::new(0),
                total_pages_freed: AtomicUsize::new(0),
                total_appends: AtomicU64::new(0),
                total_reads: AtomicU64::new(0),
                total_cleanups: AtomicU64::new(0),
                multi_page_spans: AtomicU64::new(0),
                total_bytes_written: AtomicU64::new(0),
                total_bytes_read: AtomicU64::new(0),
                total_bytes_discarded: AtomicU64::new(0),
                total_capacity_allocated: AtomicU64::new(0),
                total_capacity_freed: AtomicU64::new(0),
                start_time: Instant::now(),
            }),
        }
    }

    /// Get current statistics snapshot.
    ///
    /// Uses `Acquire` ordering on reads so that related counters (e.g.
    /// allocated vs freed) are observed in a consistent order. Derived
    /// values use `saturating_sub` to avoid underflow if a free is
    /// observed before its corresponding allocation.
    ///
    /// Time: O(1) — reads ~11 atomic counters and computes derived values.
    pub fn stats(&self) -> ProfileStats {
        let allocated_pages = self.state.total_pages_allocated.load(Ordering::Acquire);
        let freed_pages = self.state.total_pages_freed.load(Ordering::Acquire);

        let allocated_cap = self.state.total_capacity_allocated.load(Ordering::Acquire);
        let freed_cap = self.state.total_capacity_freed.load(Ordering::Acquire);

        let written = self.state.total_bytes_written.load(Ordering::Acquire);
        let discarded = self.state.total_bytes_discarded.load(Ordering::Acquire);

        let active_pages = allocated_pages.saturating_sub(freed_pages);
        let active_cap = allocated_cap.saturating_sub(freed_cap);
        let active_data = written.saturating_sub(discarded);
        let free_space = active_cap.saturating_sub(active_data);

        let fragmentation = if active_cap > 0 {
            free_space as f64 / active_cap as f64
        } else {
            0.0
        };

        ProfileStats {
            total_pages_allocated: allocated_pages,
            total_pages_freed: freed_pages,
            total_appends: self.state.total_appends.load(Ordering::Acquire),
            total_reads: self.state.total_reads.load(Ordering::Acquire),
            total_cleanups: self.state.total_cleanups.load(Ordering::Acquire),
            multi_page_spans: self.state.multi_page_spans.load(Ordering::Acquire),

            total_bytes_written: written,
            total_bytes_read: self.state.total_bytes_read.load(Ordering::Acquire),
            total_bytes_discarded: discarded,

            total_capacity_allocated: allocated_cap,
            total_capacity_freed: freed_cap,

            active_pages,
            active_capacity_bytes: active_cap,
            active_data_bytes: active_data,
            free_space_bytes: free_space,
            fragmentation_ratio: fragmentation,

            uptime_secs: self.state.start_time.elapsed().as_secs(),
        }
    }

    /// Record an append operation. Time: O(1).
    pub fn record_append(&self, size: usize) {
        self.state.total_appends.fetch_add(1, Ordering::Relaxed);
        self.state
            .total_bytes_written
            .fetch_add(size as u64, Ordering::Relaxed);
    }

    /// Record a read operation. Time: O(1).
    pub fn record_read(&self, size: usize) {
        self.state.total_reads.fetch_add(1, Ordering::Relaxed);
        self.state
            .total_bytes_read
            .fetch_add(size as u64, Ordering::Relaxed);
    }

    /// Record a page allocation. Time: O(1).
    pub fn record_page_allocated(&self, capacity: usize) {
        self.state
            .total_pages_allocated
            .fetch_add(1, Ordering::Relaxed);
        self.state
            .total_capacity_allocated
            .fetch_add(capacity as u64, Ordering::Relaxed);
    }

    /// Record a page cleanup (freed). Time: O(1).
    pub fn record_page_cleanup(&self, capacity: usize, used_data: usize) {
        self.state.total_pages_freed.fetch_add(1, Ordering::Relaxed);
        self.state
            .total_capacity_freed
            .fetch_add(capacity as u64, Ordering::Relaxed);
        self.state
            .total_bytes_discarded
            .fetch_add(used_data as u64, Ordering::Relaxed);
    }

    /// Record a cleanup cycle. Time: O(1).
    pub fn record_cleanup(&self) {
        self.state.total_cleanups.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a multi-page span. Time: O(1).
    pub fn record_multi_page_span(&self) {
        self.state.multi_page_spans.fetch_add(1, Ordering::Relaxed);
    }
}
