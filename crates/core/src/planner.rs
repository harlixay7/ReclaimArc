//! Space planner: simulates extraction over recovery units and decides,
//! before anything is touched, whether an extraction is safe — and if not,
//! exactly why.
//!
//! Simulation per unit:
//!   available = free + source space safely reclaimable from committed units
//!   requirement = unit output + scratch + emergency reserve
//!   if requirement > available → NOT SAFE at this unit
//!   available -= unit output; reclaimable pool += unit packed bytes

use spacextract_archive::model::ArchiveInfo;

use crate::config::{emergency_reserve, EngineConfig};
use crate::error::CoreError;

/// The verdict of the space simulation.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SpacePlan {
    /// Whether progressive extraction can complete safely.
    pub progressive_feasible: bool,
    /// Whether a normal extraction can complete safely.
    pub normal_feasible: bool,
    /// Free space on the destination volume now.
    pub free_now: u64,
    /// Total unpacked bytes that must be written.
    pub unpacked_total: u64,
    /// Peak extra requirement of progressive extraction beyond current free
    /// space (0 = fits without reclamation).
    pub progressive_peak_requirement: u64,
    /// Emergency reserve applied.
    pub reserve: u64,
    /// Scratch requirement of the decoder.
    pub scratch: u64,
    /// Largest single recovery unit (output bytes).
    pub largest_unit_bytes: u64,
    /// Source bytes estimated to become reclaimable.
    pub estimated_source_reclaim: u64,
    /// Why the plan is infeasible (when it is).
    pub reason: Option<String>,
}

impl SpacePlan {
    /// The fixed minimum the UI shows: "normal extraction requirement".
    pub fn normal_requirement(&self) -> u64 {
        self.unpacked_total
    }
}

/// Simulate the extraction and produce a plan.
pub fn plan(
    info: &ArchiveInfo,
    free_space: u64,
    total_space: u64,
    config: &EngineConfig,
) -> Result<SpacePlan, CoreError> {
    if info.recovery_units.is_empty() {
        return Err(CoreError::Infeasible("archive contains no entries".into()));
    }
    let reserve = emergency_reserve(free_space, total_space, config);
    let scratch = info.decoder_requirements.scratch_bytes;

    let unpacked_total: u64 = info.entries.iter().map(|e| e.unpacked_size).sum();
    let largest_unit = info
        .recovery_units
        .iter()
        .map(|u| u.unpacked_bytes)
        .max()
        .unwrap_or(0);

    // Normal extraction: everything at once.
    let normal_feasible = free_space
        .checked_sub(reserve)
        .map(|f| f >= unpacked_total + scratch)
        .unwrap_or(false);

    // Progressive simulation.
    let mut available = free_space;
    let mut reclaimable_pool: u64 = 0;
    let mut peak_deficit: u64 = 0;
    let mut estimated_reclaim: u64 = 0;
    let mut reason = None;

    for unit in &info.recovery_units {
        // Space gained from previously committed units becomes available.
        available = available.saturating_add(reclaimable_pool);
        estimated_reclaim = estimated_reclaim.saturating_add(reclaimable_pool);
        reclaimable_pool = 0;

        let requirement = unit
            .unpacked_bytes
            .saturating_add(scratch)
            .saturating_add(reserve);
        if requirement > available {
            let deficit = requirement - available;
            peak_deficit = peak_deficit.max(deficit);
            reason = Some(format!(
                "Recovery unit {} (entries {}..={}) needs {} bytes but only {} are available after all safe reclamation. \
                 Additional space required: {} bytes.",
                unit.seq,
                unit.first_entry,
                unit.last_entry,
                requirement,
                available,
                deficit
            ));
            if unit.packed_ranges.is_empty() {
                // Nothing to reclaim from this unit ever.
                break;
            }
        }
        available = available.saturating_sub(unit.unpacked_bytes);
        // After this unit commits, its packed source bytes become reclaimable.
        let unit_reclaim: u64 = unit.packed_ranges.iter().map(|r| r.len).sum();
        reclaimable_pool = unit_reclaim;
    }

    if reason.is_none() {
        // Last unit's reclaim was never consumed.
        estimated_reclaim = estimated_reclaim.saturating_add(reclaimable_pool);
    }

    let progressive_feasible = reason.is_none();

    Ok(SpacePlan {
        progressive_feasible,
        normal_feasible,
        free_now: free_space,
        unpacked_total,
        progressive_peak_requirement: peak_deficit,
        reserve,
        scratch,
        largest_unit_bytes: largest_unit,
        estimated_source_reclaim: estimated_reclaim.min(info.packed_size),
        reason,
    })
}

/// The maximum extraction mode that is feasible: 0 = none, 1 = normal,
/// 2 = progressive.
pub fn feasible_mode(plan: &SpacePlan) -> u8 {
    if plan.progressive_feasible {
        2
    } else if plan.normal_feasible {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spacextract_archive::model::{CapabilityMatrix, DecoderRequirements, Entry, PackedRange, RecoveryUnit, VolumeInfo};
    use proptest::prelude::*;

    fn fake_info(units: &[(u64, u64)], free: u64) -> ArchiveInfo {
        let mut entries = Vec::new();
        for (i, (pack, unp)) in units.iter().enumerate() {
            entries.push(Entry {
                index: i as u64,
                name: format!("f{i}.bin"),
                packed_size: *pack,
                unpacked_size: *unp,
                crc32: None,
                is_directory: false,
                is_solid: false,
                split_before: false,
                split_after: false,
                encrypted: false,
                redirection: None,
            });
        }
        let mut recovery_units = Vec::new();
        let mut idx = 0u64;
        for (i, (pack, unp)) in units.iter().enumerate() {
            recovery_units.push(RecoveryUnit {
                seq: i as u64,
                first_entry: idx,
                last_entry: idx,
                packed_ranges: vec![PackedRange {
                    volume_index: 0,
                    start: 0,
                    len: *pack,
                }],
                unpacked_bytes: *unp,
            });
            idx += 1;
        }
        let _ = free;
        ArchiveInfo {
            format: "rar5".into(),
            packed_size: units.iter().map(|(p, _)| p).sum(),
            unpacked_size: units.iter().map(|(_, u)| u).sum(),
            solid_archive: false,
            encrypted_headers: false,
            volumes: vec![VolumeInfo {
                index: 0,
                path: "a.rar".into(),
                logical_size: 0,
            }],
            entries,
            recovery_units,
            capability: CapabilityMatrix {
                format: "rar5".into(),
                supports_test_integrity: true,
                restartable_units: true,
                progressive_reclaim: true,
                supports_encryption: true,
                supports_multipart: false,
                notes: vec![],
            },
            decoder_requirements: DecoderRequirements {
                scratch_bytes: 0,
                redecodes_prefix: false,
            },
        }
    }

    #[test]
    fn normal_extraction_needs_everything_at_once() {
        // 100 GB unpacked, 34.8 GB free → normal impossible.
        let info = fake_info(&[(100_400_000_000, 102_100_000_000)], 34_800_000_000);
        let plan = plan(&info, 34_800_000_000, 500_000_000_000, &EngineConfig::default()).unwrap();
        assert!(!plan.normal_feasible);
        // Progressive also impossible (single unit, nothing to reclaim).
        assert!(!plan.progressive_feasible);
        assert!(plan.reason.is_some());
    }

    #[test]
    fn progressive_works_when_reclaim_fuels_units() {
        // Two units: each 10 GB packed, 12 GB unpacked. Reserve = 5 GB.
        // Unit 0 needs 12+5 = 17 GB. With 19 GB free: unit 0 fits (19-12=7),
        // then unit 0's 10 GB source is reclaimed before unit 1 → 7+10 = 17
        // = exactly unit 1's requirement.
        let info = fake_info(&[(10_000_000_000, 12_000_000_000), (10_000_000_000, 12_000_000_000)], 19_000_000_000);
        let plan = plan(&info, 19_000_000_000, 500_000_000_000, &EngineConfig::default()).unwrap();
        assert!(!plan.normal_feasible, "normal needs 24 GB + reserve");
        assert!(plan.progressive_feasible, "progressive recycles source: {plan:?}");
        assert_eq!(plan.estimated_source_reclaim, 20_000_000_000);
        assert_eq!(plan.progressive_peak_requirement, 0);

        // With 18 GB free, unit 1 is short by exactly 1 GB → NOT SAFE.
        let info2 = fake_info(&[(10_000_000_000, 12_000_000_000), (10_000_000_000, 12_000_000_000)], 18_000_000_000);
        let plan2 = crate::planner::plan(&info2, 18_000_000_000, 500_000_000_000, &EngineConfig::default()).unwrap();
        assert!(!plan2.progressive_feasible);
        assert_eq!(plan2.progressive_peak_requirement, 1_000_000_000);
        let reason = plan2.reason.as_ref().unwrap();
        assert!(reason.contains("Additional space required: 1000000000 bytes"));
    }

    #[test]
    fn solid_single_chain_reports_true_requirement() {
        let info = fake_info(&[(42_600_000_000, 46_000_000_000)], 3_000_000_000);
        let plan = plan(&info, 3_000_000_000, 500_000_000_000, &EngineConfig::default()).unwrap();
        assert!(!plan.normal_feasible);
        assert!(!plan.progressive_feasible);
        assert!(plan.reason.unwrap().contains("Recovery unit 0"));
    }

    #[test]
    fn small_archive_fits_normally() {
        // Volume is 8 GB: 1% reserve = 80 MB; fixed minimum 512 MB dominates.
        let info = fake_info(&[(100, 100), (200, 200)], 1_000_000_000);
        let plan = plan(&info, 1_000_000_000, 8_000_000_000, &EngineConfig::default()).unwrap();
        assert!(plan.normal_feasible);
        assert!(plan.progressive_feasible);
        assert_eq!(plan.progressive_peak_requirement, 0);
        assert_eq!(plan.largest_unit_bytes, 200);
        assert_eq!(plan.unpacked_total, 300);
    }

    // Property: if the plan says feasible, the sequential simulation never
    // dips below the reserve (verified independently of the planner).
    proptest! {
        #[test]
        fn feasible_plans_never_dip_below_reserve(
            free in 1_000_000_000u64..1_000_000_000_000u64,
            units in prop::collection::vec((100_000_000u64..5_000_000_000u64, 100_000_000u64..5_000_000_000u64), 1..10),
        ) {
            let info = fake_info(&units, free);
            let plan = plan(&info, free, free * 4 + 1, &EngineConfig::default()).unwrap();
            if !plan.progressive_feasible {
                return Ok(());
            }
            let reserve = plan.reserve;
            let scratch = plan.scratch;
            let mut available = plan.free_now;
            let mut reclaim = 0u64;
            for unit in &info.recovery_units {
                available = available.saturating_add(reclaim);
                let requirement = unit.unpacked_bytes.saturating_add(scratch).saturating_add(reserve);
                prop_assert!(available >= requirement,
                    "simulation violated safety at unit {}: available {} < requirement {}",
                    unit.seq, available, requirement);
                available = available.saturating_sub(unit.unpacked_bytes);
                reclaim = unit.packed_ranges.iter().map(|r| r.len).sum();
            }
            // After the final reclaim, free space must still respect reserve.
            prop_assert!(available + reclaim >= reserve);
        }

        #[test]
        fn infeasible_plans_explain(units in prop::collection::vec((1_000_000_000u64..9_000_000_000u64, 1_000_000_000u64..9_000_000_000u64), 2..8), free in 100_000_000u64..1_000_000_000u64) {
            let info = fake_info(&units, free);
            let plan = plan(&info, free, free * 4 + 1, &EngineConfig::default()).unwrap();
            if !plan.progressive_feasible {
                let reason = plan.reason.as_ref().expect("infeasible plan must have a reason");
                prop_assert!(reason.contains("Recovery unit"));
            }
        }
    }
}