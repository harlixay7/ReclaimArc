//! ReclaimArc core engine.
//!
//! The transactional storage engine: space planning, the recovery-unit state
//! machine, safety (reserve + pre-test + monitoring), path security, the
//! extraction engine and crash recovery.

#![deny(unsafe_op_in_unsafe_fn)]
#![allow(clippy::result_large_err)]

pub mod config;
pub mod engine;
pub mod error;
pub mod events;
pub mod fault;
pub mod paths;
pub mod planner;
pub mod recovery;
pub mod safety;
pub mod state;

pub use config::{ConflictPolicy, EngineConfig, SafetyMode};
pub use engine::{Engine, ExtractionJob, ExtractionMode, JobHandle, JobOutcome};
pub type JobJob = ExtractionJob;
pub use error::{CoreError, FailureInfo};
pub use events::Event;
pub use planner::{plan, SpacePlan};
pub use recovery::{
    abandon_job, discover_interrupted_jobs, prepare_resume, summarize, RecoverySummary,
};
