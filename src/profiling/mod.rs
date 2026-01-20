//! Lightweight profiling module - tracks ONLY current state
//!
//! Zero historical data - just atomic counters for latest metrics.
//! Fire-and-forget updates, no blocking.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Profiling statistics snapshot - current state only
/// All fields are public for direct access
#[derive(Debug, Clone)]
pub struct ProfileStats {
    pub total_pages: usize,
    pub total_data_bytes: u64,
    pub total_appends: u64,
    pub total_reads: u64,
    pub total_append_bytes: u64,
    pub total_read_bytes: u64,
    pub multi_page_spans: u64,
    pub total_cleanups: u64,
    pub pages_freed: u64,
    pub bytes_freed: u64,
    pub uptime_secs: u64,
}

impl ProfileStats {
    /// Average append size in bytes
    #[inline]
    pub fn avg_append_size(&self) -> u64 {
        if self.total_appends > 0 {
            self.total_append_bytes / self.total_appends
        } else {
            0
        }
    }

    /// Average read size in bytes
    #[inline]
    pub fn avg_read_size(&self) -> u64 {
        if self.total_reads > 0 {
            self.total_read_bytes / self.total_reads
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
    total_pages: AtomicUsize,
    total_data_bytes: AtomicU64,
    total_appends: AtomicU64,
    total_reads: AtomicU64,
    total_append_bytes: AtomicU64,
    total_read_bytes: AtomicU64,
    multi_page_spans: AtomicU64,
    total_cleanups: AtomicU64,
    pages_freed: AtomicU64,
    bytes_freed: AtomicU64,
    start_time: Instant,
}

impl Profiler {
    pub fn new() -> Self {
        Self {
            state: Arc::new(ProfilerState {
                total_pages: AtomicUsize::new(0),
                total_data_bytes: AtomicU64::new(0),
                total_appends: AtomicU64::new(0),
                total_reads: AtomicU64::new(0),
                total_append_bytes: AtomicU64::new(0),
                total_read_bytes: AtomicU64::new(0),
                multi_page_spans: AtomicU64::new(0),
                total_cleanups: AtomicU64::new(0),
                pages_freed: AtomicU64::new(0),
                bytes_freed: AtomicU64::new(0),
                start_time: Instant::now(),
            }),
        }
    }

    /// Get current statistics snapshot
    pub fn stats(&self) -> ProfileStats {
        ProfileStats {
            total_pages: self.state.total_pages.load(Ordering::Relaxed),
            total_data_bytes: self.state.total_data_bytes.load(Ordering::Relaxed),
            total_appends: self.state.total_appends.load(Ordering::Relaxed),
            total_reads: self.state.total_reads.load(Ordering::Relaxed),
            total_append_bytes: self.state.total_append_bytes.load(Ordering::Relaxed),
            total_read_bytes: self.state.total_read_bytes.load(Ordering::Relaxed),
            multi_page_spans: self.state.multi_page_spans.load(Ordering::Relaxed),
            total_cleanups: self.state.total_cleanups.load(Ordering::Relaxed),
            pages_freed: self.state.pages_freed.load(Ordering::Relaxed),
            bytes_freed: self.state.bytes_freed.load(Ordering::Relaxed),
            uptime_secs: self.state.start_time.elapsed().as_secs(),
        }
    }

    #[inline]
    pub fn record_page_allocated(&self, size: usize) {
        self.state.total_pages.fetch_add(1, Ordering::Relaxed);
        self.state
            .total_data_bytes
            .fetch_add(size as u64, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_page_freed(&self, _size: usize) {
        self.state.total_pages.fetch_sub(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_append(&self, size: usize) {
        self.state.total_appends.fetch_add(1, Ordering::Relaxed);
        self.state
            .total_append_bytes
            .fetch_add(size as u64, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_read(&self, size: usize) {
        self.state.total_reads.fetch_add(1, Ordering::Relaxed);
        self.state
            .total_read_bytes
            .fetch_add(size as u64, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_multi_page_span(&self) {
        self.state.multi_page_spans.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_cleanup(&self) {
        self.state.total_cleanups.fetch_add(1, Ordering::Relaxed);
    }

    #[inline]
    pub fn record_page_cleanup(&self, bytes_freed: usize) {
        self.state.pages_freed.fetch_add(1, Ordering::Relaxed);
        self.state
            .bytes_freed
            .fetch_add(bytes_freed as u64, Ordering::Relaxed);
    }
}

impl Default for Profiler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_profiler_basic() {
        let profiler = Profiler::new();
        profiler.record_page_allocated(2 * 1024 * 1024);
        profiler.record_append(1024);
        profiler.record_read(1024);

        let stats = profiler.stats();
        assert_eq!(stats.total_pages, 1);
        assert_eq!(stats.total_appends, 1);
        assert_eq!(stats.total_reads, 1);
        assert_eq!(stats.avg_append_size(), 1024);
        assert_eq!(stats.avg_read_size(), 1024);
    }
}
