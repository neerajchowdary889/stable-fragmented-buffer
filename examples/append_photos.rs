//! Example demonstrating photo storage and retrieval with profiling
//!
//! This example:
//! 1. Loads photos from a local directory
//! 2. Stores each photo in the blob store
//! 3. Prints the blob handles (pointers)
//! 4. Shows profiling metrics
//! 5. Retrieves and verifies each photo
//!
//! Usage: cargo run --example append_photos --release -- <photo_directory>

use stable_fragmented_buffer::{Config, PinnedBlobStore};
use std::sync::Arc;
use std::time::Duration;

fn main() {
    println!("=== Blob Store Reuse Demo ===\n");

    // 1. Create a blob store with memory-efficient config
    //    1MB pages, fast decay (500ms)
    let config = Config::memory_efficient();
    let store = Arc::new(PinnedBlobStore::new(config).expect("Failed to create blob store"));

    // 2. Start "Elastic Brain" (Automatic Cleanup)
    use stable_fragmented_buffer::lifecycle::BlobStoreLifecycleExt;
    store.start_cleanup(Duration::from_millis(100));

    println!("✅ Blob store active. Lifecycle manager running.\n");

    // Phase 1: Allocate ~10-12 pages
    println!("📦 Phase 1: Filling ~10 pages with data...");

    let item_size = 50 * 1024; // 50KB items
    let items_count = 120; // 120 * 50KB = 6MB. Paged at 512KB = ~12 pages.
    let mut handles = Vec::new();

    for i in 0..items_count {
        let data = vec![(i % 255) as u8; item_size];
        match store.append(&data) {
            Ok(handle) => handles.push(handle),
            Err(e) => eprintln!("Failed to append item {}: {:?}", i, e),
        }
    }

    let p1_stats = store.profiler().stats();
    println!("   -> Allocated {} pages", p1_stats.total_pages_allocated);
    println!("   -> Active pages: {}", p1_stats.active_pages);

    if p1_stats.total_pages_allocated < 10 {
        println!("⚠️  Warning: Expected more page allocations.");
    }

    // Phase 2: Trigger Cleanup
    println!("\n🗑️  Acknowledging ALL items to trigger cleanup...");
    for handle in &handles {
        store.acknowledge(handle);
    }

    // Drop handles to ensure no references remain (though ack should be enough)
    handles.clear();

    println!("⏳ Waiting for decay (2 seconds)...");
    std::thread::sleep(Duration::from_millis(2000));

    let cleanup_stats = store.profiler().stats();
    println!("\n🧹 Cleanup Status:");
    println!("   -> Pages Freed: {}", cleanup_stats.total_pages_freed);
    println!("   -> Active Pages: {}", cleanup_stats.active_pages);

    if cleanup_stats.total_pages_freed == 0 {
        println!("❌ NO PAGES FREED! System may not be recycling.");
    } else {
        println!("✅ System successfully freed pages.");
    }

    // Phase 3: Verify Reuse
    println!("\n♻️  Phase 2: Appending 100 NEW items...");
    println!("   Checking if they fit into previously freed page IDs...\n");

    let mut reused_count = 0;
    let mut new_alloc_count = 0;

    // We expect these new items to reuse page IDs 0, 1, 2, etc.
    // instead of allocating 13, 14, 15...
    let threshold_id = p1_stats.total_pages_allocated as u32;

    for i in 0..100 {
        let data = vec![(i % 255) as u8; item_size];
        if let Ok(handle) = store.append(&data) {
            if handle.page_id() < threshold_id {
                reused_count += 1;
            } else {
                new_alloc_count += 1;
            }
        }
    }

    let final_stats = store.profiler().stats();
    println!("📊 Final Results:");
    println!("   -> Reused Slots: {} (Target: > 0)", reused_count);
    println!("   -> New Allocations: {}", new_alloc_count);
    println!(
        "   -> Total Pages Allocated (Lifetime): {}",
        final_stats.total_pages_allocated
    );
    println!("   -> Active Pages: {}", final_stats.active_pages);

    println!("\n{}", "─".repeat(40));
    if reused_count > 0 {
        println!("🎉 SUCCESS: The system is recycling pages efficiently!");
        println!(
            "   Proof: {} items were stored in previously used page IDs.",
            reused_count
        );
    } else {
        println!("⚠️  FAILURE: No page reuse detected.");
    }
    println!("{}\n", "─".repeat(40));
}

// Helper to satisfy any remaining imports if needed, though main is self-contained.
#[allow(dead_code)]
fn dummy() {}
