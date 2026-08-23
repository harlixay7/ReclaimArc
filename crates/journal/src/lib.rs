//! Durable recovery journal for SpaceExtract.
//!
//! A per-job SQLite database (WAL, `synchronous=FULL`) recording every durable
//! transition of the transactional extraction engine, plus a mirrored job
//! registry in application data.
//!
//! Passwords are never persisted by this crate.

pub mod error;
pub mod journal;
pub mod models;
pub mod registry;
pub mod schema;
mod util;

pub use error::{JournalError, JournalError as Error};
pub use journal::JobJournal;
pub use models::*;
pub use registry::{Registry, RegistryEntry};
pub use util::now_iso;
