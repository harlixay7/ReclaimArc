//! RAR backend (4.x and 5.x).
//!
//! Decoding is performed exclusively by the official UnRAR library (vendored
//! via `unrar_sys`, license boundary kept: see the vendor's `license.txt`).
//! This module only adds:
//! - exact packed-data ranges and solid-chain analysis (header parsing),
//! - recovery-unit construction,
//! - the `ArchiveBackend` adapter.

pub mod backend;
pub mod decoder;
pub mod fixtures;
pub mod parser;
pub mod volumes;

pub use backend::RarBackend;
