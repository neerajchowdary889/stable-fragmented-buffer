//! Benchmark comparing different configuration modes
//!
//! Tests three configurations:
//! - Config::default() - Balanced (64KB pages, 80% prefetch)
//! - Config::performance() - High-performance (2MB pages, 80% prefetch)
//! - Config::memory_efficient() - Memory-optimized (64KB pages, 95% prefetch)
//!
//! Workloads tested:
//! 1. Small messages (1KB each)
//! 2. Medium messages (100KB each)
//! 3. Large messages (10MB each)
//! 4. Mixed workload

use stable_fragmented_buffer::{Config, PinnedBlobStore};
use std::time::Instant;

struct BenchmarkResult {
    config_name: &'static str,
    workload: &'static str,
    total_data_mb: f64,
    append_time_ms: f64,
    get_time_ms: f64,
    throughput_mbps: f64,
    page_count: usize,
}

impl BenchmarkResult {
    fn print_header() {
        println!(
            "\n{:<20} {:<15} {:>12} {:>15} {:>15} {:>15} {:>10}",
            "Config", "Workload", "Data (MB)", "Append (ms)", "Get (ms)", "Throughput", "Pages"
        );
        println!("{}", "=".repeat(110));
    }

    fn print(&self) {
        println!(
            "{:<20} {:<15} {:>12.2} {:>15.2} {:>15.2} {:>12.2} MB/s {:>10}",
            self.config_name,
            self.workload,
            self.total_data_mb,
            self.append_time_ms,
            self.get_time_ms,
            self.throughput_mbps,
            self.page_count
        );
    }
}

fn benchmark_config(config: Config, config_name: &'static str) -> Vec<BenchmarkResult> {
    let mut results = Vec::new();

    // Workload 1: Small messages (1KB each, 1000 messages = 1MB total)
    {
        let store = PinnedBlobStore::new(config.clone()).unwrap();
        let message_size = 1024; // 1KB
        let message_count = 1000;
        let data = vec![42u8; message_size];

        let start = Instant::now();
        let mut handles = Vec::new();
        for _ in 0..message_count {
            let handle = store.append(&data).unwrap();
            handles.push(handle);
        }
        let append_time = start.elapsed();

        let start = Instant::now();
        for handle in &handles {
            let _ = store.get(handle).unwrap();
        }
        let get_time = start.elapsed();

        let total_mb = (message_size * message_count) as f64 / 1024.0 / 1024.0;
        let throughput = total_mb / append_time.as_secs_f64();

        results.push(BenchmarkResult {
            config_name,
            workload: "Small (1KB)",
            total_data_mb: total_mb,
            append_time_ms: append_time.as_secs_f64() * 1000.0,
            get_time_ms: get_time.as_secs_f64() * 1000.0,
            throughput_mbps: throughput,
            page_count: store.stats().page_count,
        });
    }

    // Workload 2: Medium messages (100KB each, 100 messages = 10MB total)
    {
        let store = PinnedBlobStore::new(config.clone()).unwrap();
        let message_size = 100 * 1024; // 100KB
        let message_count = 100;
        let data = vec![42u8; message_size];

        let start = Instant::now();
        let mut handles = Vec::new();
        for _ in 0..message_count {
            let handle = store.append(&data).unwrap();
            handles.push(handle);
        }
        let append_time = start.elapsed();

        let start = Instant::now();
        for handle in &handles {
            let _ = store.get(handle).unwrap();
        }
        let get_time = start.elapsed();

        let total_mb = (message_size * message_count) as f64 / 1024.0 / 1024.0;
        let throughput = total_mb / append_time.as_secs_f64();

        results.push(BenchmarkResult {
            config_name,
            workload: "Medium (100KB)",
            total_data_mb: total_mb,
            append_time_ms: append_time.as_secs_f64() * 1000.0,
            get_time_ms: get_time.as_secs_f64() * 1000.0,
            throughput_mbps: throughput,
            page_count: store.stats().page_count,
        });
    }

    // Workload 3: Large messages (512KB each, 40 messages = 20MB total)
    // Note: Using 512KB to fit within Performance mode's 2MB page size
    // Multi-page spanning support coming soon!
    {
        let store = PinnedBlobStore::new(config.clone()).unwrap();
        let message_size = 512 * 1024; // 512KB
        let message_count = 40;
        let data = vec![42u8; message_size];

        let start = Instant::now();
        let mut handles = Vec::new();
        for _ in 0..message_count {
            let handle = store.append(&data).unwrap();
            handles.push(handle);
        }
        let append_time = start.elapsed();

        let start = Instant::now();
        for handle in &handles {
            let _ = store.get(handle).unwrap();
        }
        let get_time = start.elapsed();

        let total_mb = (message_size * message_count) as f64 / 1024.0 / 1024.0;
        let throughput = total_mb / append_time.as_secs_f64();

        results.push(BenchmarkResult {
            config_name,
            workload: "Large (512KB)",
            total_data_mb: total_mb,
            append_time_ms: append_time.as_secs_f64() * 1000.0,
            get_time_ms: get_time.as_secs_f64() * 1000.0,
            throughput_mbps: throughput,
            page_count: store.stats().page_count,
        });
    }

    // Workload 4: Mixed workload
    {
        let store = PinnedBlobStore::new(config.clone()).unwrap();
        let small = vec![42u8; 1024]; // 1KB
        let medium = vec![43u8; 100 * 1024]; // 100KB
        let large = vec![44u8; 1024 * 1024]; // 1MB

        let start = Instant::now();
        let mut handles = Vec::new();

        // Mix: 100 small, 50 medium, 10 large = ~6.1MB
        for _ in 0..100 {
            handles.push(store.append(&small).unwrap());
        }
        for _ in 0..50 {
            handles.push(store.append(&medium).unwrap());
        }
        for _ in 0..10 {
            handles.push(store.append(&large).unwrap());
        }
        let append_time = start.elapsed();

        let start = Instant::now();
        for handle in &handles {
            let _ = store.get(handle).unwrap();
        }
        let get_time = start.elapsed();

        let total_mb = (100 * 1024 + 50 * 100 * 1024 + 10 * 1024 * 1024) as f64 / 1024.0 / 1024.0;
        let throughput = total_mb / append_time.as_secs_f64();

        results.push(BenchmarkResult {
            config_name,
            workload: "Mixed",
            total_data_mb: total_mb,
            append_time_ms: append_time.as_secs_f64() * 1000.0,
            get_time_ms: get_time.as_secs_f64() * 1000.0,
            throughput_mbps: throughput,
            page_count: store.stats().page_count,
        });
    }

    // Workload 5: Very Large - 10MB (multi-page spanning test)
    {
        let store = PinnedBlobStore::new(config.clone()).unwrap();
        let message_size = 10 * 1024 * 1024; // 10MB
        let message_count = 3;
        let data = vec![42u8; message_size];

        let start = Instant::now();
        let mut handles = Vec::new();
        for _ in 0..message_count {
            let handle = store.append(&data).unwrap();
            handles.push(handle);
        }
        let append_time = start.elapsed();

        let start = Instant::now();
        for handle in &handles {
            let retrieved = store.get(handle).unwrap();
            assert_eq!(retrieved.len(), message_size);
        }
        let get_time = start.elapsed();

        let total_mb = (message_size * message_count) as f64 / 1024.0 / 1024.0;
        let throughput = total_mb / append_time.as_secs_f64();

        results.push(BenchmarkResult {
            config_name,
            workload: "XL (10MB)",
            total_data_mb: total_mb,
            append_time_ms: append_time.as_secs_f64() * 1000.0,
            get_time_ms: get_time.as_secs_f64() * 1000.0,
            throughput_mbps: throughput,
            page_count: store.stats().page_count,
        });
    }

    // Workload 6: Huge - 100MB
    {
        let store = PinnedBlobStore::new(config.clone()).unwrap();
        let message_size = 100 * 1024 * 1024; // 100MB
        let message_count = 2;
        let data = vec![43u8; message_size];

        let start = Instant::now();
        let mut handles = Vec::new();
        for _ in 0..message_count {
            let handle = store.append(&data).unwrap();
            handles.push(handle);
        }
        let append_time = start.elapsed();

        let start = Instant::now();
        for handle in &handles {
            let retrieved = store.get(handle).unwrap();
            assert_eq!(retrieved.len(), message_size);
        }
        let get_time = start.elapsed();

        let total_mb = (message_size * message_count) as f64 / 1024.0 / 1024.0;
        let throughput = total_mb / append_time.as_secs_f64();

        results.push(BenchmarkResult {
            config_name,
            workload: "XXL (100MB)",
            total_data_mb: total_mb,
            append_time_ms: append_time.as_secs_f64() * 1000.0,
            get_time_ms: get_time.as_secs_f64() * 1000.0,
            throughput_mbps: throughput,
            page_count: store.stats().page_count,
        });
    }

    // Workload 7: Massive - 250MB (your use case!)
    {
        let store = PinnedBlobStore::new(config.clone()).unwrap();
        let message_size = 250 * 1024 * 1024; // 250MB
        let message_count = 1;
        let data = vec![44u8; message_size];

        let start = Instant::now();
        let mut handles = Vec::new();
        for _ in 0..message_count {
            let handle = store.append(&data).unwrap();
            handles.push(handle);
        }
        let append_time = start.elapsed();

        let start = Instant::now();
        for handle in &handles {
            let retrieved = store.get(handle).unwrap();
            assert_eq!(retrieved.len(), message_size);
        }
        let get_time = start.elapsed();

        let total_mb = (message_size * message_count) as f64 / 1024.0 / 1024.0;
        let throughput = total_mb / append_time.as_secs_f64();

        results.push(BenchmarkResult {
            config_name,
            workload: "XXXL (250MB)",
            total_data_mb: total_mb,
            append_time_ms: append_time.as_secs_f64() * 1000.0,
            get_time_ms: get_time.as_secs_f64() * 1000.0,
            throughput_mbps: throughput,
            page_count: store.stats().page_count,
        });
    }

    results
}

fn main() {
    println!("=== Blob Storage Configuration Benchmark ===\n");

    println!("Configuration Details:");
    println!("  Default:          64KB pages, 80% prefetch, 5s decay, 30s TTL");
    println!("  Performance:      2MB pages,  80% prefetch, 5s decay, 30s TTL");
    println!("  Memory Efficient: 64KB pages, 95% prefetch, 1s decay, 30s TTL");

    BenchmarkResult::print_header();

    // Benchmark each configuration
    let configs = vec![
        (Config::default(), "Default"),
        (Config::performance(), "Performance"),
        (Config::memory_efficient(), "Memory Efficient"),
    ];

    let mut all_results = Vec::new();

    for (config, name) in configs {
        let results = benchmark_config(config, name);
        for result in results {
            result.print();
            all_results.push(result);
        }
    }

    // Summary analysis
    println!("\n{}", "=".repeat(110));
    println!("\n📊 Summary Analysis:\n");

    // Find best for each workload
    let workloads = [
        "Small (1KB)",
        "Medium (100KB)",
        "Large (512KB)",
        "Mixed",
        "XL (10MB)",
        "XXL (100MB)",
        "XXXL (250MB)",
    ];

    for workload in &workloads {
        let workload_results: Vec<_> = all_results
            .iter()
            .filter(|r| r.workload == *workload)
            .collect();

        let best_throughput = workload_results
            .iter()
            .max_by(|a, b| a.throughput_mbps.partial_cmp(&b.throughput_mbps).unwrap())
            .unwrap();

        let best_memory = workload_results
            .iter()
            .min_by_key(|r| r.page_count)
            .unwrap();

        println!("{}:", workload);
        println!(
            "  🚀 Best Throughput: {} ({:.2} MB/s)",
            best_throughput.config_name, best_throughput.throughput_mbps
        );
        println!(
            "  💾 Best Memory:     {} ({} pages)",
            best_memory.config_name, best_memory.page_count
        );
        println!();
    }

    // Recommendations
    println!("💡 Recommendations:\n");
    println!("  • Use Performance mode for:");
    println!("    - Large messages (>1MB)");
    println!("    - High throughput requirements");
    println!("    - Systems with abundant RAM");
    println!();
    println!("  • Use Memory Efficient mode for:");
    println!("    - Small messages (<10KB)");
    println!("    - Memory-constrained environments");
    println!("    - Mixed workloads with many small messages");
    println!();
    println!("  • Use Default mode for:");
    println!("    - General-purpose workloads");
    println!("    - Unknown message size distribution");
    println!("    - Balanced performance/memory trade-off");
}
