//! Example: Recycling Demo
//!
//! Demonstrates that the Elastic Brain not only frees memory, but reuses Page IDs
//! to keep the active set compact (Circular Buffer / Hole Filling strategy).

use stable_fragmented_buffer::{Config, PinnedBlobStore};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn main() {
    println!("=== Elastic Brain: Page Recycling Demo ===\n");

    // 1. Configure for aggressive cleanup
    let config = Config {
        page_size: 1024 * 1024, // 1MB pages
        decay_timeout_ms: 50,   // Very fast decay
        default_ttl_ms: 50,     // Very fast expiry
        ..Config::default()
    };

    let store = Arc::new(PinnedBlobStore::new(config).unwrap());

    // 2. Write 5 pages of data (IDs 0..4)
    println!("📦 Phase 1: Allocating 5 pages...");
    let mut handles = Vec::new();
    let data = vec![0u8; 1024 * 512]; // 512KB

    for i in 0..10 {
        // 10 chunks = 5 pages
        let handle = store.append(&data).unwrap();
        handles.push(handle);
        print!("{} ", handle.page_id());
    }
    println!(
        "\n✅ Phase 1 Complete. Active High Water Mark: {}\n",
        store.stats().current_page_id
    );

    // 3. Acknowledge everything to allow cleanup
    println!("🗑️  Acknowledging all data...");
    for handle in handles {
        let _ = store.get(&handle);
        store.acknowledge(&handle);
    }

    // 4. Run cleanup (Manual for deterministic demo)
    // Pass 1: Marks pages as empty (sets timestamp)
    println!("🧹 Cleanup Pass 1 (Marking empty)...");
    store.cleanup_acknowledged();

    println!("⏳ Waiting for decay (200ms)...");
    thread::sleep(Duration::from_millis(200));

    // Pass 2: Actually removes decayed pages
    let freed = store.cleanup_acknowledged();
    println!("🧹 Cleanup ran. Pages freed: {}", freed);

    print_status("After Cleanup", &store);

    // 5. Write NEW data. It should reuse Page 0, 1, 2... NOT Page 5!
    println!("📦 Phase 2: Allocating new data. Expecting LOW Page IDs...");
    let mut new_ids = Vec::new();
    for _ in 0..4 {
        let handle = store.append(&data).unwrap();
        new_ids.push(handle.page_id());
        print!("{} ", handle.page_id());
    }
    println!("\n");

    // Verification
    let reused = new_ids.iter().any(|&id| id < 5);
    if reused {
        println!("🎉 SUCCESS: System reused page IDs: {:?}", new_ids);
    } else {
        println!("❌ FAILURE: System used new page IDs: {:?}", new_ids);
    }
}

fn print_status(stage: &str, store: &PinnedBlobStore) {
    let stats = store.profiler().stats();
    let store_stats = store.stats();

    println!("--- Status: {} ---", stage);
    println!("  Active Pages:      {}", store_stats.page_count);
    println!("  Current Page ID:   {}", store_stats.current_page_id);
    println!("  Total Allocated:   {}", stats.total_pages_allocated);
    println!("  Pages Freed:       {}", stats.total_pages_freed);
    println!("--------------------------------\n");
}
