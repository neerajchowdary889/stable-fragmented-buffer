//! Tests for the shared-memory backend (`SharedBackend`).
//!
//! Covers:
//! - Basic CRUD: create, append, resolve, acknowledge
//! - Chunk overflow and multi-chunk allocation
//! - Cross-process attach and resolve
//! - Backpressure via `max_chunks`
//! - Input validation (chunk size, namespace)
//! - Crash recovery via `cleanup_namespace()`
//! - Concurrent stress: parallel appends, parallel resolve, full lifecycle

use super::*;
use std::sync::Arc;

/// Generate a unique namespace per test to avoid cross-test interference.
/// Uses a global atomic counter so parallel tests never collide.
fn test_namespace() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("st{}", id)
}

// ── Basic CRUD ───────────────────────────────────────────────────────────

#[test]
fn test_create_and_append() {
    let ns = test_namespace();
    let backend = SharedBackend::create(&ns, 4096, None).unwrap();

    let data = b"Hello, shared world!";
    let handle = backend.append(data).unwrap();

    assert_eq!(handle.page_id, 0);
    assert_eq!(handle.size, data.len() as u32);

    let resolved = backend.resolve(&handle, 30_000).unwrap();
    assert_eq!(resolved, data);
} // Drop auto-unlinks shm files

#[test]
fn test_attach_and_resolve() {
    let ns = test_namespace();

    let backend_a = SharedBackend::create(&ns, 4096, None).unwrap();
    let data = b"cross-process payload";
    let handle = backend_a.append(data).unwrap();

    let backend_b = SharedBackend::attach(&ns, None).unwrap();
    let resolved = backend_b.resolve(&handle, 30_000).unwrap();
    assert_eq!(resolved, data);

    drop(backend_b); // attacher drops first (no unlink)
} // creator drops, unlinks shm

#[test]
fn test_acknowledge() {
    let ns = test_namespace();
    let backend = SharedBackend::create(&ns, 4096, None).unwrap();

    let handle = backend.append(b"ack test").unwrap();
    assert!(backend.acknowledge(&handle));
}

// ── Chunk overflow ───────────────────────────────────────────────────────

#[test]
fn test_chunk_overflow_to_next() {
    let ns = test_namespace();
    let chunk_size = CHUNK_HEADER_SIZE + 128; // Only 128 bytes of data space
    let backend = SharedBackend::create(&ns, chunk_size, None).unwrap();

    let data = vec![0xABu8; 100];
    let h1 = backend.append(&data).unwrap();
    assert_eq!(h1.page_id, 0);

    // This should overflow to chunk 1
    let h2 = backend.append(&data).unwrap();
    assert_eq!(h2.page_id, 1);

    assert_eq!(backend.resolve(&h1, 30_000).unwrap(), data.as_slice());
    assert_eq!(backend.resolve(&h2, 30_000).unwrap(), data.as_slice());
}

// ── Backpressure ─────────────────────────────────────────────────────────

#[test]
fn test_max_chunks_backpressure() {
    let ns = test_namespace();
    let chunk_size = CHUNK_HEADER_SIZE + 64; // 64 bytes of data space
    // Allow max 2 chunks (chunk 0 + chunk 1)
    let backend = SharedBackend::create(&ns, chunk_size, Some(2)).unwrap();

    // Fill chunk 0
    let _h1 = backend.append(&[0xAAu8; 60]).unwrap();
    // Overflow to chunk 1
    let _h2 = backend.append(&[0xBBu8; 60]).unwrap();
    // Chunk 1 full — should fail because max_chunks=2
    let result = backend.append(&[0xCCu8; 60]);
    assert!(result.is_err());
}

// ── Validation ───────────────────────────────────────────────────────────

#[test]
fn test_chunk_size_validation() {
    let ns = test_namespace();
    // Too small
    assert!(SharedBackend::create(&ns, 32, None).is_err());
    // Zero
    assert!(SharedBackend::create(&ns, 0, None).is_err());
    // Exactly header size (no data space)
    assert!(SharedBackend::create(&ns, CHUNK_HEADER_SIZE, None).is_err());
}

// ── Crash recovery ───────────────────────────────────────────────────────

#[test]
fn test_cleanup_namespace() {
    let ns = test_namespace();

    // Create and populate, then drop normally so files are cleaned up
    {
        let backend = SharedBackend::create(&ns, 4096, None).unwrap();
        backend.append(b"some data").unwrap();
    }

    // Re-create to simulate orphaned files (Drop ran, files are gone,
    // but we create fresh ones and then clean them via cleanup_namespace)
    {
        let backend = SharedBackend::create(&ns, 4096, None).unwrap();
        backend.append(b"orphaned data").unwrap();
        // Manually unmap chunks without unlinking shm files
        // to simulate a crash where Drop doesn't run.
        backend.chunks.write().clear();
        // Now call cleanup_namespace while the ctrl file still exists
        SharedBackend::cleanup_namespace(&ns).unwrap();
    }
    // Files cleaned by cleanup_namespace above; Drop will try to unlink
    // again but that's a harmless no-op.

    // Creating a new one with the same namespace should work
    let _backend = SharedBackend::create(&ns, 4096, None).unwrap();
}

// ── Concurrent stress tests ──────────────────────────────────────────────

#[test]
fn test_concurrent_append() {
    let ns = test_namespace();
    let backend = Arc::new(SharedBackend::create(&ns, 8192, None).unwrap());

    let num_threads = 4;
    let writes_per_thread = 50;
    let mut join_handles = Vec::new();

    for t in 0..num_threads {
        let b = Arc::clone(&backend);
        join_handles.push(std::thread::spawn(move || {
            let mut handles = Vec::new();
            for i in 0..writes_per_thread {
                let payload = format!("t{}-msg{}", t, i);
                let h = b
                    .append(payload.as_bytes())
                    .expect("concurrent append failed");
                handles.push((h, payload));
            }
            handles
        }));
    }

    let all_results: Vec<_> = join_handles
        .into_iter()
        .flat_map(|jh| jh.join().unwrap())
        .collect();

    assert_eq!(all_results.len(), num_threads * writes_per_thread);

    // Verify all entries resolve correctly
    for (handle, expected) in &all_results {
        let data = backend
            .resolve(handle, 30_000)
            .expect("concurrent resolve failed");
        assert_eq!(data, expected.as_bytes(), "data corruption detected");
    }
}

#[test]
fn test_concurrent_append_and_resolve() {
    let ns = test_namespace();
    let backend = Arc::new(SharedBackend::create(&ns, 8192, None).unwrap());

    // Phase 1: pre-populate some data
    let mut seed_handles = Vec::new();
    for i in 0..20 {
        let payload = format!("seed-{}", i);
        let h = backend.append(payload.as_bytes()).unwrap();
        seed_handles.push((h, payload));
    }

    let seed_handles = Arc::new(seed_handles);
    let mut join_handles = Vec::new();

    // Spawn writers
    for t in 0..2 {
        let b = Arc::clone(&backend);
        join_handles.push(std::thread::spawn(move || {
            for i in 0..50 {
                let payload = format!("writer{}-{}", t, i);
                b.append(payload.as_bytes()).expect("writer append failed");
            }
        }));
    }

    // Spawn readers (resolving seed handles)
    for _ in 0..2 {
        let b = Arc::clone(&backend);
        let seeds = Arc::clone(&seed_handles);
        join_handles.push(std::thread::spawn(move || {
            for (handle, expected) in seeds.iter() {
                let data = b
                    .resolve(handle, 30_000)
                    .expect("reader resolve failed during concurrent writes");
                assert_eq!(data, expected.as_bytes());
            }
        }));
    }

    for jh in join_handles {
        jh.join().unwrap();
    }
}

#[test]
fn test_concurrent_lifecycle() {
    let ns = test_namespace();
    let backend = Arc::new(SharedBackend::create(&ns, 4096, None).unwrap());

    let num_threads = 4;
    let writes_per_thread = 30;

    // Phase 1: concurrent writes
    let mut join_handles = Vec::new();
    for t in 0..num_threads {
        let b = Arc::clone(&backend);
        join_handles.push(std::thread::spawn(move || {
            let mut handles = Vec::new();
            for i in 0..writes_per_thread {
                let payload = format!("lc{}-{}", t, i);
                let h = b.append(payload.as_bytes()).unwrap();
                handles.push(h);
            }
            handles
        }));
    }

    let all_handles: Vec<_> = join_handles
        .into_iter()
        .flat_map(|jh| jh.join().unwrap())
        .collect();

    // Phase 2: concurrent acknowledge
    let all_handles = Arc::new(all_handles);
    let mut ack_threads = Vec::new();
    let chunk_size = all_handles.len() / num_threads;
    for t in 0..num_threads {
        let b = Arc::clone(&backend);
        let handles = Arc::clone(&all_handles);
        ack_threads.push(std::thread::spawn(move || {
            let start = t * chunk_size;
            let end = if t == num_threads - 1 {
                handles.len()
            } else {
                start + chunk_size
            };
            for h in &handles[start..end] {
                b.acknowledge(h);
            }
        }));
    }

    for jh in ack_threads {
        jh.join().unwrap();
    }

    // Phase 3: cleanup (with 0ms decay for immediate recycling)
    let recycled = backend.cleanup_chunks(30_000, 0);

    // Should have recycled some non-active chunks
    assert!(
        recycled > 0 || all_handles.iter().all(|h| h.page_id == 0),
        "Expected chunks to be recycled (recycled={}, but not all on chunk 0)",
        recycled
    );
}
