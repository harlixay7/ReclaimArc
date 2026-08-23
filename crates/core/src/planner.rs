//! Space planner: simulates extraction over recovery units and decides,
//! before anything is touched, whether an extraction is safe — and if not,
//! exactly why.
//!
//! Simulation per unit:
//!   available = free + source space safely reclaimable from committed units
//!   requirement = unit output + scratch + emergency reserve
//!   if requirement > available → NOT SAFE at this unit
//!   available -= unit output; reclaimable pool += unit packed bytes

use reclaimarc_archive::model::ArchiveInfo;

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

/// Compute the theoretical inward alignment window for a packed range.
pub fn guaranteed_range_reclaim_aligned(
    range: &reclaimarc_archive::model::PackedRange,
    cluster: u32,
) -> u64 {
    const NTFS_DEALLOC_UNIT: u64 = 64 * 1024;
    let unit = (cluster as u64).max(NTFS_DEALLOC_UNIT);
    if unit == 0 {
        return range.len;
    }
    let range_end = range.start.saturating_add(range.len);
    let start = if range.start.is_multiple_of(unit) {
        range.start
    } else {
        (range.start / unit + 1) * unit
    };
    let end = (range_end / unit) * unit;
    if start >= end {
        return 0;
    }
    end - start
}

/// Compute guaranteed physical reclaimable bytes for a packed range.
/// Without physical measurement, returns 0 (fail-closed).
pub fn guaranteed_range_reclaim(
    range: &reclaimarc_archive::model::PackedRange,
    cluster: u32,
) -> u64 {
    guaranteed_range_reclaim_measured(range, cluster, None)
}

/// Compute guaranteed physical reclaimable bytes for a packed range taking
/// into account physical allocation and inward deallocation alignment.
pub fn guaranteed_range_reclaim_measured(
    range: &reclaimarc_archive::model::PackedRange,
    cluster: u32,
    allocated: Option<&[reclaimarc_platform::sparse::ByteRange]>,
) -> u64 {
    const NTFS_DEALLOC_UNIT: u64 = 64 * 1024;
    let unit = (cluster as u64).max(NTFS_DEALLOC_UNIT);
    if unit == 0 {
        return 0;
    }
    let range_end = range.start.saturating_add(range.len);
    let start = if range.start.is_multiple_of(unit) {
        range.start
    } else {
        (range.start / unit + 1) * unit
    };
    let end = (range_end / unit) * unit;
    if start >= end {
        return 0;
    }
    let aligned_window = reclaimarc_platform::sparse::ByteRange {
        start,
        len: end - start,
    };

    if let Some(alloc_list) = allocated {
        let mut total_allocated = 0u64;
        for alloc in alloc_list {
            if let Some(overlap) = aligned_window.intersect(alloc) {
                total_allocated = total_allocated.saturating_add(overlap.len);
            }
        }
        total_allocated
    } else {
        // Measurement unavailable: credit ZERO future reclaim (fail-closed).
        0
    }
}

/// Simulate the extraction and produce a plan.
pub fn plan(
    info: &ArchiveInfo,
    free_space: u64,
    total_space: u64,
    config: &EngineConfig,
) -> Result<SpacePlan, CoreError> {
    plan_with_measurements(info, free_space, total_space, None, None, config)
}

/// Simulate extraction with platform measurements (actual volume cluster geometry and physical allocation).
pub fn plan_with_measurements(
    info: &ArchiveInfo,
    free_space: u64,
    total_space: u64,
    cluster_size: Option<u32>,
    allocated_by_volume: Option<
        &std::collections::HashMap<u64, Vec<reclaimarc_platform::sparse::ByteRange>>,
    >,
    config: &EngineConfig,
) -> Result<SpacePlan, CoreError> {
    if info.recovery_units.is_empty() {
        return Err(CoreError::Infeasible("archive contains no entries".into()));
    }
    let geometry_known = cluster_size.map(|c| c > 0).unwrap_or(false);
    let cluster = cluster_size.unwrap_or(0);
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

    // Progressive capability check: encrypted headers or unsupported format.
    if !info.capability.progressive_reclaim {
        let reason = if info.encrypted_headers {
            "Progressive reclamation is unsupported for archives with encrypted headers because packed ranges cannot be mapped without decryption."
        } else {
            "Progressive reclamation is unsupported for this archive structure."
        };
        return Ok(SpacePlan {
            progressive_feasible: false,
            normal_feasible,
            free_now: free_space,
            unpacked_total,
            progressive_peak_requirement: if normal_feasible {
                0
            } else {
                unpacked_total.saturating_sub(free_space.saturating_sub(reserve))
            },
            reserve,
            scratch,
            largest_unit_bytes: largest_unit,
            estimated_source_reclaim: 0,
            reason: Some(reason.into()),
        });
    }

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
                "Recovery unit {} (entries {}..={}) needs {} bytes but only {} are available after all safe reclamation{}. \
                 Additional space required: {} bytes.",
                unit.seq,
                unit.first_entry,
                unit.last_entry,
                requirement,
                available,
                if !geometry_known { " (volume cluster geometry could not be measured; 0 bytes reclaim credited)" } else { "" },
                deficit
            ));
            if unit.packed_ranges.is_empty() || !geometry_known {
                // Nothing to reclaim from this unit or geometry unknown.
                break;
            }
        }
        available = available.saturating_sub(unit.unpacked_bytes);
        // After this unit commits, its guaranteed physical source bytes become reclaimable.
        let unit_reclaim: u64 = if geometry_known {
            unit.packed_ranges
                .iter()
                .map(|r| {
                    let vol_alloc = allocated_by_volume
                        .and_then(|m| m.get(&r.volume_index).map(|v| v.as_slice()));
                    guaranteed_range_reclaim_measured(r, cluster, vol_alloc)
                })
                .sum()
        } else {
            0
        };
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
    use proptest::prelude::*;
    use reclaimarc_archive::model::{
        CapabilityMatrix, DecoderRequirements, Entry, PackedRange, RecoveryUnit, VolumeInfo,
    };

    const GIB: u64 = 1024 * 1024 * 1024;

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
        let mut offset = 0u64;
        for (i, (pack, unp)) in units.iter().enumerate() {
            let idx = i as u64;
            recovery_units.push(RecoveryUnit {
                seq: idx,
                first_entry: idx,
                last_entry: idx,
                packed_ranges: vec![PackedRange {
                    volume_index: 0,
                    start: offset,
                    len: *pack,
                }],
                unpacked_bytes: *unp,
            });
            offset = offset.saturating_add(*pack);
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

    fn fake_allocations(
        info: &ArchiveInfo,
    ) -> std::collections::HashMap<u64, Vec<reclaimarc_platform::sparse::ByteRange>> {
        let mut map = std::collections::HashMap::new();
        for u in &info.recovery_units {
            for r in &u.packed_ranges {
                map.entry(r.volume_index).or_insert_with(Vec::new).push(
                    reclaimarc_platform::sparse::ByteRange {
                        start: r.start,
                        len: r.len,
                    },
                );
            }
        }
        map
    }

    #[test]
    fn normal_extraction_needs_everything_at_once() {
        // 100 GB unpacked, 34.8 GB free → normal impossible.
        let info = fake_info(&[(100_400_000_000, 102_100_000_000)], 34_800_000_000);
        let plan = plan(
            &info,
            34_800_000_000,
            500_000_000_000,
            &EngineConfig::default(),
        )
        .unwrap();
        assert!(!plan.normal_feasible);
        // Progressive also impossible (single unit, nothing to reclaim).
        assert!(!plan.progressive_feasible);
        assert!(plan.reason.is_some());
    }

    #[test]
    fn test_guaranteed_range_reclaim_alignment() {
        // Range smaller than 64 KiB yields 0 guaranteed reclaim
        let small = PackedRange {
            volume_index: 0,
            start: 0,
            len: 65535,
        };
        assert_eq!(guaranteed_range_reclaim_aligned(&small, 4096), 0);

        // Exact 64 KiB yields 64 KiB
        let exact = PackedRange {
            volume_index: 0,
            start: 0,
            len: 65536,
        };
        assert_eq!(guaranteed_range_reclaim_aligned(&exact, 4096), 65536);

        // Unaligned start and end: 1000..150000 -> inward aligned: 65536..131072 (65536 bytes)
        let unaligned = PackedRange {
            volume_index: 0,
            start: 1000,
            len: 149000,
        };
        assert_eq!(guaranteed_range_reclaim_aligned(&unaligned, 4096), 65536);
    }

    #[test]
    fn progressive_works_when_reclaim_fuels_units() {
        // Two units: each 10 GiB packed (aligned), 12 GiB unpacked. Reserve = 5 GiB.
        const GIB: u64 = 1024 * 1024 * 1024;
        let pack = 10 * GIB;
        let unp = 12 * GIB;
        let free = 19 * GIB;
        let info = fake_info(&[(pack, unp), (pack, unp)], free);
        let allocs = fake_allocations(&info);

        // Case A: Measured physical allocation available -> Progressive succeeds
        let plan = plan_with_measurements(
            &info,
            free,
            500 * GIB,
            Some(4096),
            Some(&allocs),
            &EngineConfig::default(),
        )
        .unwrap();
        assert!(!plan.normal_feasible, "normal needs 24 GiB + reserve");
        assert!(
            plan.progressive_feasible,
            "progressive recycles source: {plan:?}"
        );
        assert_eq!(plan.estimated_source_reclaim, 20 * GIB);
        assert_eq!(plan.progressive_peak_requirement, 0);

        // Case B: Measurement unavailable (None) -> credits ZERO future reclaim (fail-closed)
        let unmeasured_plan = plan_with_measurements(
            &info,
            free,
            500 * GIB,
            Some(4096),
            None,
            &EngineConfig::default(),
        )
        .unwrap();
        assert_eq!(unmeasured_plan.estimated_source_reclaim, 0);
        assert!(!unmeasured_plan.progressive_feasible);

        // With 18 GiB free, unit 1 is short by exactly 1 GiB → NOT SAFE.
        let info2 = fake_info(&[(pack, unp), (pack, unp)], 18 * GIB);
        let allocs2 = fake_allocations(&info2);
        let plan2 = plan_with_measurements(
            &info2,
            18 * GIB,
            500 * GIB,
            Some(4096),
            Some(&allocs2),
            &EngineConfig::default(),
        )
        .unwrap();
        assert!(!plan2.progressive_feasible);
        assert_eq!(plan2.progressive_peak_requirement, GIB);
        let reason = plan2.reason.as_ref().unwrap();
        assert!(reason.contains(&format!("Additional space required: {GIB} bytes")));
    }

    #[test]
    fn unknown_cluster_geometry_credits_zero_reclaim_and_fails_closed() {
        let pack = 10 * GIB;
        let unp = 10 * GIB;
        let info = fake_info(&[(pack, unp), (pack, unp)], 15 * GIB);
        let plan = plan_with_measurements(
            &info,
            15 * GIB,
            500 * GIB,
            None, // unknown cluster geometry
            None,
            &EngineConfig::default(),
        )
        .unwrap();
        assert!(!plan.progressive_feasible);
        assert_eq!(plan.estimated_source_reclaim, 0);
        let reason = plan.reason.as_ref().unwrap();
        assert!(reason.contains("volume cluster geometry could not be measured"));
    }

    #[test]
    fn solid_single_chain_reports_true_requirement() {
        let info = fake_info(&[(42_600_000_000, 46_000_000_000)], 3_000_000_000);
        let plan = plan(
            &info,
            3_000_000_000,
            500_000_000_000,
            &EngineConfig::default(),
        )
        .unwrap();
        assert!(!plan.normal_feasible);
        assert!(!plan.progressive_feasible);
        assert!(plan.reason.unwrap().contains("Recovery unit 0"));
    }

    #[test]
    fn small_archive_fits_normally() {
        // Volume is 8 GB: 1% reserve = 80 MB; fixed minimum 512 MB dominates.
        let info = fake_info(&[(100, 100), (200, 200)], 1_000_000_000);
        let plan = plan(
            &info,
            1_000_000_000,
            8_000_000_000,
            &EngineConfig::default(),
        )
        .unwrap();
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
