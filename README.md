# Project Documentation: Pinned Page Store

## 1. Executive Summary

**stable fragmented buffer** is a high-performance, in-memory blob storage system designed for Rust. It addresses a specific systems programming challenge: growing a memory buffer dynamically without invalidating existing pointers to that data.

Unlike standard dynamic arrays (e.g., `std::vector` in C++ or Rust), which reallocate and move memory upon growth, Pinned Page Store ensures **Pointer Stability**. Once data is written, its physical address never changes. It combines this stability with an **Elastic Lifecycle Manager** that prefetches memory to prevent latency spikes and lazily releases memory to prevent thrashing.

---

## 2. Core Architecture

The system follows a **Layered Monolith** pattern, separating the public safety guarantees from the internal complex allocation strategies.

### 2.1 Architectural Layers

1. **The Public Facade:** A type-safe Rust API ensuring thread safety and pointer lifetimes.
2. **The Elastic Brain:** A logic layer that manages "when" to allocate or deallocate, decoupled from the actual "write" operations.
3. **The Storage Backend:** An abstract layer supporting two distinct memory strategies (Segmented Heap vs. Virtual Memory).

### 2.2 System Diagram

```plaintext
+-------------------------------------------------------------+
|               PinnedBlobStore (Public API)                  |
|  - append(data) -> &Data    (Safe, Stable Pointer)          |
|  - get_ref(offset) -> &Data (O(1) Random Access)            |
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
|   |  Usage > 80% AND      |     |  Page Empty AND       |   |
|   |  Next Page is Null    |     |  Age > 5 seconds      |   |
|   +----------+------------+     +-----------+-----------+   |
|              |                              |               |
|              v (Prefetch)                   v (Drop)        |
+--------------+------------------------------+---------------+
               |                              |
               +---------------+--------------+
                               |
                               v
+-------------------------------------------------------------+
|                Storage Backend (The Muscle)                 |
|                                                             |
|   [ PageDirectory (Vec<Page>) ]                             |
|                                                             |
|           (Polymorphism / Strategy Trait)                   |
|          +--------------------------+                       |
|          |                          |                       |
+----------v-----------+    +---------v-----------+           |
|  Mode A: Segmented   |    |   Mode B: Virtual   |           |
|  (Memory Efficient)  |    | (Performance Heavy) |           |
+----------------------+    +---------------------+           |
| - Linked 64KB Blocks |    | - Contiguous 1TB    |           |
| - Heap (Box::new)    |    | - OS Commit (mmap)  |           |
| - "Skip" overflow    |    | - No fragmentation  |           |
+----------------------+    +---------------------+           |
+-------------------------------------------------------------+

```

---

## 3. Key Features & Algorithms

### 3.1 Pointer Stability

Standard vectors grow by allocating a larger block and moving all data to it, invalidating old pointers. Pinned Page Store grows by chaining new fixed-size blocks (pages).

* **Benefit:** A pointer `&MyStruct` obtained at startup remains valid until the store is dropped.
* **Mechanism:** Data is never moved, only appended.

### 3.2 Elastic Scaling (Hysteresis)

To ensure consistent latency, the system employs an asymmetric growth/decay strategy.

* **Rapid Scale-Up (The 80% Rule):**
* *Goal:* Eliminate allocation latency during high-throughput writes.
* *Algorithm:* During a write to `Page N`, if usage exceeds **80%** and `Page N+1` does not exist, the system triggers an immediate prefetch of `Page N+1`.
* *Result:* The next page is allocated and cache-hot before the writer needs it.


* **Slow Scale-Down (The Decay Rule):**
* *Goal:* Prevent "thrashing" (allocating and freeing repeatedly at the boundary).
* *Algorithm:* If a page is empty, it is not freed immediately. It is marked with a timestamp. It is only dropped if it remains empty for a configurable duration (e.g., 5 seconds).



### 3.3 Dual Storage Modes

The system can be configured at compile-time or initialization for two distinct use cases.

#### Mode A: Segmented (Memory Efficient)

* **Implementation:** `Vec<Box<[u8]>>`
* **Behavior:** Allocates distinct 4KB or 64KB chunks on the heap.
* **Contiguity:** Logically contiguous, physically fragmented.
* **Overflow:** Uses a "Skip-and-Pad" strategy. If an object does not fit in the remaining space of the current page, the remaining space is skipped (wasted), and the object is written to the start of a new page to ensure the object itself is contiguous.

#### Mode B: Virtual (Performance Heavy)

* **Implementation:** `mmap` (Linux/macOS) or `VirtualAlloc` (Windows).
* **Behavior:** Reserves a massive virtual address space (e.g., 1TB) but only commits physical RAM as needed.
* **Contiguity:** Perfectly contiguous.
* **Overflow:** None. Objects can span across physical page boundaries because the virtual addressing is linear.

---

## 4. Configuration Specification

The store can be tuned using the following parameters:

| Parameter | Recommended (Performance) | Recommended (Memory) | Description |
| --- | --- | --- | --- |
| `PAGE_SIZE` | 2 MB (Huge Pages) | 64 KB | The unit of allocation. |
| `PREFETCH_THRESHOLD` | 0.8 (80%) | 0.95 (95%) | When to trigger the next allocation. |
| `DECAY_TIMEOUT` | 5000 ms | 1000 ms | How long to keep empty pages alive. |
| `BACKEND_MODE` | `Virtual` | `Segmented` | The underlying storage strategy. |
