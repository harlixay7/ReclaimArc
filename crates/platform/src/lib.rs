#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

//! ReclaimArc platform layer.
//!
//! Windows-first implementation of the filesystem operations required by the
//! transactional storage engine: file identity, allocated sizes, free space,
//! sparse-file capability probing, range reclamation (`FSCTL_SET_ZERO_DATA`),
//! allocated-range queries and durable flushes.
//!
//! Every operation reports errors precisely (with OS codes) — the engine never
//! guesses. Nothing here silently ignores a failed flush or reclaim.

pub mod capabilities;
/// Error types for filesystem operations.
pub mod error;
pub mod flush;
pub mod fs;
pub mod longpath;
pub mod shell;
pub mod sparse;

pub use error::{PlatformError, PlatformErrorKind};
