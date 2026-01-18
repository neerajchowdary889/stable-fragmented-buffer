use crate::page::Page;
use crate::types::Result;

/// Trait for storage backend implementations
pub(crate) trait StorageBackend: Send + Sync {
    /// Allocate a new page with the given size
    fn allocate_page(&mut self, id: u32, size: usize, generation: u32) -> Result<()>;

    /// Get an immutable reference to a page
    fn get_page(&self, id: u32) -> Option<&Page>;

    /// Get a mutable reference to a page
    fn get_page_mut(&mut self, id: u32) -> Option<&mut Page>;

    /// Get the total number of pages
    fn page_count(&self) -> usize;

    /// Remove a page (for decay/cleanup)
    fn remove_page(&mut self, id: u32) -> bool;
}

pub mod segmented;
pub mod virtual_mem;
