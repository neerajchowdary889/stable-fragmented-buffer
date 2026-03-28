# Project Documentation: Pinned Page Store

## 1. Executive Summary

**stable fragmented buffer** is a high-performance, in-memory blob storage system designed for Rust. It addresses a specific systems programming challenge: growing a memory buffer dynamically without invalidating existing pointers to that data.

Unlike standard dynamic arrays (e.g., `std::vector` in C++ or Rust), which reallocate and move memory upon growth, Pinned Page Store ensures **Pointer Stability**. Once data is written, its physical address never changes. The primary production mode is **SharedBackend** — a cross-process arena backed by POSIX shared memory (`/dev/shm`) that lets multiple processes read and write the same data with zero-copy access and no OS-level IPC on the hot path.

---

## 2. Core Architecture

The system follows a **Layered Monolith** pattern, separating the public safety guarantees from the internal complex allocation strategies.

#### 2.1 Architectural Layers

1. **The Public Facade:** A type-safe Rust API ensuring thread safety and pointer lifetimes.
2. **The Elastic Brain:** A logic layer that manages "when" to allocate or deallocate, decoupled from the actual "write" operations.
3. **The Storage Backend:** An abstract layer. The primary backend is **SharedBackend** — a cross-process `/dev/shm` arena. A single-process **SegmentedBackend** is also available for local use.

### 2.2 System Diagram

```plaintext
+-------------------------------------------------------------+
|               PinnedBlobStore (Public API)                  |
|  append_shared(data) -> OverflowHandle  (creator/attacher)  |
|  resolve(&handle) -> &[u8]             (zero-copy mmap)     |
|  acknowledge_shared(&handle)           (mark consumed)      |
|  cleanup_shared()                      (recycle chunks)     |
+------------------------------+------------------------------+
                               |
                               v
+-------------------------------------------------------------+
|              Elastic Lifecycle Manager (The Brain)          |
|                                                             |
|   +-----------------------+     +-----------------------+   |
|   |   GrowthController    |     |     DecayManager      |   |
|   | (Rapid Scale-Up)      |     | (Slow Scale-Down)     |   |
|   +-----------------------+     +-----------------------+   |
|   | Trigger:              |     | Trigger:              |   |
|   |  Chunk full           |     |  Chunk fully acked AND|   |
|   |  → allocate new chunk |     |  Age > decay_timeout  |   |
|   +----------+------------+     +-----------+-----------+   |
|              |                              |               |
|              v (new /dev/shm chunk)         v (recycle)     |
+--------------+------------------------------+---------------+
                               |
                               v
+-------------------------------------------------------------+
|            SharedBackend (Primary — /dev/shm)               |
|                                                             |
|  /dev/shm/{ns}_ctrl          128-byte control file         |
|    magic · version · chunk_size                             |
|    write_head (AtomicU32) · chunk_count (AtomicU32)         |
|    generation (AtomicU32)                                   |
|                                                             |
|  /dev/shm/{ns}_data_0        32 MB data chunk              |
|  /dev/shm/{ns}_data_1        32 MB data chunk              |
|  ...                                                        |
|                                                             |
|  Each chunk header (64 bytes):                              |
|    used · generation · entry_count · ack_count · empty_since|
|                                                             |
|  Synchronisation: atomics embedded in mmap (no syscalls)   |
+-------------------------------------------------------------+
```

---

## 3. Key Features & Algorithms

### 3.1 Pointer Stability

Standard vectors grow by allocating a larger block and moving all data to it, invalidating old pointers. Pinned Page Store grows by allocating new fixed-size chunks.

* **Benefit:** An `OverflowHandle` obtained in one process resolves to valid data in any attached process until the chunk is recycled.
* **Mechanism:** Data is never moved after it is written into a chunk.

### 3.2 Elastic Scaling (Hysteresis)

To ensure consistent latency, the system employs an asymmetric growth/decay strategy.

* **Rapid Scale-Up:** When the active chunk is full, the writer immediately allocates a new `/dev/shm` chunk (or reuses a recycled one) and advances `write_head`.
* **Slow Scale-Down (The Decay Rule):** A chunk whose `ack_count >= entry_count` is not freed immediately. It is only recycled (header reset, new generation) after it has been fully acknowledged for at least `decay_timeout_ms` (default 5 s). This prevents thrashing at workload boundaries.
* **Chunk Recycling:** Before allocating a fresh chunk, the writer scans for existing chunks with `used == 0 && entry_count == 0` and reuses them, keeping the file count compact.

### 3.3 SharedBackend — Cross-Process Design

All synchronisation happens via atomics embedded directly in the mmap region — no mutexes, semaphores, or futexes on the hot write path.

#### Control File Layout (`{ns}_ctrl`, 128 bytes)

| Offset | Field         | Size | Description                          |
|--------|---------------|------|--------------------------------------|
| 0      | `magic`       | 8    | `0x444D58505F4F5646` ("DMXP_OVF")   |
| 8      | `version`     | 4    | Protocol version (1)                 |
| 12     | `chunk_size`  | 4    | Bytes per data chunk                 |
| 16     | `write_head`  | 4    | Active chunk ID (`AtomicU32`)        |
| 20     | `chunk_count` | 4    | Highest allocated chunk + 1          |
| 24     | `generation`  | 4    | Global generation counter            |
| 28     | _(reserved)_  | 100  | Pad to 128 bytes                     |

#### Chunk Header Layout (first 64 bytes of each `{ns}_data_N`)

| Offset | Field          | Size | Description                              |
|--------|----------------|------|------------------------------------------|
| 0      | `used`         | 4    | Bytes written (`AtomicU32`, CAS target)  |
| 4      | `generation`   | 4    | Recycling generation (`AtomicU32`)       |
| 8      | `entry_count`  | 4    | Total entries appended (`AtomicU32`)     |
| 12     | `ack_count`    | 4    | Acknowledged entries (`AtomicU32`)       |
| 16     | `empty_since`  | 8    | Timestamp when fully acked (`AtomicU64`) |
| 24     | _(reserved)_   | 40   | Pad to 64 bytes                          |

Data starts at byte 64 of each chunk.

---

## 4. Configuration Specification

| Parameter              | Recommended (Performance) | Recommended (Memory) | Description                          |
|------------------------|---------------------------|----------------------|--------------------------------------|
| `page_size`            | 2 MB                      | 512 KB               | Heap page size (single-process mode) |
| `prefetch_threshold`   | 0.8 (80%)                 | 0.95 (95%)           | When to trigger next allocation      |
| `decay_timeout_ms`     | 5000 ms                   | 1000 ms              | How long to keep empty chunks alive  |
| `default_ttl_ms`       | 30 000 ms                 | 10 000 ms            | Handle TTL before expiry             |
| `chunk_size` (shared)  | 32 MB (default)           | 4–8 MB               | Size of each `/dev/shm` data chunk   |
