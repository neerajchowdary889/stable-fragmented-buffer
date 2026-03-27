//! Shared Backend Lifecycle Benchmark
//!
//! Tests the full lifecycle of the shared-memory backend:
//!
//! 1. **Dynamic chunk creation** — writes >1 MB chunks to force multi-chunk
//! 2. **Acknowledge + cleanup** — acknowledges all entries to trigger chunk recycling
//! 3. **Chunk reuse** — verifies new writes reuse recycled chunk IDs (not infinite growth)
//! 4. **Auto-cleanup** — Drop impl handles shm_unlink automatically
//! 5. **P50/P90/P99 latencies** — for append, resolve, and acknowledge
//!
//! Run: `cargo run --example shared_lifecycle_benchmark --release`

use stable_fragmented_buffer::{Config, OverflowHandle, PinnedBlobStore};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ── Configuration ────────────────────────────────────────────────────────

/// Chunk size: 1 MB (small for faster testing — use 32 MB in production)
const CHUNK_SIZE: usize = 1 * 1024 * 1024; // 1 MB
/// Payload size per message
const PAYLOAD_SIZE: usize = 4 * 1024; // 4 KB
/// Total data to push (must exceed CHUNK_SIZE to trigger multi-chunk)
const TOTAL_DATA: usize = 5 * 1024 * 1024; // 5 MB → forces ~5 chunks with 1 MB each
/// How many messages that is
const NUM_MESSAGES: usize = TOTAL_DATA / PAYLOAD_SIZE;
/// SHM namespace (short for macOS compat)
const NAMESPACE: &str = "sbench";

fn main() {
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║        Shared Backend Lifecycle Benchmark                   ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();
    println!("  Chunk size:     {} KB", CHUNK_SIZE / 1024);
    println!("  Payload size:   {} KB", PAYLOAD_SIZE / 1024);
    println!("  Total data:     {} MB", TOTAL_DATA / (1024 * 1024));
    println!("  Messages:       {}", NUM_MESSAGES);
    println!();

    let config = Config {
        default_ttl_ms: 60_000,     // 60s TTL
        decay_timeout_ms: 500,      // 500ms decay
        ..Config::default()
    };

    // ── Phase 1: Create store + append (trigger multi-chunk) ─────────

    println!("━━━ Phase 1: Append {} messages (trigger multi-chunk creation) ━━━", NUM_MESSAGES);

    let store = Arc::new(
        PinnedBlobStore::new_shared(config.clone(), NAMESPACE, CHUNK_SIZE)
            .expect("Failed to create shared store"),
    );

    let payload = vec![0xABu8; PAYLOAD_SIZE];
    let mut handles: Vec<OverflowHandle> = Vec::with_capacity(NUM_MESSAGES);
    let mut append_times: Vec<Duration> = Vec::with_capacity(NUM_MESSAGES);

    let phase1_start = Instant::now();
    for _ in 0..NUM_MESSAGES {
        let start = Instant::now();
        let handle = store.append_shared(&payload).expect("append_shared failed");
        append_times.push(start.elapsed());
        handles.push(handle);
    }
    let phase1_elapsed = phase1_start.elapsed();

    let chunks_used = handles.last().unwrap().page_id + 1;

    println!("  ✓ Appended {} messages in {:.2?}", NUM_MESSAGES, phase1_elapsed);
    println!("  ✓ Chunks created: {} (expected ≥ {})", chunks_used, TOTAL_DATA / CHUNK_SIZE);
    println!("  ✓ Throughput: {:.1} MB/s", (TOTAL_DATA as f64 / 1e6) / phase1_elapsed.as_secs_f64());

    append_times.sort();
    print_percentiles("  Append", &append_times);
    println!();

    // ── Phase 2: Resolve all handles (zero-copy read) ────────────────

    println!("━━━ Phase 2: Resolve {} handles (zero-copy read) ━━━", NUM_MESSAGES);

    let mut resolve_times: Vec<Duration> = Vec::with_capacity(NUM_MESSAGES);
    let mut resolve_ok = 0usize;
    let mut resolve_fail = 0usize;

    let phase2_start = Instant::now();
    for handle in &handles {
        let start = Instant::now();
        match store.resolve(handle) {
            Some(data) => {
                assert_eq!(data.len(), PAYLOAD_SIZE, "Resolved data has wrong size");
                assert_eq!(data[0], 0xAB, "Resolved data has wrong content");
                resolve_ok += 1;
            }
            None => {
                resolve_fail += 1;
            }
        }
        resolve_times.push(start.elapsed());
    }
    let phase2_elapsed = phase2_start.elapsed();

    println!("  ✓ Resolved {} ok, {} failed in {:.2?}", resolve_ok, resolve_fail, phase2_elapsed);
    println!("  ✓ Read throughput: {:.1} MB/s", (TOTAL_DATA as f64 / 1e6) / phase2_elapsed.as_secs_f64());

    resolve_times.sort();
    print_percentiles("  Resolve", &resolve_times);
    println!();

    // ── Phase 3: Simulate second process (attach + resolve) ──────────

    println!("━━━ Phase 3: Attach as 'Process B' and resolve ━━━");

    let store_b = Arc::new(
        PinnedBlobStore::attach_shared(config.clone(), NAMESPACE)
            .expect("Failed to attach shared store"),
    );

    let mut cross_process_ok = 0usize;
    let phase3_start = Instant::now();
    for handle in &handles {
        if let Some(data) = store_b.resolve(handle) {
            assert_eq!(data.len(), PAYLOAD_SIZE);
            cross_process_ok += 1;
        }
    }
    let phase3_elapsed = phase3_start.elapsed();

    println!("  ✓ Process B resolved {}/{} handles in {:.2?}", cross_process_ok, NUM_MESSAGES, phase3_elapsed);

    // Drop Process B's reference before cleanup
    drop(store_b);
    println!();

    // ── Phase 4: Acknowledge all entries ─────────────────────────────

    println!("━━━ Phase 4: Acknowledge all {} entries ━━━", NUM_MESSAGES);

    let mut ack_times: Vec<Duration> = Vec::with_capacity(NUM_MESSAGES);
    let phase4_start = Instant::now();
    for handle in &handles {
        let start = Instant::now();
        store.acknowledge_shared(handle);
        ack_times.push(start.elapsed());
    }
    let phase4_elapsed = phase4_start.elapsed();

    println!("  ✓ Acknowledged {} entries in {:.2?}", NUM_MESSAGES, phase4_elapsed);
    ack_times.sort();
    print_percentiles("  Ack", &ack_times);
    println!();

    // ── Phase 5: Trigger cleanup + verify chunk recycling ────────────

    println!("━━━ Phase 5: Cleanup (recycle fully-acknowledged chunks) ━━━");

    println!("  ⏳ Waiting 600ms for decay timeout...");
    std::thread::sleep(Duration::from_millis(600));

    let recycled = store.cleanup_shared();
    println!("  ✓ Recycled {} chunks", recycled);

    let mut stale_count = 0;
    for handle in &handles {
        if store.resolve(handle).is_none() {
            stale_count += 1;
        }
    }
    println!(
        "  ✓ After cleanup: {}/{} handles correctly invalidated",
        stale_count, NUM_MESSAGES
    );
    println!();

    // ── Phase 6: Re-use recycled chunks (verify chunk IDs are reused) ─

    println!("━━━ Phase 6: Write new data into recycled chunks ━━━");

    let new_payload = vec![0xCDu8; PAYLOAD_SIZE];
    let mut new_handles = Vec::with_capacity(100);
    for _ in 0..100 {
        let handle = store.append_shared(&new_payload).expect("append after recycle failed");
        new_handles.push(handle);
    }

    // Verify chunk IDs are being REUSED (not new IDs like 6, 7, 8...)
    let max_new_chunk_id = new_handles.iter().map(|h| h.page_id).max().unwrap();
    let reused_old_chunks = new_handles.iter().any(|h| h.page_id < chunks_used);

    let mut new_ok = 0;
    for handle in &new_handles {
        if let Some(data) = store.resolve(handle) {
            assert_eq!(data[0], 0xCD);
            new_ok += 1;
        }
    }
    println!("  ✓ Wrote and resolved {}/100 new messages after recycling", new_ok);
    println!("  ✓ Max chunk ID used: {} (was {} before recycling)", max_new_chunk_id, chunks_used - 1);
    println!("  ✓ Reused old chunk IDs: {}", if reused_old_chunks { "YES ✓" } else { "NO ✗" });
    println!();

    // ── Summary ──────────────────────────────────────────────────────

    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║                     BENCHMARK SUMMARY                      ║");
    println!("╠══════════════════════════════════════════════════════════════╣");
    println!("║  Chunks created:       {:>4}                                ║", chunks_used);
    println!("║  All appends ok:       {:>4}                                ║", handles.len() == NUM_MESSAGES);
    println!("║  All resolves ok:      {:>4}                                ║", resolve_ok == NUM_MESSAGES);
    println!("║  Cross-proc resolves:  {:>4}                                ║", cross_process_ok == NUM_MESSAGES);
    println!("║  Chunks recycled:      {:>4}                                ║", recycled);
    println!("║  Stale after cleanup:  {:>4}                                ║", stale_count);
    println!("║  Chunk IDs reused:     {:>4}                                ║", reused_old_chunks);
    println!("║  Re-use after recycle: {:>4}                                ║", new_ok == 100);
    println!("╚══════════════════════════════════════════════════════════════╝");

    // Drop triggers automatic shm_unlink — no manual cleanup needed!
    drop(store);
    println!("\n  🧹 Auto-cleanup via Drop. Done!\n");
}

// ── Helpers ──────────────────────────────────────────────────────────────

fn print_percentiles(label: &str, sorted_times: &[Duration]) {
    if sorted_times.is_empty() {
        return;
    }
    let p50 = sorted_times[sorted_times.len() * 50 / 100];
    let p90 = sorted_times[sorted_times.len() * 90 / 100];
    let p99 = sorted_times[sorted_times.len() * 99 / 100];
    let max = sorted_times[sorted_times.len() - 1];

    println!(
        "{}  p50={:.1}µs  p90={:.1}µs  p99={:.1}µs  max={:.1}µs",
        label,
        p50.as_nanos() as f64 / 1000.0,
        p90.as_nanos() as f64 / 1000.0,
        p99.as_nanos() as f64 / 1000.0,
        max.as_nanos() as f64 / 1000.0,
    );
}
