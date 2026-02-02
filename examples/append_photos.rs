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
use std::env;
use std::fs;
use std::io::Read;
use std::path::Path;

fn main() {
    println!("=== Photo Storage Example ===\n");

    // Get photo directory from command line args
    let args: Vec<String> = env::args().collect();
    let photo_dir = if args.len() > 1 {
        &args[1]
    } else {
        println!("Usage: cargo run --example append_photos -- <photo_directory>");
        println!("\nNo directory provided, using current directory as default.");
        "."
    };

    println!("📂 Loading photos from: {}\n", photo_dir);

    // Create store with performance config
    let store = PinnedBlobStore::new(Config::performance()).expect("Failed to create blob store");

    println!("✅ Blob store created\n");

    // Load and store photos
    let photo_files = load_photo_files(photo_dir);

    if photo_files.is_empty() {
        println!("⚠️  No image files found in directory!");
        println!("   Supported formats: .jpg, .jpeg, .png, .gif, .bmp, .webp");
        return;
    }

    println!("📸 Found {} photos\n", photo_files.len());

    // Store photos and collect handles
    let mut handles = Vec::new();
    let mut photo_data = Vec::new();

    println!("💾 Storing photos in memory...\n");
    for (i, path) in photo_files.iter().enumerate() {
        match load_photo(path) {
            Ok(data) => {
                let size_kb = data.len() as f64 / 1024.0;

                match store.append(&data) {
                    Ok(handle) => {
                        println!(
                            "  [{}] {} ({:.2} KB)",
                            i + 1,
                            path.file_name().unwrap().to_string_lossy(),
                            size_kb
                        );
                        println!("      → Handle: {:?}", handle);

                        handles.push(handle);
                        photo_data.push(data);
                    }
                    Err(e) => {
                        eprintln!("  ❌ Failed to store {}: {:?}", path.display(), e);
                    }
                }
            }
            Err(e) => {
                eprintln!("  ❌ Failed to load {}: {}", path.display(), e);
            }
        }
    }

    println!("\n{}", "─".repeat(60));

    // Show profiling metrics
    let stats = store.profiler().stats();

    println!("\n📊 Profiling Metrics After Append:");
    println!("  Total Pages Allocated:  {}", stats.total_pages_allocated);
    println!(
        "  Total Data Stored:      {:.2} MB",
        stats.total_bytes_written as f64 / 1024.0 / 1024.0
    );
    println!("  Total Appends:          {}", stats.total_appends);
    println!(
        "  Avg Append Size:        {:.2} KB",
        stats.avg_append_size() as f64 / 1024.0
    );
    println!("  Multi-page Spans:       {}", stats.multi_page_spans);
    println!("  Total Reads:            {}", stats.total_reads);
    println!("  Uptime:                 {}s", stats.uptime_secs);

    println!("\n{}", "─".repeat(60));

    // Retrieve and verify photos
    println!("\n🔍 Retrieving and verifying photos...\n");

    let mut verified = 0;
    let mut failed = 0;

    for (i, (handle, original_data)) in handles.iter().zip(photo_data.iter()).enumerate() {
        match store.get(handle) {
            Some(retrieved_data) => {
                if &retrieved_data == original_data {
                    println!(
                        "  ✅ [{}] Verified - {} bytes match",
                        i + 1,
                        retrieved_data.len()
                    );
                    verified += 1;

                    // Acknowledge that we've consumed this data
                    store.acknowledge(handle);
                } else {
                    println!(
                        "  ❌ [{}] Mismatch - expected {} bytes, got {}",
                        i + 1,
                        original_data.len(),
                        retrieved_data.len()
                    );
                    failed += 1;
                }
            }
            None => {
                println!("  ❌ [{}] Failed to retrieve photo", i + 1);
                failed += 1;
            }
        }
    }

    println!("\n{}", "─".repeat(60));

    // Run cleanup to free acknowledged data
    println!("\n🧹 Running cleanup...");
    let empty_pages = store.cleanup_acknowledged();
    println!("  Marked {} pages as empty", empty_pages);

    println!("\n{}", "─".repeat(60));

    // Final profiling metrics
    let final_stats = store.profiler().stats();

    println!("\n📊 Final Profiling Metrics:");
    println!("  Total Reads:            {}", final_stats.total_reads);
    println!(
        "  Total Read Bytes:       {:.2} MB",
        final_stats.total_reads as f64 / 1024.0 / 1024.0
    );
    println!(
        "  Avg Read Size:          {:.2} KB",
        final_stats.avg_read_size() as f64 / 1024.0
    );
    println!("  Total Cleanups:         {}", final_stats.total_cleanups);
    println!("  Pages Freed:            {}", final_stats.total_pages_freed);
    println!(
        "  Bytes Freed:            {:.2} MB",
        final_stats.total_capacity_freed as f64 / 1024.0 / 1024.0
    );

    println!("\n{}", "─".repeat(60));

    // Summary
    println!("\n📋 Summary:");
    println!("  Photos Stored:          {}", handles.len());
    println!("  Photos Verified:        {}", verified);
    println!("  Verification Failed:    {}", failed);

    if failed == 0 && verified > 0 {
        println!("\n🎉 All photos successfully stored and verified!");
    } else if failed > 0 {
        println!("\n⚠️  Some photos failed verification");
    }
}

/// Load all photo files from a directory
fn load_photo_files(dir: &str) -> Vec<std::path::PathBuf> {
    let path = Path::new(dir);

    if !path.exists() {
        eprintln!("Directory does not exist: {}", dir);
        return Vec::new();
    }

    let mut photos = Vec::new();

    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let path = entry.path();

            if path.is_file() {
                if let Some(ext) = path.extension() {
                    let ext = ext.to_string_lossy().to_lowercase();
                    if matches!(
                        ext.as_str(),
                        "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp"
                    ) {
                        photos.push(path);
                    }
                }
            }
        }
    }

    // Sort for consistent ordering
    photos.sort();
    photos
}

/// Load a photo file into memory
fn load_photo(path: &Path) -> Result<Vec<u8>, std::io::Error> {
    let mut file = fs::File::open(path)?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;
    Ok(buffer)
}
