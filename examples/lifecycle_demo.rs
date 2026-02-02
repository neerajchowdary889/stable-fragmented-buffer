//! Example: Lifecycle Manager (Automatic Cleanup)
//!
//! Demonstrates how the LifecycleManager runs in the background to automatically
//! free memory from acknowledged pages.

use stable_fragmented_buffer::lifecycle::LifecycleManager;
use stable_fragmented_buffer::{Config, PinnedBlobStore};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn main() {
    println!("=== Elastic Brain: Lifecycle Manager Demo ===\n");

    // 1. Configure for aggressive cleanup
    let config = Config {
        page_size: 1024 * 1024, // 1MB pages
        decay_timeout_ms: 100,  // Fast decay (100ms)
        default_ttl_ms: 100,    // Fast expiry (100ms for demo)
        ..Config::default()
    };

    println!("⚙️  Configured for fast decay: 100ms timeout");

    let store = Arc::new(PinnedBlobStore::new(config).unwrap());

    // 2. Start the Lifecycle Manager in the background
    let lifecycle = LifecycleManager::new(&store);
    lifecycle.start_background_cleanup(Duration::from_millis(50));
    println!("🧠 Lifecycle Manager started (running every 50ms)\n");

    // 3. Simulate Workload
    println!("📦 Writing 10 pages of data...");
    let mut handles = Vec::new();
    let data = vec![0u8; 1024 * 512]; // 512KB chunks (2 chunks per page)

    for i in 0..20 {
        let handle = store.append(&data).unwrap();
        handles.push(handle);
        if i % 2 == 0 {
            print!(".");
        }
    }
    println!("\n✅ Data written.\n");

    print_status("Before Consumption", &store);

    // 4. Consume and Acknowledge Data
    println!("🍴 Consuming and acknowledging data...");
    for handle in handles {
        // "Consume"
        let _ = store.get(&handle);
        // Acknowledge
        store.acknowledge(&handle);
    }
    println!("✅ All data acknowledged.\n");

    print_status("Immediately after Ack", &store);

    // 5. Wait for Background Cleanup
    println!("⏳ Waiting for background cleanup (1s)...");
    thread::sleep(Duration::from_secs(1));

    print_status("After Background Cleanup", &store);

    // Verify
    let stats = store.profiler().stats();
    if stats.total_pages_freed > 0 {
        println!(
            "🎉 SUCCESS: Elastic Brain automatically freed {} pages!",
            stats.total_pages_freed
        );

        // Phase 2: Verify Reuse
        println!("\n📦 Phase 2: Writing new data to verify automatic reuse...");
        let mut reused_ids = Vec::new();
        for _ in 0..5 {
            let handle = store.append(&data).unwrap();
            reused_ids.push(handle.page_id());
            print!("{} ", handle.page_id());
        }
        println!("");

        if reused_ids.iter().any(|&id| id < 5) {
            println!("♻️  PERFECT! System automatically reused freed pages.");
        } else {
            println!("⚠️  System allocated new pages (Did not reuse).");
        }
    } else {
        println!("❌ FAILURE: No pages were freed automatically.");
    }
}

fn print_status(stage: &str, store: &PinnedBlobStore) {
    let stats = store.profiler().stats();
    let store_stats = store.stats();

    println!("--- Status: {} ---", stage);
    println!("  Active Pages:      {}", store_stats.page_count);
    println!("  Total Allocated:   {}", stats.total_pages_allocated);
    println!("  Pages Freed:       {}", stats.total_pages_freed);
    println!(
        "  Bytes Freed:       {:.2} MB",
        stats.total_capacity_freed as f64 / 1024.0 / 1024.0
    );
    println!("--------------------------------\n");
}
