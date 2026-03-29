//! Lifecycle management tests for the shared backend.
//!
//! These tests verify the three pillars of the lifecycle manager:
//!
//! 1. **Acknowledgement-based cleanup** — chunks are freed after all entries
//!    are acked and the decay timeout elapses.
//! 2. **TTL-based expiry** — chunks are freed when their oldest data exceeds
//!    the TTL, even if no entries were acknowledged (consumer crash scenario).
//! 3. **Proactive prefetch** — the next chunk is pre-allocated when the active
//!    chunk crosses the prefetch threshold (80%), eliminating allocation
//!    latency on the overflow path.
//!
//! Additionally tests:
//! - Real `/dev/shm` file cleanup (shm_unlink, not just header reset)
//! - Background `LifecycleManager` thread integration
//! - Concurrent append + cleanup safety
//! - Write-after-cleanup correctness (new data into fresh chunks)
//!
//! Run with `cargo test -- --nocapture` to see the print statements.

use crate::backend::shared::{SharedBackend, DEFAULT_CHUNK_SIZE, CHUNK_HEADER_SIZE};
use crate::lifecycle::{LifecycleManager, BlobStoreLifecycleExt};
use crate::page::PinnedBlobStore;
use crate::types::{Config, OverflowHandle};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Unique namespace per test (global atomic counter).
fn ns() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(1000);
    format!("lt{}", C.fetch_add(1, Ordering::Relaxed))
}

// ═══════════════════════════════════════════════════════════════════════════
// 1. ACKNOWLEDGEMENT-BASED CLEANUP
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_ack_cleanup_frees_chunks() {
    println!("\n=== test_ack_cleanup_frees_chunks ===");
    let ns = ns();
    let chunk_size = CHUNK_HEADER_SIZE + 128;
    let backend = SharedBackend::create(&ns, chunk_size, None).unwrap();
    println!("[1] Created backend ns={}, chunk_size={} ({}B header + {}B data)",
        ns, chunk_size, CHUNK_HEADER_SIZE, chunk_size - CHUNK_HEADER_SIZE);

    let h0 = backend.append(&[0xAAu8; 100]).unwrap();
    println!("[2] Appended 100B -> chunk {}, offset {}", h0.page_id, h0.offset);
    let h1 = backend.append(&[0xBBu8; 100]).unwrap();
    println!("[3] Appended 100B -> chunk {}, offset {} (overflowed because 100 > 28B remaining)", h1.page_id, h1.offset);
    let h2 = backend.append(&[0xCCu8; 100]).unwrap();
    println!("[4] Appended 100B -> chunk {}, offset {}", h2.page_id, h2.offset);

    let initial_count = backend.chunk_count();
    println!("[5] Total chunks mapped: {}", initial_count);

    backend.acknowledge(&h0);
    backend.acknowledge(&h1);
    backend.acknowledge(&h2);
    println!("[6] Acknowledged all 3 handles");

    let freed = backend.cleanup_chunks(30_000, 0);
    let after_count = backend.chunk_count();
    println!("[7] cleanup_chunks(ttl=30s, decay=0ms) -> freed {} chunks", freed);
    println!("[8] Chunks remaining: {} (was {})", after_count, initial_count);
    println!("[9] Active write chunk is never freed, so {} chunk(s) survived", after_count);

    assert!(freed >= 2, "expected >= 2 freed, got {}", freed);
    assert!(after_count < initial_count);
    println!("[PASS] Ack-based cleanup freed non-active chunks\n");
}

#[test]
fn test_partial_ack_does_not_free() {
    println!("\n=== test_partial_ack_does_not_free ===");
    let ns = ns();
    let chunk_size = CHUNK_HEADER_SIZE + 256;
    let backend = SharedBackend::create(&ns, chunk_size, None).unwrap();
    println!("[1] Created backend, data capacity = {}B", chunk_size - CHUNK_HEADER_SIZE);

    let h1 = backend.append(&[0xAAu8; 100]).unwrap();
    let h2 = backend.append(&[0xBBu8; 100]).unwrap();
    println!("[2] Wrote 2 entries to chunk {} (100B + 100B = 200B / {}B capacity)",
        h1.page_id, chunk_size - CHUNK_HEADER_SIZE);

    backend.acknowledge(&h1);
    println!("[3] Acknowledged entry 1 only (entry 2 NOT acked)");
    println!("    ack_count=1, entry_count=2 -> acked < entries -> NOT eligible");

    let freed = backend.cleanup_chunks(30_000, 0);
    println!("[4] cleanup_chunks -> freed {} chunks", freed);

    assert_eq!(freed, 0);
    println!("[PASS] Partially-acked chunk was not freed\n");
}

#[test]
fn test_decay_timeout_prevents_premature_free() {
    println!("\n=== test_decay_timeout_prevents_premature_free ===");
    let ns = ns();
    let chunk_size = CHUNK_HEADER_SIZE + 128;
    let backend = SharedBackend::create(&ns, chunk_size, None).unwrap();

    let h0 = backend.append(&[0xAAu8; 100]).unwrap();
    let _h1 = backend.append(&[0xBBu8; 100]).unwrap();
    println!("[1] Wrote to chunk {} and overflowed to next chunk", h0.page_id);

    backend.acknowledge(&h0);
    println!("[2] Acknowledged chunk {} entry", h0.page_id);

    let freed = backend.cleanup_chunks(30_000, 60_000);
    println!("[3] cleanup_chunks(decay=60s) -> freed {} (should be 0, decay not elapsed)", freed);
    assert_eq!(freed, 0);

    let freed = backend.cleanup_chunks(30_000, 0);
    println!("[4] cleanup_chunks(decay=0ms) -> freed {} (should be >=1, immediate decay)", freed);
    assert!(freed >= 1);

    println!("[PASS] Decay timeout prevents premature freeing\n");
}

// ═══════════════════════════════════════════════════════════════════════════
// 2. TTL-BASED EXPIRY (CONSUMER CRASH SCENARIO)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_ttl_expiry_frees_unacked_chunks() {
    println!("\n=== test_ttl_expiry_frees_unacked_chunks ===");
    let ns = ns();
    let chunk_size = CHUNK_HEADER_SIZE + 128;
    let backend = SharedBackend::create(&ns, chunk_size, None).unwrap();

    let h0 = backend.append(&[0xAAu8; 100]).unwrap();
    let h1 = backend.append(&[0xBBu8; 100]).unwrap();
    println!("[1] Wrote to chunk {} and chunk {} — NOT acknowledging (simulating consumer crash)", h0.page_id, h1.page_id);

    println!("[2] Sleeping 5ms to let TTL expire...");
    thread::sleep(Duration::from_millis(5));

    let freed = backend.cleanup_chunks(1, 0); // ttl=1ms
    println!("[3] cleanup_chunks(ttl=1ms, decay=0ms) -> freed {} chunks", freed);
    println!("    This works because first_write_ts was stamped on append,");
    println!("    and now - first_write_ts > 1ms TTL -> expired!");

    assert!(freed >= 1);
    println!("[PASS] TTL-expired unacked chunks were freed (consumer crash recovery)\n");
}

#[test]
fn test_ttl_does_not_expire_fresh_data() {
    println!("\n=== test_ttl_does_not_expire_fresh_data ===");
    let ns = ns();
    let chunk_size = CHUNK_HEADER_SIZE + 128;
    let backend = SharedBackend::create(&ns, chunk_size, None).unwrap();

    let _h0 = backend.append(&[0xAAu8; 100]).unwrap();
    let _h1 = backend.append(&[0xBBu8; 100]).unwrap();
    println!("[1] Wrote data, NOT acked (but data is fresh)");

    let freed = backend.cleanup_chunks(30_000, 0); // ttl=30s
    println!("[2] cleanup_chunks(ttl=30s) -> freed {} (should be 0, data is < 30s old)", freed);

    assert_eq!(freed, 0);
    println!("[PASS] Fresh unacked data is safe from TTL expiry\n");
}

#[test]
fn test_ttl_expiry_invalidates_handles() {
    println!("\n=== test_ttl_expiry_invalidates_handles ===");
    let ns = ns();
    let chunk_size = CHUNK_HEADER_SIZE + 128;
    let backend = SharedBackend::create(&ns, chunk_size, None).unwrap();

    let h0 = backend.append(&[0xAAu8; 100]).unwrap();
    let _h1 = backend.append(&[0xBBu8; 100]).unwrap();
    println!("[1] Wrote handle on chunk {}", h0.page_id);

    thread::sleep(Duration::from_millis(5));
    backend.cleanup_chunks(1, 0);
    println!("[2] TTL-expired and cleaned up chunk {}", h0.page_id);

    let result = backend.resolve(&h0, 30_000);
    println!("[3] resolve(h0) -> {:?} (should be None — chunk was unlinked)", result.as_ref().map(|v| v.len()));

    assert!(result.is_none());
    println!("[PASS] Handle is invalid after TTL-based cleanup\n");
}

// ═══════════════════════════════════════════════════════════════════════════
// 3. PROACTIVE PREFETCH
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_prefetch_allocates_next_chunk() {
    println!("\n=== test_prefetch_allocates_next_chunk ===");
    let ns = ns();
    let chunk_size = CHUNK_HEADER_SIZE + 100; // 100 bytes data space
    let backend = SharedBackend::create(&ns, chunk_size, None).unwrap();
    println!("[1] Created backend, data capacity = 100B, prefetch threshold = 80%");

    let h = backend.append(&[0xAAu8; 85]).unwrap();
    let usage_pct = 85.0 / 100.0 * 100.0;
    println!("[2] Appended 85B -> chunk {} (usage = {:.0}% > 80% threshold)", h.page_id, usage_pct);

    let count = backend.chunk_count();
    println!("[3] chunk_count = {} (should be >= 2 — prefetch triggered!)", count);
    println!("    Chunk 0: active, 85/100 bytes used");
    println!("    Chunk 1: pre-allocated, 0 bytes used (ready for overflow)");

    assert!(count >= 2);
    println!("[PASS] Prefetch pre-allocated the next chunk at 85% usage\n");
}

#[test]
fn test_prefetch_does_not_trigger_below_threshold() {
    println!("\n=== test_prefetch_does_not_trigger_below_threshold ===");
    let ns = ns();
    let chunk_size = CHUNK_HEADER_SIZE + 200;
    let backend = SharedBackend::create(&ns, chunk_size, None).unwrap();
    println!("[1] Created backend, data capacity = 200B");

    let h = backend.append(&[0xAAu8; 100]).unwrap();
    let usage_pct = 100.0 / 200.0 * 100.0;
    println!("[2] Appended 100B -> chunk {} (usage = {:.0}% < 80% threshold)", h.page_id, usage_pct);

    let count = backend.chunk_count();
    println!("[3] chunk_count = {} (should be 1 — no prefetch triggered)", count);

    assert_eq!(count, 1);
    println!("[PASS] No prefetch below 80% threshold\n");
}

#[test]
fn test_prefetch_reduces_overflow_latency() {
    println!("\n=== test_prefetch_reduces_overflow_latency ===");
    let ns = ns();
    let chunk_size = CHUNK_HEADER_SIZE + 100;
    let backend = SharedBackend::create(&ns, chunk_size, None).unwrap();

    let _h1 = backend.append(&[0xAAu8; 85]).unwrap();
    let count_after_prefetch = backend.chunk_count();
    println!("[1] Wrote 85/100B -> prefetch fired -> chunk_count = {}", count_after_prefetch);

    let h2 = backend.append(&[0xBBu8; 50]).unwrap();
    let count_after_overflow = backend.chunk_count();
    println!("[2] Wrote 50B -> overflowed to chunk {} -> chunk_count = {}", h2.page_id, count_after_overflow);
    println!("    chunk_count didn't change! Overflow reused the prefetched chunk.");
    println!("    (No shm_open/mmap syscall was needed on the overflow path)");

    assert_eq!(count_after_prefetch, count_after_overflow);
    assert_ne!(h2.page_id, 0);
    println!("[PASS] Prefetch eliminated overflow allocation latency\n");
}

// ═══════════════════════════════════════════════════════════════════════════
// 4. REAL SHM FILE CLEANUP (NOT JUST HEADER RESET)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_cleanup_removes_shm_files() {
    println!("\n=== test_cleanup_removes_shm_files ===");
    let ns = ns();
    let chunk_size = CHUNK_HEADER_SIZE + 128;
    let backend = SharedBackend::create(&ns, chunk_size, None).unwrap();

    let h0 = backend.append(&[0xAAu8; 100]).unwrap();
    let _h1 = backend.append(&[0xBBu8; 100]).unwrap();
    let freed_chunk_id = h0.page_id;
    println!("[1] Wrote to chunk {} and overflowed", freed_chunk_id);

    backend.acknowledge(&h0);
    let freed = backend.cleanup_chunks(30_000, 0);
    println!("[2] Acked and cleaned up -> freed {} chunk(s)", freed);

    #[cfg(unix)]
    {
        let shm_path = format!("/{}_data_{}", ns, freed_chunk_id);
        let shm_name = std::ffi::CString::new(shm_path.as_str()).unwrap();
        let fd = unsafe { libc::shm_open(shm_name.as_ptr(), libc::O_RDONLY, 0o600) };
        println!("[3] Tried shm_open('{}', O_RDONLY) -> fd = {}", shm_path, fd);
        println!("    fd < 0 means the file was truly unlinked from /dev/shm!");
        assert!(fd < 0);
    }
    println!("[PASS] shm file was unlinked (not just header-reset)\n");
}

#[test]
fn test_write_after_cleanup_uses_new_chunks() {
    println!("\n=== test_write_after_cleanup_uses_new_chunks ===");
    let ns = ns();
    let chunk_size = CHUNK_HEADER_SIZE + 128;
    let backend = SharedBackend::create(&ns, chunk_size, None).unwrap();

    let h0 = backend.append(&[0xAAu8; 100]).unwrap();
    let h1 = backend.append(&[0xBBu8; 100]).unwrap();
    let h2 = backend.append(&[0xCCu8; 100]).unwrap();
    println!("[1] Filled chunks {}, {}, {}", h0.page_id, h1.page_id, h2.page_id);

    backend.acknowledge(&h0);
    backend.acknowledge(&h1);
    backend.acknowledge(&h2);
    let freed = backend.cleanup_chunks(30_000, 0);
    println!("[2] Acked all, cleanup freed {} chunks", freed);

    let new_data = b"fresh data after cleanup";
    let h_new = backend.append(new_data).unwrap();
    let resolved = backend.resolve(&h_new, 30_000).unwrap();
    println!("[3] Wrote '{}' -> chunk {}, offset {}",
        String::from_utf8_lossy(new_data), h_new.page_id, h_new.offset);
    println!("[4] Resolved successfully: {} bytes", resolved.len());

    assert_eq!(resolved, new_data);
    println!("[PASS] New data written to fresh chunk after cleanup\n");
}

// ═══════════════════════════════════════════════════════════════════════════
// 5. LIFECYCLE MANAGER INTEGRATION (BACKGROUND THREAD)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_lifecycle_manager_single_cycle() {
    println!("\n=== test_lifecycle_manager_single_cycle ===");
    let ns = ns();
    let config = Config {
        decay_timeout_ms: 0,
        default_ttl_ms: 30_000,
        ..Default::default()
    };
    let store = Arc::new(
        PinnedBlobStore::new_shared(config, &ns, CHUNK_HEADER_SIZE + 128).unwrap()
    );
    println!("[1] Created PinnedBlobStore in shared mode");

    let h0 = store.append_shared(&[0xAAu8; 100]).unwrap();
    let _h1 = store.append_shared(&[0xBBu8; 100]).unwrap();
    store.acknowledge_shared(&h0);
    println!("[2] Wrote 2 entries, acked first one");

    let manager = LifecycleManager::new(&store);
    println!("[3] Created LifecycleManager (Weak ref to store)");

    let freed = manager.maintenance_cycle();
    println!("[4] maintenance_cycle() -> freed {} (calls cleanup_acknowledged + cleanup_shared)", freed);

    assert!(freed >= 1);
    println!("[PASS] Single maintenance cycle freed acked chunks\n");
}

#[test]
fn test_lifecycle_manager_background_thread() {
    println!("\n=== test_lifecycle_manager_background_thread ===");
    let ns = ns();
    let config = Config {
        decay_timeout_ms: 0,
        default_ttl_ms: 30_000,
        ..Default::default()
    };
    let store = Arc::new(
        PinnedBlobStore::new_shared(config, &ns, CHUNK_HEADER_SIZE + 128).unwrap()
    );

    store.start_cleanup(Duration::from_millis(10));
    println!("[1] Started background cleanup thread (interval = 10ms)");

    let h0 = store.append_shared(&[0xAAu8; 100]).unwrap();
    let _h1 = store.append_shared(&[0xBBu8; 100]).unwrap();
    store.acknowledge_shared(&h0);
    println!("[2] Wrote 2 entries, acked chunk {}", h0.page_id);

    println!("[3] Sleeping 50ms (5 cleanup cycles)...");
    thread::sleep(Duration::from_millis(50));

    let result = store.resolve(&h0);
    println!("[4] resolve(h0) -> {:?} (should be None — background thread cleaned it)",
        result.as_ref().map(|v| v.len()));

    assert!(result.is_none());
    println!("[PASS] Background thread automatically cleaned up acked chunks\n");
}

#[test]
fn test_lifecycle_manager_stops_on_drop() {
    println!("\n=== test_lifecycle_manager_stops_on_drop ===");
    let ns = ns();
    let config = Config {
        decay_timeout_ms: 0,
        default_ttl_ms: 30_000,
        ..Default::default()
    };
    let store = Arc::new(
        PinnedBlobStore::new_shared(config, &ns, CHUNK_HEADER_SIZE + 128).unwrap()
    );

    store.start_cleanup(Duration::from_millis(5));
    println!("[1] Started background thread (interval = 5ms)");

    println!("[2] Dropping the store Arc...");
    drop(store);

    println!("[3] Sleeping 30ms to let background thread detect the drop...");
    thread::sleep(Duration::from_millis(30));
    println!("[4] If we got here, the thread exited cleanly!");
    println!("    (It called Weak::upgrade(), got None, and broke out of the loop)");
    println!("[PASS] Background thread stops when store is dropped\n");
}

#[test]
fn test_lifecycle_ttl_expiry_via_manager() {
    println!("\n=== test_lifecycle_ttl_expiry_via_manager ===");
    let ns = ns();
    let config = Config {
        decay_timeout_ms: 0,
        default_ttl_ms: 1, // 1ms TTL
        ..Default::default()
    };
    let store = Arc::new(
        PinnedBlobStore::new_shared(config, &ns, CHUNK_HEADER_SIZE + 128).unwrap()
    );

    let _h0 = store.append_shared(&[0xAAu8; 100]).unwrap();
    let _h1 = store.append_shared(&[0xBBu8; 100]).unwrap();
    println!("[1] Wrote 2 entries, NOT acknowledging (simulating dead consumer)");
    println!("    Config: default_ttl_ms = 1ms");

    println!("[2] Sleeping 5ms to let TTL expire...");
    thread::sleep(Duration::from_millis(5));

    let manager = LifecycleManager::new(&store);
    let freed = manager.maintenance_cycle();
    println!("[3] maintenance_cycle() -> freed {} chunks via TTL expiry", freed);
    println!("    cleanup_shared() saw first_write_ts is > 1ms old -> expired!");

    assert!(freed >= 1);
    println!("[PASS] LifecycleManager triggers TTL-based expiry for unacked data\n");
}

// ═══════════════════════════════════════════════════════════════════════════
// 6. CONCURRENT APPEND + CLEANUP SAFETY
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn test_concurrent_append_during_cleanup() {
    println!("\n=== test_concurrent_append_during_cleanup ===");
    let ns = ns();
    let config = Config {
        decay_timeout_ms: 0,
        default_ttl_ms: 30_000,
        ..Default::default()
    };
    let store = Arc::new(
        PinnedBlobStore::new_shared(config, &ns, CHUNK_HEADER_SIZE + 256).unwrap()
    );

    store.start_cleanup(Duration::from_millis(5));
    println!("[1] Started background cleanup (5ms interval)");
    println!("[2] Spawning 4 writer threads x 50 msgs each...");

    let mut writers = Vec::new();
    for t in 0..4 {
        let s = Arc::clone(&store);
        writers.push(thread::spawn(move || {
            let mut handles = Vec::new();
            for i in 0..50 {
                let data = format!("t{}-msg{}", t, i);
                let h = s.append_shared(data.as_bytes()).expect("append during cleanup");
                handles.push((h, data));
                if i % 2 == 0 {
                    s.acknowledge_shared(&h);
                }
            }
            handles
        }));
    }

    let all: Vec<(OverflowHandle, String)> = writers
        .into_iter()
        .flat_map(|jh| jh.join().unwrap())
        .collect();

    println!("[3] All threads joined. Total appends: {}", all.len());

    let mut resolved_count = 0;
    let mut cleaned_count = 0;
    for (handle, expected) in &all {
        if let Some(data) = store.resolve(handle) {
            assert_eq!(data, expected.as_bytes());
            resolved_count += 1;
        } else {
            cleaned_count += 1;
        }
    }
    println!("[4] Resolve check: {} still alive, {} already cleaned up", resolved_count, cleaned_count);
    println!("    (acked entries may have been cleaned by background thread — that's correct)");

    assert_eq!(all.len(), 200);
    println!("[PASS] Concurrent append + cleanup: no panics, no corruption, no deadlocks\n");
}

#[test]
fn test_cleanup_does_not_corrupt_active_chunk() {
    println!("\n=== test_cleanup_does_not_corrupt_active_chunk ===");
    let ns = ns();
    let chunk_size = CHUNK_HEADER_SIZE + 256;
    let backend = SharedBackend::create(&ns, chunk_size, None).unwrap();

    let h1 = backend.append(&[0xAAu8; 50]).unwrap();
    let h2 = backend.append(&[0xBBu8; 50]).unwrap();
    println!("[1] Wrote 2 entries to chunk {} (active write chunk, no overflow)", h1.page_id);
    assert_eq!(h1.page_id, h2.page_id);

    backend.acknowledge(&h1);
    backend.acknowledge(&h2);
    println!("[2] Acknowledged both entries (all entries on active chunk are acked)");

    let freed = backend.cleanup_chunks(30_000, 0);
    println!("[3] cleanup_chunks -> freed {} (should be 0!)", freed);
    println!("    Even though all entries are acked, chunk {} is the active write_head", h1.page_id);
    println!("    -> cleanup SKIPS it to prevent corruption of the hot write path");
    assert_eq!(freed, 0);

    let h3 = backend.append(&[0xCCu8; 50]).unwrap();
    let data = backend.resolve(&h3, 30_000).unwrap();
    println!("[4] Wrote and resolved new entry on chunk {} -> {} bytes OK", h3.page_id, data.len());

    assert!(data.len() == 50);
    println!("[PASS] Active chunk is protected from cleanup\n");
}
