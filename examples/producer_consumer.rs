//! Example demonstrating producer-consumer pattern with message queue
//!
//! This example shows how to use the blob storage for inter-service communication:
//! 1. Producer appends large data to blob storage
//! 2. Producer sends small handle (<24 bytes) through message queue
//! 3. Consumer receives handle and retrieves data from blob storage

use stable_fragmented_buffer::{Config, PinnedBlobStore};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Blob Storage Producer-Consumer Example ===\n");

    // Create shared blob storage
    let config = Config::default();
    let store = Arc::new(PinnedBlobStore::new(config)?);

    // Create a message queue with 1KB limit (simulated)
    let (tx, rx) = mpsc::channel();

    // Spawn producer thread
    let producer_store = Arc::clone(&store);
    let producer = thread::spawn(move || {
        println!("[Producer] Starting...");

        for i in 0..5 {
            // Create large data (simulating a file or large payload)
            let large_data = format!(
                "Large message #{} with lots of data: {}",
                i,
                "x".repeat(1000)
            );
            let data_size = large_data.len();

            // Append to blob storage
            let handle = producer_store
                .append(large_data.as_bytes())
                .expect("Failed to append");

            println!(
                "[Producer] Appended {} bytes, got handle (size: {} bytes)",
                data_size,
                std::mem::size_of_val(&handle)
            );

            // Send handle through message queue (only 24 bytes!)
            tx.send(handle).expect("Failed to send");

            thread::sleep(std::time::Duration::from_millis(100));
        }

        println!("[Producer] Finished sending {} messages", 5);
    });

    // Spawn consumer thread
    let consumer_store = Arc::clone(&store);
    let consumer = thread::spawn(move || {
        println!("[Consumer] Starting...\n");

        let mut count = 0;
        while let Ok(handle) = rx.recv() {
            // Retrieve data using handle
            if let Some(data) = consumer_store.get(&handle) {
                let message = String::from_utf8_lossy(&data);
                println!(
                    "[Consumer] Received: {} (age: {}ms)",
                    &message[..50.min(message.len())],
                    handle.age_ms()
                );

                // Acknowledge processing
                consumer_store.acknowledge(&handle);

                count += 1;
                if count >= 5 {
                    break;
                }
            } else {
                println!("[Consumer] Handle expired or invalid");
            }
        }

        println!("\n[Consumer] Processed {} messages", count);
    });

    // Wait for both threads
    producer.join().unwrap();
    consumer.join().unwrap();

    // Print statistics
    let stats = store.stats();
    println!("\n=== Blob Storage Statistics ===");
    println!("Total pages allocated: {}", stats.page_count);
    println!("Current page ID: {}", stats.current_page_id);

    Ok(())
}
