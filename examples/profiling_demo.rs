//! Example demonstrating profiling usage

use stable_fragmented_buffer::{Config, PinnedBlobStore};
use std::thread::sleep;
use std::time::Duration;

fn main() {
    println!("=== Profiling Example ===\n");

    // Create store (profiling is always enabled)
    let store = PinnedBlobStore::new(Config::performance()).unwrap();

    println!("✅ Store created with profiling enabled\n");

    // Append some data
    println!("📝 Appending data...");
    for i in 0..100 {
        let data = vec![42u8; 10 * 1024 * 1024]; // 10MB each
        store.append(&data).unwrap();

        if i % 10 == 0 {
            print!(".");
            std::io::Write::flush(&mut std::io::stdout()).unwrap();
        }
    }
    println!("\n");

    // Give profiler time to process events
    sleep(Duration::from_millis(100));

    // Get profiling stats
    let stats = store.profiler().stats();

    println!("📊 Profiling Statistics:");
    println!("  Total Pages:        {}", stats.total_pages);
    println!(
        "  Total Data:         {:.2} GB",
        stats.total_data_bytes as f64 / 1024.0 / 1024.0 / 1024.0
    );
    println!("  Total Appends:      {}", stats.total_appends);
    println!("  Total Reads:        {}", stats.total_reads);
    println!(
        "  Avg Append Size:    {:.2} MB",
        stats.avg_append_size() as f64 / 1024.0 / 1024.0
    );
    println!("  Multi-page Spans:   {}", stats.multi_page_spans);
    println!("  Uptime:             {}s", stats.uptime_secs);

    // Read some data
    println!("\n📖 Reading data...");
    let handle = store.append(&vec![43u8; 250 * 1024 * 1024]).unwrap();
    let _data = store.get(&handle).unwrap();

    sleep(Duration::from_millis(50));

    // Final stats
    let stats = store.profiler().stats();
    println!("\n📊 Final Statistics:");
    println!("  Total Reads:        {}", stats.total_reads);
    println!(
        "  Avg Read Size:      {:.2} MB",
        stats.avg_read_size() as f64 / 1024.0 / 1024.0
    );
}
