//! Shared-memory backend using `/dev/shm` chunked files.
//!
//! Provides cross-process data sharing via:
//! - A **control file** (`/dev/shm/{ns}_ctrl`) holding global metadata
//! - **Data chunks** (`/dev/shm/{ns}_data_{id}`) each of a fixed size
//!
//! All synchronisation uses atomics embedded in the shared memory itself,
//! so no OS-level IPC is required for the hot path.

use crate::types::{now_ms, BlobError, OverflowHandle, Result};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

// ── Constants ─────────────────────────────────────────────────────────────

const CTRL_MAGIC: u64 = 0x444D58505F4F5646; // "DMXP_OVF"
const CTRL_VERSION: u32 = 1;
const CHUNK_HEADER_SIZE: usize = 64; // bytes reserved at the start of each chunk

/// Minimum chunk size: must exceed the 64-byte header to have usable data space.
const MIN_CHUNK_SIZE: usize = CHUNK_HEADER_SIZE + 1;

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

    /// Initialise the chunk header for first use.
    ///
    /// # Safety
    /// Caller must ensure no other thread is concurrently reading this
    /// chunk's header (i.e. this is only called during allocation, before
    /// the chunk is visible to readers).
    unsafe fn init(&self, generation: u32) {
        ptr::write_bytes(self.ptr.as_ptr(), 0, CHUNK_HEADER_SIZE);
        self.generation().store(generation, Ordering::Release);
    }

    /// Reset the chunk header atomically for recycling.
    ///
    /// Unlike `init()`, this uses individual atomic stores so it is safe
    /// to call while other threads may be reading the header fields via
    /// their own atomic loads.
    fn reset_atomic(&self, new_generation: u32) {
        self.used().store(0, Ordering::Release);
        self.entry_count().store(0, Ordering::Release);
        self.ack_count().store(0, Ordering::Release);
        self.empty_since().store(0, Ordering::Release);
        // Generation must be last — readers check generation to detect recycling.
        self.generation().store(new_generation, Ordering::Release);
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
    chunks: parking_lot::RwLock<BTreeMap<u32, Arc<SharedChunk>>>,
    namespace: String,
    chunk_size: usize,
    /// Maximum number of chunks allowed (`None` = unlimited).
    max_chunks: Option<u32>,
    /// True if this process created the shared region (and is responsible for cleanup).
    is_creator: bool,
}

impl SharedBackend {
    // ── Construction ──────────────────────────────────────────────────

    /// Validate that a namespace is safe for use in `/dev/shm` file names.
    ///
    /// Rejects empty strings, strings containing '/', null bytes, or
    /// strings longer than 200 characters (POSIX shm names are limited to ~255 chars
    /// and we append suffixes like `_data_4294967295`).
    fn validate_namespace(namespace: &str) -> Result<()> {
        if namespace.is_empty() {
            return Err(BlobError::InvalidHandle);
        }
        if namespace.len() > 200 {
            return Err(BlobError::InvalidHandle);
        }
        if namespace.contains('/') || namespace.contains('\0') {
            return Err(BlobError::InvalidHandle);
        }
        Ok(())
    }

    /// Validate that chunk_size is within acceptable bounds.
    ///
    /// Must be > CHUNK_HEADER_SIZE (64) so there is usable data space,
    /// and must fit in a u32 since the control file stores it as u32.
    fn validate_chunk_size(chunk_size: usize) -> Result<()> {
        if chunk_size < MIN_CHUNK_SIZE {
            return Err(BlobError::DataTooLarge {
                size: chunk_size,
                max: MIN_CHUNK_SIZE,
            });
        }
        if chunk_size > u32::MAX as usize {
            return Err(BlobError::DataTooLarge {
                size: chunk_size,
                max: u32::MAX as usize,
            });
        }
        Ok(())
    }

    /// Create a new shared arena (called by the first/creator process).
    ///
    /// Allocates the control file and the first data chunk.
    ///
    /// `max_chunks`: Optional upper bound on the number of chunks.
    /// When the limit is reached and no recycled chunks are available,
    /// `append()` returns `Err(OutOfMemory)`.
    /// Pass `None` for unlimited.
    #[cfg(unix)]
    pub fn create(namespace: &str, chunk_size: usize, max_chunks: Option<u32>) -> Result<Self> {
        Self::validate_namespace(namespace)?;
        Self::validate_chunk_size(chunk_size)?;
        let ctrl = Self::open_ctrl(namespace, true)?;
        unsafe { ctrl.init(chunk_size as u32) };

        let backend = Self {
            ctrl,
            chunks: parking_lot::RwLock::new(BTreeMap::new()),
            namespace: namespace.to_string(),
            chunk_size,
            max_chunks,
            is_creator: true,
        };

        // Allocate chunk 0 and set chunk_count = 1
        backend.allocate_chunk(0)?;
        backend.ctrl.chunk_count().store(1, Ordering::Release);
        Ok(backend)
    }

    /// Attach to an existing shared arena (called by subsequent processes).
    ///
    /// `max_chunks`: Optional upper bound — should match the creator's setting.
    #[cfg(unix)]
    pub fn attach(namespace: &str, max_chunks: Option<u32>) -> Result<Self> {
        Self::validate_namespace(namespace)?;
        let ctrl = Self::open_ctrl(namespace, false)?;
        ctrl.validate()?;

        let chunk_size = ctrl.chunk_size() as usize;

        let backend = Self {
            ctrl,
            chunks: parking_lot::RwLock::new(BTreeMap::new()),
            namespace: namespace.to_string(),
            chunk_size,
            max_chunks,
            is_creator: false,
        };

        // Eagerly map all existing chunks
        let count = backend.ctrl.chunk_count().load(Ordering::Acquire);
        for id in 0..count {
            let _ = backend.get_or_map_chunk(id);
        }

        Ok(backend)
    }

    /// Unlink all `/dev/shm` files for a given namespace.
    ///
    /// Call this at application startup to clean up after a previous crash
    /// where the creator process was killed before `Drop` could run.
    /// Safe to call even if no files exist (stale unlinks are no-ops).
    #[cfg(unix)]
    pub fn cleanup_namespace(namespace: &str) -> Result<()> {
        Self::validate_namespace(namespace)?;

        // Try to open and read the control file to discover chunk count
        let ctrl_shm = format!("/{}_ctrl", namespace);
        if let Ok(c_name) = CString::new(ctrl_shm.as_str()) {
            let fd = unsafe { libc::shm_open(c_name.as_ptr(), libc::O_RDONLY, 0o600) };
            if fd >= 0 {
                // Map it read-only to read chunk_count
                let map_ptr = unsafe {
                    libc::mmap(
                        ptr::null_mut(),
                        CTRL_SIZE,
                        libc::PROT_READ,
                        libc::MAP_SHARED,
                        fd,
                        0,
                    )
                };
                let chunk_count = if map_ptr != libc::MAP_FAILED {
                    let count = unsafe {
                        let p = map_ptr as *const u8;
                        (p.add(20) as *const u32).read_volatile()
                    };
                    unsafe {
                        libc::munmap(map_ptr, CTRL_SIZE);
                    }
                    count
                } else {
                    // Can't read — try a reasonable upper bound
                    1024
                };
                unsafe {
                    libc::close(fd);
                }

                // Unlink data chunks
                for i in 0..chunk_count {
                    if let Ok(name) = CString::new(format!("/{}_data_{}", namespace, i)) {
                        unsafe {
                            libc::shm_unlink(name.as_ptr());
                        }
                    }
                }
            }

            // Unlink control file last
            unsafe {
                libc::shm_unlink(c_name.as_ptr());
            }
        }

        Ok(())
    }

    // ── Public API ────────────────────────────────────────────────────

    /// Append data to the shared arena.
    ///
    /// Uses a CAS loop on the active chunk's `used` counter. If the current
    /// chunk is full, a new chunk is allocated and the `write_head` is advanced.
    ///
    /// Returns an `OverflowHandle` that any process can `resolve()`.
    ///
    /// Returns `Err(OutOfMemory)` if `max_chunks` is reached and no recycled
    /// chunks are available.
    pub fn append(&self, data: &[u8]) -> Result<OverflowHandle> {
        if data.is_empty() {
            return Err(BlobError::DataTooLarge {
                size: 0,
                max: self.data_capacity(),
            });
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
                // Chunk full — try to advance to a new chunk
                let next_id = self.allocate_next_chunk(page_id)?;
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

    /// Resolve an `OverflowHandle` to an owned copy of the data.
    ///
    /// Returns `None` if:
    /// - The chunk has been recycled (generation mismatch)
    /// - The handle references out-of-bounds data
    /// - The TTL has expired
    ///
    /// The data is copied out of the mmap region so it remains valid
    /// even if the chunk is recycled after this call returns.
    pub fn resolve(&self, handle: &OverflowHandle, ttl_ms: u64) -> Option<Vec<u8>> {
        if handle.is_expired(ttl_ms) {
            return None;
        }

        let chunk = self.get_or_map_chunk(handle.page_id).ok()?;
        let gen = chunk.generation().load(Ordering::Acquire);
        if gen != handle.generation {
            return None;
        }

        let start = handle.offset as usize;
        let end = start.checked_add(handle.size as usize)?;
        if end > chunk.data_capacity() {
            return None;
        }

        // Copy data out so the caller is safe even if the chunk is recycled.
        let mut buf = vec![0u8; handle.size as usize];
        unsafe {
            ptr::copy_nonoverlapping(chunk.data_ptr().add(start), buf.as_mut_ptr(), buf.len());
        }

        // Re-check generation after copy to detect concurrent recycling.
        let gen_after = chunk.generation().load(Ordering::Acquire);
        if gen_after != handle.generation {
            return None;
        }

        Some(buf)
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
                    let ts = now_ms();
                    // CAS to avoid overwriting if already set
                    let _ = chunk.empty_since().compare_exchange(
                        0,
                        ts,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                }
                return true;
            }
        }
        false
    }

    /// Sweep all chunks and recycle any that are fully acknowledged/expired.
    ///
    /// Uses atomic field resets (not `ptr::write_bytes`) so it is safe to
    /// call concurrently with `resolve()` and `append()`.
    ///
    /// Returns the number of chunks recycled.
    pub fn cleanup_chunks(&self, _ttl_ms: u64, decay_timeout_ms: u64) -> usize {
        let write_head = self.ctrl.write_head().load(Ordering::Acquire);
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
            if acked < entries {
                continue;
            }

            // Check decay timeout
            let mut empty_ts = chunk.empty_since().load(Ordering::Acquire);
            let ts = now_ms();

            if empty_ts == 0 {
                // First time we see it fully acked — record timestamp
                chunk.empty_since().store(ts, Ordering::Release);
                empty_ts = ts;
            }

            if ts.saturating_sub(empty_ts) < decay_timeout_ms {
                continue;
            }

            // Recycle: reset the chunk header atomically with a new generation
            let new_gen = self.ctrl.generation().fetch_add(1, Ordering::AcqRel) + 1;
            chunk.reset_atomic(new_gen);
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
        eprintln!(
            "  [DEBUG] write_head={}, mapped_chunks={}",
            write_head,
            chunks.len()
        );
        for (&id, chunk) in chunks.iter() {
            let entries = chunk.entry_count().load(Ordering::Acquire);
            let acked = chunk.ack_count().load(Ordering::Acquire);
            let used = chunk.used().load(Ordering::Acquire);
            let gen = chunk.generation().load(Ordering::Acquire);
            let empty_ts = chunk.empty_since().load(Ordering::Acquire);
            eprintln!(
                "    chunk[{}]: gen={} entries={} acked={} used={}/{} empty_since={} {}",
                id,
                gen,
                entries,
                acked,
                used,
                self.data_capacity(),
                empty_ts,
                if id == write_head { "<-- ACTIVE" } else { "" }
            );
        }
    }

    // ── Internal helpers ──────────────────────────────────────────────

    /// Allocate or find a recycled chunk when the current write chunk is full.
    ///
    /// Strategy:
    /// 1. Try to find a recycled chunk (used == 0, entry_count == 0).
    /// 2. If none, atomically reserve a new chunk ID via CAS on `chunk_count`.
    /// 3. If `max_chunks` is set and reached, return `OutOfMemory`.
    fn allocate_next_chunk(&self, skip_id: u32) -> Result<u32> {
        // Try recycled first
        if let Some(recycled_id) = self.find_recycled_chunk(skip_id) {
            return Ok(recycled_id);
        }

        // Atomically reserve a new chunk ID via CAS on chunk_count.
        // This prevents two threads from racing to allocate the same ID.
        loop {
            let current_count = self.ctrl.chunk_count().load(Ordering::Acquire);
            let new_id = current_count;

            // Backpressure: enforce max_chunks limit
            if let Some(max) = self.max_chunks {
                if new_id >= max {
                    return Err(BlobError::OutOfMemory);
                }
            }

            // Wraparound protection
            if new_id == u32::MAX {
                return Err(BlobError::OutOfMemory);
            }

            // Try to atomically claim this ID
            if self
                .ctrl
                .chunk_count()
                .compare_exchange_weak(
                    current_count,
                    current_count + 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                // We own this ID — now allocate the shm file
                self.allocate_chunk(new_id)?;
                return Ok(new_id);
            }

            // Another thread won the race — retry
            std::hint::spin_loop();
        }
    }

    /// Get a chunk reference, lazily mapping it if needed.
    ///
    /// Returns an `Arc<SharedChunk>` so the caller can hold it safely
    /// without keeping the RwLock guard alive.
    fn get_or_map_chunk(&self, id: u32) -> Result<Arc<SharedChunk>> {
        // Fast path: already mapped
        {
            let chunks = self.chunks.read();
            if let Some(chunk) = chunks.get(&id) {
                return Ok(Arc::clone(chunk));
            }
        }

        // Slow path: map it
        let chunk = Arc::new(Self::open_chunk(
            &self.namespace,
            id,
            self.chunk_size,
            false,
        )?);
        let mut chunks = self.chunks.write();
        Ok(Arc::clone(chunks.entry(id).or_insert(chunk)))
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
    ///
    /// The caller must have already reserved `id` via the `chunk_count` CAS
    /// in `allocate_next_chunk()`, so no other thread will use this ID.
    fn allocate_chunk(&self, id: u32) -> Result<()> {
        let gen = self.ctrl.generation().fetch_add(1, Ordering::AcqRel) + 1;
        let chunk = Arc::new(Self::open_chunk(
            &self.namespace,
            id,
            self.chunk_size,
            true,
        )?);
        unsafe { chunk.init(gen) };

        {
            let mut chunks = self.chunks.write();
            chunks.insert(id, chunk);
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
        let c_name = CString::new(shm_name.as_str()).map_err(|_| BlobError::OutOfMemory)?;

        let fd: RawFd = if create {
            let fd =
                unsafe { libc::shm_open(c_name.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o600) };
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
            .field("max_chunks", &self.max_chunks)
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
                if let Ok(name) = CString::new(format!("/{}_data_{}", self.namespace, i)) {
                    unsafe {
                        libc::shm_unlink(name.as_ptr());
                    }
                }
            }

            // Unlink control file (done last so attachers can still read it)
            if let Ok(ctrl_name) = CString::new(format!("/{}_ctrl", self.namespace)) {
                unsafe {
                    libc::shm_unlink(ctrl_name.as_ptr());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_namespace() -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("st{}", id)
    }

    // No more manual cleanup_shm — Drop handles it automatically.

    #[test]
    fn test_create_and_append() {
        let ns = test_namespace();
        let backend = SharedBackend::create(&ns, 4096, None).unwrap();

        let data = b"Hello, shared world!";
        let handle = backend.append(data).unwrap();

        assert_eq!(handle.page_id, 0);
        assert_eq!(handle.size, data.len() as u32);

        let resolved = backend.resolve(&handle, 30_000).unwrap();
        assert_eq!(resolved, data);
    } // Drop auto-unlinks shm files

    #[test]
    fn test_attach_and_resolve() {
        let ns = test_namespace();

        let backend_a = SharedBackend::create(&ns, 4096, None).unwrap();
        let data = b"cross-process payload";
        let handle = backend_a.append(data).unwrap();

        let backend_b = SharedBackend::attach(&ns, None).unwrap();
        let resolved = backend_b.resolve(&handle, 30_000).unwrap();
        assert_eq!(resolved, data);

        drop(backend_b); // attacher drops first (no unlink)
    } // creator drops, unlinks shm

    #[test]
    fn test_chunk_overflow_to_next() {
        let ns = test_namespace();
        let chunk_size = CHUNK_HEADER_SIZE + 128; // Only 128 bytes of data space
        let backend = SharedBackend::create(&ns, chunk_size, None).unwrap();

        let data = vec![0xABu8; 100];
        let h1 = backend.append(&data).unwrap();
        assert_eq!(h1.page_id, 0);

        // This should overflow to chunk 1
        let h2 = backend.append(&data).unwrap();
        assert_eq!(h2.page_id, 1);

        assert_eq!(backend.resolve(&h1, 30_000).unwrap(), data.as_slice());
        assert_eq!(backend.resolve(&h2, 30_000).unwrap(), data.as_slice());
    }

    #[test]
    fn test_acknowledge() {
        let ns = test_namespace();
        let backend = SharedBackend::create(&ns, 4096, None).unwrap();

        let handle = backend.append(b"ack test").unwrap();
        assert!(backend.acknowledge(&handle));
    }

    #[test]
    fn test_max_chunks_backpressure() {
        let ns = test_namespace();
        let chunk_size = CHUNK_HEADER_SIZE + 64; // 64 bytes of data space
        // Allow max 2 chunks (chunk 0 + chunk 1)
        let backend = SharedBackend::create(&ns, chunk_size, Some(2)).unwrap();

        // Fill chunk 0
        let _h1 = backend.append(&[0xAAu8; 60]).unwrap();
        // Overflow to chunk 1
        let _h2 = backend.append(&[0xBBu8; 60]).unwrap();
        // Chunk 1 full — should fail because max_chunks=2
        let result = backend.append(&[0xCCu8; 60]);
        assert!(result.is_err());
    }

    #[test]
    fn test_chunk_size_validation() {
        let ns = test_namespace();
        // Too small
        assert!(SharedBackend::create(&ns, 32, None).is_err());
        // Zero
        assert!(SharedBackend::create(&ns, 0, None).is_err());
        // Exactly header size (no data space)
        assert!(SharedBackend::create(&ns, CHUNK_HEADER_SIZE, None).is_err());
    }

    #[test]
    fn test_cleanup_namespace() {
        let ns = test_namespace();

        // Create and populate, then drop normally so files are cleaned up
        {
            let backend = SharedBackend::create(&ns, 4096, None).unwrap();
            backend.append(b"some data").unwrap();
        }

        // Re-create to simulate orphaned files (Drop ran, files are gone,
        // but we create fresh ones and then clean them via cleanup_namespace)
        {
            let backend = SharedBackend::create(&ns, 4096, None).unwrap();
            backend.append(b"orphaned data").unwrap();
            // Manually unmap chunks without unlinking shm files
            // to simulate a crash where Drop doesn't run.
            backend.chunks.write().clear();
            // Now call cleanup_namespace while the ctrl file still exists
            SharedBackend::cleanup_namespace(&ns).unwrap();
        }
        // Files cleaned by cleanup_namespace above; Drop will try to unlink
        // again but that's a harmless no-op.

        // Creating a new one with the same namespace should work
        let _backend = SharedBackend::create(&ns, 4096, None).unwrap();
    }

    // ── Concurrent stress tests ──────────────────────────────────────

    #[test]
    fn test_concurrent_append() {
        let ns = test_namespace();
        let backend = Arc::new(SharedBackend::create(&ns, 8192, None).unwrap());

        let num_threads = 4;
        let writes_per_thread = 50;
        let mut join_handles = Vec::new();

        for t in 0..num_threads {
            let b = Arc::clone(&backend);
            join_handles.push(std::thread::spawn(move || {
                let mut handles = Vec::new();
                for i in 0..writes_per_thread {
                    let payload = format!("t{}-msg{}", t, i);
                    let h = b
                        .append(payload.as_bytes())
                        .expect("concurrent append failed");
                    handles.push((h, payload));
                }
                handles
            }));
        }

        let all_results: Vec<_> = join_handles
            .into_iter()
            .flat_map(|jh| jh.join().unwrap())
            .collect();

        assert_eq!(all_results.len(), num_threads * writes_per_thread);

        // Verify all entries resolve correctly
        for (handle, expected) in &all_results {
            let data = backend
                .resolve(handle, 30_000)
                .expect("concurrent resolve failed");
            assert_eq!(data, expected.as_bytes(), "data corruption detected");
        }
    }

    #[test]
    fn test_concurrent_append_and_resolve() {
        let ns = test_namespace();
        let backend = Arc::new(SharedBackend::create(&ns, 8192, None).unwrap());

        // Phase 1: pre-populate some data
        let mut seed_handles = Vec::new();
        for i in 0..20 {
            let payload = format!("seed-{}", i);
            let h = backend.append(payload.as_bytes()).unwrap();
            seed_handles.push((h, payload));
        }

        let seed_handles = Arc::new(seed_handles);
        let mut join_handles = Vec::new();

        // Spawn writers
        for t in 0..2 {
            let b = Arc::clone(&backend);
            join_handles.push(std::thread::spawn(move || {
                for i in 0..50 {
                    let payload = format!("writer{}-{}", t, i);
                    b.append(payload.as_bytes()).expect("writer append failed");
                }
            }));
        }

        // Spawn readers (resolving seed handles)
        for _ in 0..2 {
            let b = Arc::clone(&backend);
            let seeds = Arc::clone(&seed_handles);
            join_handles.push(std::thread::spawn(move || {
                for (handle, expected) in seeds.iter() {
                    let data = b
                        .resolve(handle, 30_000)
                        .expect("reader resolve failed during concurrent writes");
                    assert_eq!(data, expected.as_bytes());
                }
            }));
        }

        for jh in join_handles {
            jh.join().unwrap();
        }
    }

    #[test]
    fn test_concurrent_lifecycle() {
        let ns = test_namespace();
        let backend = Arc::new(SharedBackend::create(&ns, 4096, None).unwrap());

        let num_threads = 4;
        let writes_per_thread = 30;

        // Phase 1: concurrent writes
        let mut join_handles = Vec::new();
        for t in 0..num_threads {
            let b = Arc::clone(&backend);
            join_handles.push(std::thread::spawn(move || {
                let mut handles = Vec::new();
                for i in 0..writes_per_thread {
                    let payload = format!("lc{}-{}", t, i);
                    let h = b.append(payload.as_bytes()).unwrap();
                    handles.push(h);
                }
                handles
            }));
        }

        let all_handles: Vec<_> = join_handles
            .into_iter()
            .flat_map(|jh| jh.join().unwrap())
            .collect();

        // Phase 2: concurrent acknowledge
        let all_handles = Arc::new(all_handles);
        let mut ack_threads = Vec::new();
        let chunk_size = all_handles.len() / num_threads;
        for t in 0..num_threads {
            let b = Arc::clone(&backend);
            let handles = Arc::clone(&all_handles);
            ack_threads.push(std::thread::spawn(move || {
                let start = t * chunk_size;
                let end = if t == num_threads - 1 {
                    handles.len()
                } else {
                    start + chunk_size
                };
                for h in &handles[start..end] {
                    b.acknowledge(h);
                }
            }));
        }

        for jh in ack_threads {
            jh.join().unwrap();
        }

        // Phase 3: cleanup (with 0ms decay for immediate recycling)
        let recycled = backend.cleanup_chunks(30_000, 0);

        // Should have recycled some non-active chunks
        assert!(
            recycled > 0 || all_handles.iter().all(|h| h.page_id == 0),
            "Expected chunks to be recycled (recycled={}, but not all on chunk 0)",
            recycled
        );
    }
}
