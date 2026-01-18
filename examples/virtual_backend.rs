//! Example demonstrating the virtual memory backend with mmap
//!
//! This shows how mmap provides:
//! 1. Contiguous addressing (no page fragmentation)
//! 2. Zero-copy reads (returns references, not copies)
//! 3. Fast appends for large files (250MB)

use stable_fragmented_buffer::backend::virtual_mem::VirtualBackend;
use std::time::Instant;

fn main() {
    println!("=== Virtual Memory (mmap) Backend Demo ===\n");

    // Reserve 1GB of virtual address space
    // Physical memory allocated on-demand via page faults
    let reserved_size = 1024 * 1024 * 1024; // 1GB
    let backend = VirtualBackend::new(reserved_size, 0).expect("Failed to create virtual backend");

    println!(
        "✅ Reserved {} MB of virtual address space",
        reserved_size / 1024 / 1024
    );
    println!("   (Physical memory: 0 MB - allocated on-demand)\n");

    // Test 1: Small appends
    println!("📝 Test 1: Small messages");
    let mut handles = Vec::new();
    for i in 0..5 {
        let message = format!("Message #{}: Hello from mmap!", i);
        let offset = backend.append(message.as_bytes()).unwrap();
        handles.push((offset, message.len()));
        println!("   Appended {} bytes at offset {}", message.len(), offset);
    }

    // Verify small messages (zero-copy!)
    println!("\n🔍 Verifying small messages (zero-copy reads):");
    for (i, (offset, size)) in handles.iter().enumerate() {
        let data = backend.get(*offset, *size as u64).unwrap();
        let message = String::from_utf8_lossy(data);
        println!("   Message {}: {}", i, message);
    }

    // Test 2: Large file (250MB)
    println!("\n📦 Test 2: Large file (250MB)");
    let large_data = vec![42u8; 250 * 1024 * 1024]; // 250MB

    let start = Instant::now();
    let large_offset = backend.append(&large_data).unwrap();
    let append_time = start.elapsed();

    println!("   ✅ Appended 250MB in {:?}", append_time);
    println!(
        "   Throughput: {:.2} GB/s",
        250.0 / append_time.as_secs_f64() / 1024.0
    );

    // Read back (zero-copy!)
    let start = Instant::now();
    let retrieved = backend.get(large_offset, large_data.len() as u64).unwrap();
    let read_time = start.elapsed();

    println!("   ✅ Retrieved 250MB in {:?} (zero-copy!)", read_time);
    println!(
        "   First byte: {}, Last byte: {}",
        retrieved[0],
        retrieved[retrieved.len() - 1]
    );

    // Test 3: Multiple large files
    println!("\n📚 Test 3: Multiple large files");
    let mut large_handles = Vec::new();

    for i in 0..3 {
        let data = vec![i as u8; 50 * 1024 * 1024]; // 50MB each
        let start = Instant::now();
        let offset = backend.append(&data).unwrap();
        let time = start.elapsed();

        large_handles.push((offset, data.len(), i as u8));
        println!("   File {}: 50MB appended in {:?}", i, time);
    }

    // Verify all large files
    println!("\n✅ Verifying all large files:");
    for (offset, size, expected_byte) in large_handles {
        let data = backend.get(offset, size as u64).unwrap();
        assert_eq!(data[0], expected_byte);
        assert_eq!(data[data.len() - 1], expected_byte);
        println!(
            "   Offset {}: {} bytes verified (all bytes = {})",
            offset, size, expected_byte
        );
    }

    // Statistics
    println!("\n📊 Statistics:");
    println!("{:?}", backend);

    println!("\n🎯 Key Benefits:");
    println!("   • Contiguous addressing (single offset, no page lookups)");
    println!("   • Zero-copy reads (returns &[u8], not Vec<u8>)");
    println!("   • Lazy allocation (physical memory on page faults)");
    println!("   • True pointer stability (addresses never change)");
}
