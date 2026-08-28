//! ZIP format module: dual-parser structural validation, streaming decompression,
//! and transactional backend implementation.

pub mod backend;
pub mod decoder;
pub mod fixtures;
pub mod parser;

pub use backend::ZipBackend;
