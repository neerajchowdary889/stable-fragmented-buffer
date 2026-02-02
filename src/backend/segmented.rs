use crate::backend::StorageBackend;
use crate::page::Page;
use crate::types::Result;
use std::collections::HashMap;

/// Segmented storage backend using heap-allocated pages
/// Memory efficient, suitable for memory-constrained environments
pub(crate) struct SegmentedBackend {
    /// Map of page ID to page
    pages: HashMap<u32, Page>,
}

impl SegmentedBackend {
    /// Create a new segmented backend
    pub fn new() -> Self {
        Self {
            pages: HashMap::new(),
        }
    }
}

impl StorageBackend for SegmentedBackend {
    fn allocate_page(&mut self, id: u32, size: usize, generation: u32) -> Result<()> {
        // Check if page already exists
        if self.pages.contains_key(&id) {
            return Ok(());
        }

        // Allocate new page
        let page = Page::new(id, size, generation);
        self.pages.insert(id, page);

        Ok(())
    }

    fn get_page(&self, id: u32) -> Option<&Page> {
        self.pages.get(&id)
    }

    fn get_page_mut(&mut self, id: u32) -> Option<&mut Page> {
        self.pages.get_mut(&id)
    }

    fn page_count(&self) -> usize {
        self.pages.len()
    }

    fn remove_page(&mut self, id: u32) -> bool {
        self.pages.remove(&id).is_some()
    }

    fn active_page_ids(&self) -> Vec<u32> {
        self.pages.keys().copied().collect()
    }
}

impl std::fmt::Debug for SegmentedBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SegmentedBackend")
            .field("page_count", &self.pages.len())
            .finish()
    }
}
