//! Fault injection: crash points used by the test harness to simulate
//! process death after every durable transition.
//!
//! Activated via `RECLAIMARC_FAULT_AT=<name>`; when the engine reaches the
//! named point it calls `std::process::exit(86)`. The journal has already
//! been durably updated up to that point, so reopening the job must prove the
//! invariants hold.

use std::sync::atomic::{AtomicBool, Ordering};

/// Crash points, in engine order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashPoint {
    /// After partial output for the unit has been written by the decoder.
    AfterPartialWrite,
    /// After output flush (FlushFileBuffers on partials).
    AfterOutputFlush,
    /// After the atomic rename to the final name.
    AfterRename,
    /// After the COMMITTED journal record.
    AfterJournalCommit,
    /// After RECLAIM_INTENT is persisted, before punching holes.
    BeforeHolePunch,
    /// During the hole-punching loop (after the first range).
    DuringHolePunch,
    /// Immediately after physical FSCTL_SET_ZERO_DATA deallocation, before mark_range_outcome journal record.
    AfterPhysicalHolePunch,
    /// After holes are punched, before the RECLAIMED journal record.
    BeforeReclaimedCommit,
}

impl CrashPoint {
    pub fn as_str(&self) -> &'static str {
        match self {
            CrashPoint::AfterPartialWrite => "after-partial-write",
            CrashPoint::AfterOutputFlush => "after-output-flush",
            CrashPoint::AfterRename => "after-rename",
            CrashPoint::AfterJournalCommit => "after-journal-commit",
            CrashPoint::BeforeHolePunch => "before-hole-punch",
            CrashPoint::DuringHolePunch => "during-hole-punch",
            CrashPoint::AfterPhysicalHolePunch => "after-physical-hole-punch",
            CrashPoint::BeforeReclaimedCommit => "before-reclaimed-commit",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<CrashPoint> {
        match s {
            "after-partial-write" => Some(CrashPoint::AfterPartialWrite),
            "after-output-flush" => Some(CrashPoint::AfterOutputFlush),
            "after-rename" => Some(CrashPoint::AfterRename),
            "after-journal-commit" => Some(CrashPoint::AfterJournalCommit),
            "before-hole-punch" => Some(CrashPoint::BeforeHolePunch),
            "during-hole-punch" => Some(CrashPoint::DuringHolePunch),
            "after-physical-hole-punch" => Some(CrashPoint::AfterPhysicalHolePunch),
            "before-reclaimed-commit" => Some(CrashPoint::BeforeReclaimedCommit),
            _ => None,
        }
    }
}

/// The crash point armed via the environment, if any.
pub fn armed_crash_point() -> Option<CrashPoint> {
    std::env::var("RECLAIMARC_FAULT_AT")
        .ok()
        .and_then(|s| CrashPoint::from_str(s.trim()))
}

/// The job id that must match for the crash to fire (optional).
pub fn armed_job_id() -> Option<String> {
    std::env::var("RECLAIMARC_FAULT_JOB").ok()
}

static CRASH_FIRED: AtomicBool = AtomicBool::new(false);

/// Crash the process if the armed point matches.
///
/// Returns `true` when the crash fired (only relevant for the caller in
/// single-process tests).
pub fn fire(point: CrashPoint, job_id: &str) -> bool {
    if CRASH_FIRED.load(Ordering::SeqCst) {
        return false;
    }
    let armed = armed_crash_point();
    let job_ok = armed_job_id().map(|j| j == job_id).unwrap_or(true);
    if armed == Some(point) && job_ok {
        CRASH_FIRED.store(true, Ordering::SeqCst);
        tracing::error!(crash_point = point.as_str(), job_id = %job_id, "FAULT INJECTION: simulating process death");
        std::process::exit(86);
    }
    false
}

/// The exit code used for simulated deaths.
pub const CRASH_EXIT_CODE: i32 = 86;

/// Parse the list of crash points from an env-style string (test helper).
pub fn parse_points(s: &str) -> Vec<CrashPoint> {
    s.split(',').filter_map(CrashPoint::from_str).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_points_roundtrip() {
        for p in [
            CrashPoint::AfterPartialWrite,
            CrashPoint::AfterOutputFlush,
            CrashPoint::AfterRename,
            CrashPoint::AfterJournalCommit,
            CrashPoint::BeforeHolePunch,
            CrashPoint::DuringHolePunch,
            CrashPoint::BeforeReclaimedCommit,
        ] {
            assert_eq!(CrashPoint::from_str(p.as_str()), Some(p));
        }
    }

    #[test]
    fn parse_points_handles_list() {
        let v = parse_points("after-rename,before-hole-punch");
        assert_eq!(
            v,
            vec![CrashPoint::AfterRename, CrashPoint::BeforeHolePunch]
        );
    }
}
