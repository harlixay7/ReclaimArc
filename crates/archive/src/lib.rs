//! ReclaimArc archive backends.
//!
//! The engine communicates with archives exclusively through the
//! `ArchiveBackend` trait. The RAR backend (v1 target) uses the official
//! UnRAR library for decoding behind an explicit license boundary, plus a
//! header parser for exact packed ranges and solid-chain analysis.

pub mod backend;
pub mod error;
pub mod model;
pub mod rar;
pub mod zip;

pub use backend::{ArchiveBackend, ExtractOptions, OpenOptions, ProgressFn};
pub use error::ArchiveError;
pub use model::*;
pub use rar::RarBackend;
pub use zip::ZipBackend;

/// Detect the archive format from its signature and return the matching
/// backend. RAR is the v1 first-class target; future formats (ZIP, 7z, tar)
/// plug in here.
pub fn backend_for(path: &std::path::Path) -> Result<Box<dyn ArchiveBackend>, ArchiveError> {
    let mut file = std::fs::File::open(path)
        .map_err(|e| ArchiveError::open(format!("cannot open '{}': {e}", path.display())))?;
    let mut sig = [0u8; 8];
    use std::io::Read;
    let n = file.read(&mut sig).unwrap_or(0);
    if n >= 7 && sig[..7] == crate::rar::parser::RAR4_SIGNATURE {
        return Ok(Box::new(RarBackend::new(path)));
    }
    if n >= 8 && sig == crate::rar::parser::RAR5_SIGNATURE {
        return Ok(Box::new(RarBackend::new(path)));
    }
    if n >= 4
        && (sig[..4] == crate::zip::parser::ZIP_LOCAL_HEADER_SIG
            || sig[..4] == crate::zip::parser::ZIP_EOCD_SIG
            || sig[..4] == crate::zip::parser::ZIP_SPANNED_SIG)
    {
        return Ok(Box::new(ZipBackend::new(path)));
    }
    Err(ArchiveError::unsupported(format!(
        "'{}' is not a supported archive format",
        path.display()
    )))
}
