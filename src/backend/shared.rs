//! Shared-memory backend using `/dev/shm` chunked files.
//!
//! Provides cross-process data sharing via:
//! - A **control file** (`/dev/shm/{ns}_ctrl`) holding global metadata
//! - **Data chunks** (`/dev/shm/{ns}_data_{id}`) each of a fixed size
//!
//! All synchronisation uses atomics embedded in the shared memory itself,
//! so no OS-level IPC is required for the hot path.

use crate::types::{BlobError, OverflowHandle, Result};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

// ── Constants ─────────────────────────────────────────────────────────────

const CTRL_MAGIC: u64 = 0x444D58505F4F5646; // "DMXP_OVF"
const CTRL_VERSION: u32 = 1;
const CHUNK_HEADER_SIZE: usize = 64; // bytes reserved at the start of each chunk

/// Default chunk size: 32 MB.
pub const DEFAULT_CHUNK_SIZE: usize = 32 * 1024 * 1024;

// ── Control File Layout ───────────────────────────────────────────────────
//
// Offset  Field              Size   Description
// ------  -----------------  -----  ----------------------------------
//   0     magic              8      0x444D58505F4F5646
//   8     version            4      Protocol version (1)
//  12     chunk_size         4      Bytes per data chunk
//  16     write_head         4      Current active chunk for appends (AtomicU32)
//  20     chunk_count        4      Highest allocated chunk id + 1 (AtomicU32)
//  24     generation         4      Global generation counter (AtomicU32)
//  28     _reserved         100     Padding to 128 bytes
//
// Total: 128 bytes.

const CTRL_SIZE: usize = 128;

/// Raw view over the control file's mmap region.
struct ControlFile {
    ptr: NonNull<u8>,
    #[cfg(unix)]
    fd: std::os::unix::io::RawFd,
    /// We keep the original (pre-alignment) pointer for munmap.
    _map_ptr: *mut u8,
    _map_len: usize,
}

// SAFETY: The control file is designed for cross-thread + cross-process use
// via atomics embedded in the shared region.
unsafe impl Send for ControlFile {}
unsafe impl Sync for ControlFile {}

impl ControlFile {
    // ── Accessors (pointer arithmetic into the mmap) ──────────────────

    fn magic(&self) -> u64 {
        unsafe { (self.ptr.as_ptr() as *const u64).read_volatile() }
    }

    fn version(&self) -> u32 {
        unsafe { (self.ptr.as_ptr().add(8) as *const u32).read_volatile() }
    }

    fn chunk_size(&self) -> u32 {
        unsafe { (self.ptr.as_ptr().add(12) as *const u32).read_volatile() }
    }

    fn write_head(&self) -> &AtomicU32 {
        unsafe { &*(self.ptr.as_ptr().add(16) as *const AtomicU32) }
    }

    fn chunk_count(&self) -> &AtomicU32 {
        unsafe { &*(self.ptr.as_ptr().add(20) as *const AtomicU32) }
    }

    fn generation(&self) -> &AtomicU32 {
        unsafe { &*(self.ptr.as_ptr().add(24) as *const AtomicU32) }
    }

    // ── Initialise (creator only) ─────────────────────────────────────

    unsafe fn init(&self, chunk_size: u32) {
        let p = self.ptr.as_ptr();
        // Zero everything first
        ptr::write_bytes(p, 0, CTRL_SIZE);
        // Write header fields
        (p as *mut u64).write(CTRL_MAGIC);
        (p.add(8) as *mut u32).write(CTRL_VERSION);
        (p.add(12) as *mut u32).write(chunk_size);
        // write_head, chunk_count, generation start at 0 (already zeroed)
    }

    fn validate(&self) -> Result<()> {
        if self.magic() != CTRL_MAGIC {
            return Err(BlobError::InvalidHandle);
        }
        if self.version() != CTRL_VERSION {
            return Err(BlobError::InvalidHandle);
        }
        Ok(())
    }
}

impl Drop for ControlFile {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::munmap(self._map_ptr as *mut libc::c_void, self._map_len);
            libc::close(self.fd);
        }
    }
}

// ── Chunk Header Layout ──────────────────────────────────────────────────
//
// Sits at byte 0 of every data chunk mmap.
//
// Offset  Field           Size  Description
//   0     used            4     Bytes written (AtomicU32, CAS target)
//   4     generation      4     Recycling generation (AtomicU32)
//   8     entry_count     4     Total entries appended (AtomicU32)
//  12     ack_count       4     Acknowledged entries (AtomicU32)
//  16     empty_since     8     Timestamp when all entries became dead (AtomicU64)
//  24     _reserved      40     Pad to 64 bytes
//
// Data region starts at offset CHUNK_HEADER_SIZE (64).

/// One mapped data chunk.
struct SharedChunk {
    ptr: NonNull<u8>,
    total_size: usize, // header + data
    #[cfg(unix)]
    fd: std::os::unix::io::RawFd,
    _map_ptr: *mut u8,
    _map_len: usize,
}

unsafe impl Send for SharedChunk {}
unsafe impl Sync for SharedChunk {}

impl SharedChunk {
    // ── Header field accessors ────────────────────────────────────────

    fn used(&self) -> &AtomicU32 {
        unsafe { &*(self.ptr.as_ptr() as *const AtomicU32) }
    }

    fn generation(&self) -> &AtomicU32 {
        unsafe { &*(self.ptr.as_ptr().add(4) as *const AtomicU32) }
    }

    fn entry_count(&self) -> &AtomicU32 {
        unsafe { &*(self.ptr.as_ptr().add(8) as *const AtomicU32) }
    }

    fn ack_count(&self) -> &AtomicU32 {
        unsafe { &*(self.ptr.as_ptr().add(12) as *const AtomicU32) }
    }

    fn empty_since(&self) -> &AtomicU64 {
        unsafe { &*(self.ptr.as_ptr().add(16) as *const AtomicU64) }
    }

    /// Pointer to the start of the data region (after the 64-byte header).
    fn data_ptr(&self) -> *mut u8 {
        unsafe { self.ptr.as_ptr().add(CHUNK_HEADER_SIZE) }
    }

    /// Usable data capacity (total - header).
    fn data_capacity(&self) -> usize {
        self.total_size - CHUNK_HEADER_SIZE
    }

    // ── Init (creator only) ──────────────────────────────────────────

    unsafe fn init(&self, generation: u32) {
        ptr::write_bytes(self.ptr.as_ptr(), 0, CHUNK_HEADER_SIZE);
        self.generation().store(generation, Ordering::Release);
    }
}

impl Drop for SharedChunk {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::munmap(self._map_ptr as *mut libc::c_void, self._map_len);
            libc::close(self.fd);
        }
    }
}

// ── SharedBackend ─────────────────────────────────────────────────────────

/// Cross-process overflow arena backed by `/dev/shm` chunked files.
pub struct SharedBackend {
    ctrl: ControlFile,
    chunks: parking_lot::RwLock<BTreeMap<u32, SharedChunk>>,
    namespace: String,
    chunk_size: usize,
    /// True if this process created the shared region (and is responsible for cleanup).
    is_creator: bool,
}

impl SharedBackend {
    // ── Construction ──────────────────────────────────────────────────

    /// Create a new shared arena (called by the first/creator process).
    ///
    /// Allocates the control file and the first data chunk.
    #[cfg(unix)]
    pub fn create(namespace: &str, chunk_size: usize) -> Result<Self> {
        let ctrl = Self::open_ctrl(namespace, true)?;
        unsafe { ctrl.init(chunk_size as u32) };

        let backend = Self {
            ctrl,
            chunks: parking_lot::RwLock::new(BTreeMap::new()),
            namespace: namespace.to_string(),
            chunk_size,
            is_creator: true,
        };

        // Allocate chunk 0
        backend.allocate_chunk(0)?;
        Ok(backend)
    }

    /// Attach to an existing shared arena (called by subsequent processes).
    #[cfg(unix)]
    pub fn attach(namespace: &str) -> Result<Self> {
        let ctrl = Self::open_ctrl(namespace, false)?;
        ctrl.validate()?;

        let chunk_size = ctrl.chunk_size() as usize;

        let backend = Self {
            ctrl,
            chunks: parking_lot::RwLock::new(BTreeMap::new()),
            namespace: namespace.to_string(),
            chunk_size,
            is_creator: false,
        };

        // Eagerly map all existing chunks
        let count = backend.ctrl.chunk_count().load(Ordering::Acquire);
        for id in 0..count {
            let _ = backend.get_or_map_chunk(id);
        }

        Ok(backend)
    }

    // ── Public API ────────────────────────────────────────────────────

    /// Append data to the shared arena.
    ///
    /// Uses a CAS loop on the active chunk's `used` counter. If the current
    /// chunk is full, a new chunk is allocated and the `write_head` is advanced.
    ///
    /// Returns an `OverflowHandle` that any process can `resolve()`.
    pub fn append(&self, data: &[u8]) -> Result<OverflowHandle> {
        if data.is_empty() {
            return Err(BlobError::DataTooLarge { size: 0, max: self.data_capacity() });
        }
        if data.len() > self.data_capacity() {
            return Err(BlobError::DataTooLarge {
                size: data.len(),
                max: self.data_capacity(),
            });
        }

        loop {
            let page_id = self.ctrl.write_head().load(Ordering::Acquire);
            let chunk = self.get_or_map_chunk(page_id)?;

            let current_used = chunk.used().load(Ordering::Acquire);
            let new_used = current_used as usize + data.len();

            if new_used > chunk.data_capacity() {
                // Chunk full — try to reuse a recycled chunk before allocating new
                let next_id = self.find_recycled_chunk(page_id)
                    .unwrap_or_else(|| {
                        let new_id = self.ctrl.chunk_count().load(Ordering::Acquire);
                        let _ = self.allocate_chunk(new_id);
                        new_id
                    });
                let _ = self.ctrl.write_head().compare_exchange(
                    page_id,
                    next_id,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                continue;
            }

            match chunk.used().compare_exchange_weak(
                current_used,
                new_used as u32,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(offset) => {
                    // We own [offset, offset + data.len()) in the data region.
                    unsafe {
                        ptr::copy_nonoverlapping(
                            data.as_ptr(),
                            chunk.data_ptr().add(offset as usize),
                            data.len(),
                        );
                    }

                    chunk.entry_count().fetch_add(1, Ordering::Release);
                    let gen = chunk.generation().load(Ordering::Acquire);

                    return Ok(OverflowHandle::new(page_id, offset, data.len() as u32, gen));
                }
                Err(_) => {
                    std::hint::spin_loop();
                    continue;
                }
            }
        }
    }

    /// Resolve an `OverflowHandle` to a zero-copy byte slice.
    ///
    /// The returned slice points directly into the mmapped shared region.
    ///
    /// Returns `None` if:
    /// - The chunk has been recycled (generation mismatch)
    /// - The handle references out-of-bounds data
    /// - The TTL has expired
    pub fn resolve<'a>(&'a self, handle: &OverflowHandle, ttl_ms: u64) -> Option<&'a [u8]> {
        if handle.is_expired(ttl_ms) {
            return None;
        }

        let chunk = self.get_or_map_chunk(handle.page_id).ok()?;
        let gen = chunk.generation().load(Ordering::Acquire);
        if gen != handle.generation {
            return None;
        }

        let start = handle.offset as usize;
        let end = start + handle.size as usize;
        if end > chunk.data_capacity() {
            return None;
        }

        Some(unsafe {
            std::slice::from_raw_parts(chunk.data_ptr().add(start), handle.size as usize)
        })
    }

    /// Acknowledge that an entry has been consumed.
    pub fn acknowledge(&self, handle: &OverflowHandle) -> bool {
        if let Ok(chunk) = self.get_or_map_chunk(handle.page_id) {
            let gen = chunk.generation().load(Ordering::Acquire);
            if gen == handle.generation {
                let prev_ack = chunk.ack_count().fetch_add(1, Ordering::AcqRel);
                let entries = chunk.entry_count().load(Ordering::Acquire);
                // If this ack completes all entries, record the timestamp
                if prev_ack + 1 >= entries && entries > 0 {
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64;
                    // CAS to avoid overwriting if already set
                    let _ = chunk.empty_since().compare_exchange(
                        0,
                        now_ms,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    );
                }
                return true;
            }
        }
        false
    }

    /// Sweep all chunks and recycle any that are fully acknowledged/expired.
    ///
    /// Returns the number of chunks recycled.
    pub fn cleanup_chunks(&self, _ttl_ms: u64, decay_timeout_ms: u64) -> usize {
        let write_head = self.ctrl.write_head().load(Ordering::Acquire);
        let _chunk_count = self.ctrl.chunk_count().load(Ordering::Acquire);
        let mut freed = 0;

        let chunks = self.chunks.read();
        for (&id, chunk) in chunks.iter() {
            // Never touch the active write chunk
            if id == write_head {
                continue;
            }

            let entries = chunk.entry_count().load(Ordering::Acquire);
            let acked = chunk.ack_count().load(Ordering::Acquire);

            if entries == 0 {
                continue;
            }

            // Check if all entries are acknowledged
            let all_done = acked >= entries;
            if !all_done {
                continue;
            }

            // Check decay timeout
            let mut empty_ts = chunk.empty_since().load(Ordering::Acquire);
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;

            if empty_ts == 0 {
                // First time we see it fully acked — record timestamp
                chunk.empty_since().store(now_ms, Ordering::Release);
                empty_ts = now_ms;
            }

            if now_ms.saturating_sub(empty_ts) < decay_timeout_ms {
                continue;
            }

            // Recycle: reset the chunk header with a new generation
            let new_gen = self.ctrl.generation().fetch_add(1, Ordering::AcqRel) + 1;
            unsafe { chunk.init(new_gen) };
            freed += 1;
        }

        freed
    }

    // ── Introspection ─────────────────────────────────────────────────

    /// Number of currently mapped chunks.
    pub fn chunk_count(&self) -> usize {
        self.chunks.read().len()
    }

    /// Usable bytes per chunk (total - header).
    pub fn data_capacity(&self) -> usize {
        self.chunk_size - CHUNK_HEADER_SIZE
    }

    /// Print debug info about all mapped chunks.
    pub fn debug_chunks(&self) {
        let write_head = self.ctrl.write_head().load(Ordering::Acquire);
        let chunks = self.chunks.read();
        eprintln!("  [DEBUG] write_head={}, mapped_chunks={}", write_head, chunks.len());
        for (&id, chunk) in chunks.iter() {
            let entries = chunk.entry_count().load(Ordering::Acquire);
            let acked = chunk.ack_count().load(Ordering::Acquire);
            let used = chunk.used().load(Ordering::Acquire);
            let gen = chunk.generation().load(Ordering::Acquire);
            let empty_ts = chunk.empty_since().load(Ordering::Acquire);
            eprintln!(
                "    chunk[{}]: gen={} entries={} acked={} used={}/{} empty_since={} {}",
                id, gen, entries, acked, used, self.data_capacity(),
                empty_ts,
                if id == write_head { "<-- ACTIVE" } else { "" }
            );
        }
    }

    // ── Internal helpers ──────────────────────────────────────────────

    /// Get a chunk reference, lazily mapping it if needed.
    fn get_or_map_chunk(&self, id: u32) -> Result<&SharedChunk> {
        // Fast path: already mapped
        {
            let chunks = self.chunks.read();
            if chunks.contains_key(&id) {
                // SAFETY: The chunk stays in the map for its entire lifetime.
                // We hold a read-ref and return a reference whose lifetime is
                // tied to `&self`, which outlives any individual read-guard.
                let chunk = chunks.get(&id).unwrap();
                let chunk_ptr = chunk as *const SharedChunk;
                return Ok(unsafe { &*chunk_ptr });
            }
        }

        // Slow path: map it
        let chunk = Self::open_chunk(&self.namespace, id, self.chunk_size, false)?;
        let mut chunks = self.chunks.write();
        chunks.entry(id).or_insert(chunk);
        let chunk = chunks.get(&id).unwrap();
        let chunk_ptr = chunk as *const SharedChunk;
        Ok(unsafe { &*chunk_ptr })
    }

    /// Find an already-recycled chunk (used == 0, entry_count == 0) to reuse.
    ///
    /// Returns `Some(chunk_id)` if one is found, `None` if all mapped chunks are in use.
    fn find_recycled_chunk(&self, skip_id: u32) -> Option<u32> {
        let chunks = self.chunks.read();
        for (&id, chunk) in chunks.iter() {
            if id == skip_id {
                continue;
            }
            let used = chunk.used().load(Ordering::Acquire);
            let entries = chunk.entry_count().load(Ordering::Acquire);
            if used == 0 && entries == 0 {
                return Some(id);
            }
        }
        None
    }

    /// Allocate a new chunk (create the shm file).
    fn allocate_chunk(&self, id: u32) -> Result<()> {
        let gen = self.ctrl.generation().fetch_add(1, Ordering::AcqRel) + 1;
        let chunk = Self::open_chunk(&self.namespace, id, self.chunk_size, true)?;
        unsafe { chunk.init(gen) };

        {
            let mut chunks = self.chunks.write();
            chunks.insert(id, chunk);
        }

        // Update chunk_count high-water mark
        loop {
            let current = self.ctrl.chunk_count().load(Ordering::Acquire);
            if id + 1 <= current {
                break;
            }
            if self
                .ctrl
                .chunk_count()
                .compare_exchange_weak(current, id + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
        }

        Ok(())
    }

    // ── Platform-specific shm helpers ─────────────────────────────────

    #[cfg(unix)]
    fn open_ctrl(namespace: &str, create: bool) -> Result<ControlFile> {
        Self::shm_open_and_mmap(
            &format!("{}_ctrl", namespace),
            CTRL_SIZE,
            create,
            |ptr, fd, map_ptr, map_len| ControlFile {
                ptr,
                fd,
                _map_ptr: map_ptr,
                _map_len: map_len,
            },
        )
    }

    #[cfg(unix)]
    fn open_chunk(namespace: &str, id: u32, size: usize, create: bool) -> Result<SharedChunk> {
        Self::shm_open_and_mmap(
            &format!("{}_data_{}", namespace, id),
            size,
            create,
            |ptr, fd, map_ptr, map_len| SharedChunk {
                ptr,
                total_size: size,
                fd,
                _map_ptr: map_ptr,
                _map_len: map_len,
            },
        )
    }

    /// Low-level: open (or create) a POSIX shared memory object and mmap it.
    #[cfg(unix)]
    fn shm_open_and_mmap<T>(
        name: &str,
        size: usize,
        create: bool,
        build: impl FnOnce(NonNull<u8>, std::os::unix::io::RawFd, *mut u8, usize) -> T,
    ) -> Result<T> {
        use std::os::unix::io::RawFd;

        let shm_name = format!("/{}", name);
        let c_name =
            CString::new(shm_name.as_str()).map_err(|_| BlobError::OutOfMemory)?;

        let fd: RawFd = if create {
            let fd = unsafe {
                libc::shm_open(
                    c_name.as_ptr(),
                    libc::O_CREAT | libc::O_RDWR,
                    0o600,
                )
            };
            if fd < 0 {
                return Err(BlobError::OutOfMemory);
            }
            if unsafe { libc::ftruncate(fd, size as i64) } != 0 {
                unsafe { libc::close(fd) };
                return Err(BlobError::OutOfMemory);
            }
            fd
        } else {
            let fd = unsafe { libc::shm_open(c_name.as_ptr(), libc::O_RDWR, 0o600) };
            if fd < 0 {
                return Err(BlobError::OutOfMemory);
            }
            fd
        };

        let map_len = size;
        let map_ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };

        if map_ptr == libc::MAP_FAILED {
            unsafe { libc::close(fd) };
            return Err(BlobError::OutOfMemory);
        }

        let ptr = NonNull::new(map_ptr as *mut u8).ok_or(BlobError::OutOfMemory)?;
        Ok(build(ptr, fd, map_ptr as *mut u8, map_len))
    }
}

impl std::fmt::Debug for SharedBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedBackend")
            .field("namespace", &self.namespace)
            .field("chunk_size", &self.chunk_size)
            .field("mapped_chunks", &self.chunks.read().len())
            .finish()
    }
}

impl Drop for SharedBackend {
    fn drop(&mut self) {
        // Unmap all chunks (SharedChunk Drop handles munmap + close)
        self.chunks.write().clear();

        // Only the creator process unlinks the shm files
        if !self.is_creator {
            return;
        }

        #[cfg(unix)]
        {
            let chunk_count = self.ctrl.chunk_count().load(Ordering::Acquire);

            // Unlink data chunks
            for i in 0..chunk_count {
                let name = CString::new(format!("/{}_data_{}", self.namespace, i)).unwrap();
                unsafe { libc::shm_unlink(name.as_ptr()); }
            }

            // Unlink control file (done last so attachers can still read it)
            let ctrl_name = CString::new(format!("/{}_ctrl", self.namespace)).unwrap();
            unsafe { libc::shm_unlink(ctrl_name.as_ptr()); }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_namespace() -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        // Each test call gets a unique, short namespace ≤ 16 chars.
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("st{}", id)
    }

    fn cleanup_shm(namespace: &str, chunk_count: u32) {
        #[cfg(unix)]
        unsafe {
            let ctrl_name = CString::new(format!("/{}_ctrl", namespace)).unwrap();
            libc::shm_unlink(ctrl_name.as_ptr());
            for i in 0..chunk_count {
                let data_name = CString::new(format!("/{}_data_{}", namespace, i)).unwrap();
                libc::shm_unlink(data_name.as_ptr());
            }
        }
    }

    #[test]
    fn test_create_and_append() {
        let ns = test_namespace();
        let chunk_size = 4096; // small for testing
        let backend = SharedBackend::create(&ns, chunk_size).unwrap();

        let data = b"Hello, shared world!";
        let handle = backend.append(data).unwrap();

        assert_eq!(handle.page_id, 0);
        assert_eq!(handle.size, data.len() as u32);

        let resolved = backend.resolve(&handle, 30_000).unwrap();
        assert_eq!(resolved, data);

        cleanup_shm(&ns, 1);
    }

    #[test]
    fn test_attach_and_resolve() {
        let ns = test_namespace();
        let chunk_size = 4096;

        // Process A: create and write
        let backend_a = SharedBackend::create(&ns, chunk_size).unwrap();
        let data = b"cross-process payload";
        let handle = backend_a.append(data).unwrap();

        // Process B: attach and read
        let backend_b = SharedBackend::attach(&ns).unwrap();
        let resolved = backend_b.resolve(&handle, 30_000).unwrap();
        assert_eq!(resolved, data);

        cleanup_shm(&ns, 1);
    }

    #[test]
    fn test_chunk_overflow_to_next() {
        let ns = test_namespace();
        let chunk_size = CHUNK_HEADER_SIZE + 128; // Only 128 bytes of data space
        let backend = SharedBackend::create(&ns, chunk_size).unwrap();

        // Fill chunk 0
        let data = vec![0xABu8; 100];
        let h1 = backend.append(&data).unwrap();
        assert_eq!(h1.page_id, 0);

        // This should overflow to chunk 1
        let h2 = backend.append(&data).unwrap();
        assert_eq!(h2.page_id, 1);

        // Both should resolve
        let r1 = backend.resolve(&h1, 30_000).unwrap();
        assert_eq!(r1, data.as_slice());
        let r2 = backend.resolve(&h2, 30_000).unwrap();
        assert_eq!(r2, data.as_slice());

        cleanup_shm(&ns, 2);
    }

    #[test]
    fn test_acknowledge() {
        let ns = test_namespace();
        let chunk_size = 4096;
        let backend = SharedBackend::create(&ns, chunk_size).unwrap();

        let handle = backend.append(b"ack test").unwrap();
        assert!(backend.acknowledge(&handle));

        cleanup_shm(&ns, 1);
    }
}
