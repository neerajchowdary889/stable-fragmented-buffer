//! # Core Types
//!
//! Defines the fundamental data types shared across all modules:
//!
//! - [`BlobHandle`] — 32-byte reference to data in the heap-based backend.
//!   Encodes page ID, offset, size, generation, and multi-page span info.
//! - [`OverflowHandle`] — 24-byte `#[repr(C)]` reference to data in the
//!   shared-memory backend. ABI-stable for cross-process use; can be
//!   serialized to bytes and embedded in ring-buffer slot payloads.
//! - [`Config`] — Tunable parameters (page size, TTL, decay timeout, etc.).
//! - [`BlobError`] / [`Result`] — Error types for all operations.
//! - [`BackendMode`] — Enum selecting heap vs shared storage.
//! - [`now_ms()`] — Safe monotonic-ish timestamp helper used throughout.

mod types;
mod overflow_handle;

pub use types::*;
pub use overflow_handle::*;
