use crate::types::{BlobError, Result};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Virtual memory backend using mmap for contiguous addressing
/// Provides true pointer stability and zero-copy reads
pub struct VirtualBackend {
    /// Base pointer to mmap'd region
    base_ptr: *mut u8,

    /// Total reserved virtual address space
    reserved_size: usize,

    /// Currently used bytes (atomic for lock-free append)
    used: AtomicUsize,

    /// Generation counter
    generation: u32,
}

impl VirtualBackend {
    /// Create a new virtual memory backend
    ///
    /// # Arguments
    /// * `reserved_size` - Virtual address space to reserve (e.g., 1TB)
    ///
    /// # Safety
    /// Uses mmap to reserve virtual address space. Physical memory is allocated
    /// on-demand via page faults when data is written.
    pub fn new(reserved_size: usize, generation: u32) -> Result<Self> {
        let base_ptr = Self::mmap_anonymous(reserved_size)?;

        Ok(Self {
            base_ptr,
            reserved_size,
            used: AtomicUsize::new(0),
            generation,
        })
    }

    /// Reserve virtual address space using mmap
    #[cfg(unix)]
    fn mmap_anonymous(size: usize) -> Result<*mut u8> {
        use libc::{mmap, MAP_ANONYMOUS, MAP_FAILED, MAP_PRIVATE, PROT_READ, PROT_WRITE};

        let ptr = unsafe {
            mmap(
                std::ptr::null_mut(),
                size,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS,
                -1,
                0,
            )
        };

        if ptr == MAP_FAILED {
            return Err(BlobError::OutOfMemory);
        }

        Ok(ptr as *mut u8)
    }

    #[cfg(windows)]
    fn mmap_anonymous(size: usize) -> Result<*mut u8> {
        use winapi::um::memoryapi::VirtualAlloc;
        use winapi::um::winnt::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE};

        let ptr = unsafe {
            VirtualAlloc(
                std::ptr::null_mut(),
                size,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        };

        if ptr.is_null() {
            return Err(BlobError::OutOfMemory);
        }

        Ok(ptr as *mut u8)
    }

    /// Append data to the virtual memory region
    /// Returns the offset where data was written
    pub fn append(&self, data: &[u8]) -> Result<u64> {
        let data_len = data.len();

        // Atomically reserve space
        let offset = self.used.fetch_add(data_len, Ordering::AcqRel);

        // Check if we exceeded reserved space
        if offset + data_len > self.reserved_size {
            // Rollback
            self.used.fetch_sub(data_len, Ordering::AcqRel);
            return Err(BlobError::OutOfMemory);
        }

        // Copy data to reserved space
        // SAFETY: We've atomically reserved this space
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), self.base_ptr.add(offset), data_len);
        }

        Ok(offset as u64)
    }

    /// Get a reference to data at the given offset
    ///
    /// # Safety
    /// Returns a reference with lifetime tied to self.
    /// The data is guaranteed to be stable (never moves) due to mmap.
    pub fn get(&self, offset: u64, size: u64) -> Option<&[u8]> {
        let offset_usize = offset as usize;
        let size_usize = size as usize;

        // Validate bounds
        if offset_usize + size_usize > self.used.load(Ordering::Acquire) {
            return None;
        }

        // Return slice directly from mmap'd region (zero-copy!)
        unsafe {
            Some(std::slice::from_raw_parts(
                self.base_ptr.add(offset_usize),
                size_usize,
            ))
        }
    }

    /// Get current usage
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }

    /// Get generation
    pub fn generation(&self) -> u32 {
        self.generation
    }
}

impl Drop for VirtualBackend {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::munmap(self.base_ptr as *mut libc::c_void, self.reserved_size);
        }

        #[cfg(windows)]
        unsafe {
            use winapi::um::memoryapi::VirtualFree;
            use winapi::um::winnt::MEM_RELEASE;
            VirtualFree(self.base_ptr as *mut winapi::ctypes::c_void, 0, MEM_RELEASE);
        }
    }
}

// Thread safety
unsafe impl Send for VirtualBackend {}
unsafe impl Sync for VirtualBackend {}

impl std::fmt::Debug for VirtualBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VirtualBackend")
            .field("reserved_size", &self.reserved_size)
            .field("used", &self.used.load(Ordering::Acquire))
            .field(
                "utilization",
                &format!(
                    "{:.2}%",
                    (self.used.load(Ordering::Acquire) as f64 / self.reserved_size as f64) * 100.0
                ),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_virtual_backend_basic() {
        let backend = VirtualBackend::new(1024 * 1024, 0).unwrap(); // 1MB

        let data = b"Hello, World!";
        let offset = backend.append(data).unwrap();

        let retrieved = backend.get(offset, data.len() as u64).unwrap();
        assert_eq!(retrieved, data);
    }

    #[test]
    fn test_virtual_backend_large() {
        let backend = VirtualBackend::new(256 * 1024 * 1024, 0).unwrap(); // 256MB

        // Append 10MB
        let large_data = vec![42u8; 10 * 1024 * 1024];
        let offset = backend.append(&large_data).unwrap();

        let retrieved = backend.get(offset, large_data.len() as u64).unwrap();
        assert_eq!(retrieved.len(), large_data.len());
        assert_eq!(retrieved[0], 42);
    }

    #[test]
    fn test_virtual_backend_multiple_appends() {
        let backend = VirtualBackend::new(1024 * 1024, 0).unwrap();

        let mut offsets = Vec::new();
        for i in 0..100 {
            let data = format!("Message {}", i);
            let offset = backend.append(data.as_bytes()).unwrap();
            offsets.push((offset, data));
        }

        // Verify all data
        for (offset, expected) in offsets {
            let retrieved = backend.get(offset, expected.len() as u64).unwrap();
            assert_eq!(retrieved, expected.as_bytes());
        }
    }
}
