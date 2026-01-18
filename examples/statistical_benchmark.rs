//! Benchmark with statistical analysis (p50, p90, p99)
//!
//! Runs multiple iterations to get accurate percentile measurements

use stable_fragmented_buffer::{Config, PinnedBlobStore};
use std::time::Instant;

struct BenchmarkResult {
    config_name: &'static str,
    workload: &'static str,
    total_data_mb: f64,
    append_p50_ms: f64,
    append_p90_ms: f64,
    append_p99_ms: f64,
    get_p50_ms: f64,
    get_p90_ms: f64,
    get_p99_ms: f64,
    throughput_p50_mbps: f64,
    page_count: usize,
}

impl BenchmarkResult {
    fn print_header() {
        println!(
            "\n{:<20} {:<15} {:>10} {:>12} {:>12} {:>12} {:>15} {:>8}",
            "Config",
            "Workload",
            "Data (MB)",
            "p50 (ms)",
            "p90 (ms)",
            "p99 (ms)",
            "p50 Throughput",
            "Pages"
        );
        println!("{}", "=".repeat(115));
    }

    fn print(&self) {
        println!(
            "{:<20} {:<15} {:>10.2} {:>12.2} {:>12.2} {:>12.2} {:>12.2} MB/s {:>8}",
            self.config_name,
            self.workload,
            self.total_data_mb,
            self.append_p50_ms,
            self.append_p90_ms,
            self.append_p99_ms,
            self.throughput_p50_mbps,
            self.page_count
        );
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    let idx = (p * (sorted.len() as f64 - 1.0)) as usize;
    sorted[idx]
}

fn benchmark_workload(
    config: Config,
    config_name: &'static str,
    workload_name: &'static str,
    iterations: usize,
    data_generator: impl Fn() -> (Vec<Vec<u8>>, usize), // Returns (data_vec, total_bytes)
) -> BenchmarkResult {
    let mut append_times = Vec::with_capacity(iterations);
    let mut get_times = Vec::with_capacity(iterations);
    let mut page_count = 0;

    for _ in 0..iterations {
        let store = PinnedBlobStore::new(config.clone()).unwrap();
        let (data_vec, total_bytes) = data_generator();

        // Measure append
        let start = Instant::now();
        let mut handles = Vec::new();
        for data in &data_vec {
            let handle = store.append(data).unwrap();
            handles.push(handle);
        }
        append_times.push(start.elapsed().as_secs_f64() * 1000.0);

        // Measure get
        let start = Instant::now();
        for handle in &handles {
            let _ = store.get(handle).unwrap();
        }
        get_times.push(start.elapsed().as_secs_f64() * 1000.0);

        page_count = store.stats().page_count;
    }

    // Sort for percentiles
    append_times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    get_times.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let (_, total_bytes) = data_generator();
    let total_mb = total_bytes as f64 / 1024.0 / 1024.0;

    BenchmarkResult {
        config_name,
        workload: workload_name,
        total_data_mb: total_mb,
        append_p50_ms: percentile(&append_times, 0.50),
        append_p90_ms: percentile(&append_times, 0.90),
        append_p99_ms: percentile(&append_times, 0.99),
        get_p50_ms: percentile(&get_times, 0.50),
        get_p90_ms: percentile(&get_times, 0.90),
        get_p99_ms: percentile(&get_times, 0.99),
        throughput_p50_mbps: total_mb / (percentile(&append_times, 0.50) / 1000.0),
        page_count,
    }
}

fn main() {
    println!("=== Blob Storage Statistical Benchmark (p50/p90/p99) ===\n");
    println!("Running 20 iterations per workload for statistical accuracy...\n");

    let iterations = 20;

    let configs = vec![
        (Config::default(), "Default"),
        (Config::performance(), "Performance"),
        (Config::memory_efficient(), "Memory Efficient"),
    ];

    // Define workloads
    let workloads: Vec<(&str, Box<dyn Fn() -> (Vec<Vec<u8>>, usize)>)> = vec![
        (
            "Small (1KB)",
            Box::new(|| {
                let data = vec![vec![42u8; 1024]; 1000];
                let total = 1024 * 1000;
                (data, total)
            }),
        ),
        (
            "Medium (100KB)",
            Box::new(|| {
                let data = vec![vec![42u8; 100 * 1024]; 100];
                let total = 100 * 1024 * 100;
                (data, total)
            }),
        ),
        (
            "Large (512KB)",
            Box::new(|| {
                let data = vec![vec![42u8; 512 * 1024]; 40];
                let total = 512 * 1024 * 40;
                (data, total)
            }),
        ),
        (
            "XL (10MB)",
            Box::new(|| {
                let data = vec![vec![42u8; 10 * 1024 * 1024]; 3];
                let total = 10 * 1024 * 1024 * 3;
                (data, total)
            }),
        ),
        (
            "XXL (100MB)",
            Box::new(|| {
                let data = vec![vec![43u8; 100 * 1024 * 1024]; 2];
                let total = 100 * 1024 * 1024 * 2;
                (data, total)
            }),
        ),
        (
            "XXXL (250MB)",
            Box::new(|| {
                let data = vec![vec![44u8; 250 * 1024 * 1024]; 1];
                let total = 250 * 1024 * 1024;
                (data, total)
            }),
        ),
    ];

    BenchmarkResult::print_header();

    for (config, config_name) in &configs {
        for (workload_name, generator) in &workloads {
            let result = benchmark_workload(
                config.clone(),
                config_name,
                workload_name,
                iterations,
                || generator(),
            );
            result.print();
        }
        println!(); // Blank line between configs
    }

    println!("\n{}", "=".repeat(115));
    println!("\n📊 Key Insights:");
    println!("  • p50 (median): Typical performance");
    println!("  • p90: 90% of requests faster than this");
    println!("  • p99: 99% of requests faster than this (tail latency)");
    println!("\n  Lower p99 = more consistent performance");
    println!("  High p99 = occasional slowdowns (GC, page faults, etc.)");
}
