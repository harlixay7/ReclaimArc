//! Safety machinery: emergency reserve, pre-flight capacity validation and
//! free-space monitoring during extraction.
//!
//! The engine never intentionally drives a volume below the reserve. If
//! actual expansion exceeds the estimate, the engine STOPS before consuming
//! the reserve — the current unit's source remains intact, so its partial
//! output can be discarded and retried.

use spacextract_archive::model::RecoveryUnit;

use crate::config::EngineConfig;
use crate::error::CoreError;

/// Validate that the volume has enough free space for a unit's output plus
/// scratch plus the emergency reserve. Fails precisely when not.
pub fn validate_capacity_before_unit(
    dest_dir: &std::path::Path,
    unit: &RecoveryUnit,
    scratch: u64,
    reserve: u64,
) -> Result<(), CoreError> {
    let free = crate::engine::observed_free_space(dest_dir)?;
    let required = unit.unpacked_bytes.saturating_add(scratch).saturating_add(reserve);
    if free < required {
        return Err(CoreError::Infeasible(format!(
            "Unit {} needs {} bytes (output {} + scratch {} + reserve {}) but only {} are free. \
             Stopping before consuming the reserve.",
            unit.seq, required, unit.unpacked_bytes, scratch, reserve, free
        )));
    }
    Ok(())
}

/// Result of a free-space check during extraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpaceCheck {
    Ok,
    /// Free space dropped below the emergency reserve: STOP immediately.
    BelowReserve,
    /// Free space is within `headroom` of the reserve: warn.
    ApproachingReserve,
}

/// Monitor free space while a unit extracts. Called between files and via
/// progress callbacks.
pub struct SpaceMonitor {
    reserve: u64,
    last_free: Option<u64>,
}

impl SpaceMonitor {
    pub fn new(reserve: u64) -> Self {
        SpaceMonitor { reserve, last_free: None }
    }

    /// Check current free space on the volume containing `dir`.
    pub fn check(&mut self, dir: &std::path::Path) -> Result<SpaceCheck, CoreError> {
        let free = crate::engine::observed_free_space(dir)?;
        self.last_free = Some(free);
        if free < self.reserve {
            Ok(SpaceCheck::BelowReserve)
        } else if free < self.reserve.saturating_mul(2) {
            Ok(SpaceCheck::ApproachingReserve)
        } else {
            Ok(SpaceCheck::Ok)
        }
    }

    pub fn last_free(&self) -> Option<u64> {
        self.last_free
    }
}

/// The safety-mode-appropriate confirmation requirement: in SAFE mode the
/// engine retains the previous completed unit's source bytes (no immediate
/// reclamation of the unit before the next one starts).
pub fn retain_previous_unit(config: &EngineConfig) -> bool {
    config.safety_mode == crate::config::SafetyMode::Safe && config.retain_previous_unit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserve_logic() {
        let cfg = EngineConfig::default();
        let r = crate::config::emergency_reserve(1_000_000_000, 500_000_000_000, &cfg);
        assert!(r > 0);
        assert!(r >= crate::config::JOURNAL_REQUIREMENT);
    }
}