//! Engine configuration and safety presets.

use serde::{Deserialize, Serialize};

/// Safety presets exposed to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SafetyMode {
    /// Full pre-test; restartable boundaries only; retain one previous
    /// completed unit where practical; full durability flushes; conservative
    /// reserve.
    Safe,
    /// Full pre-test; reclaim immediately after each durable unit;
    /// restartable boundaries only; normal reserve.
    Balanced,
    /// Smallest safe reserve; immediate reclamation at every proven restart
    /// boundary.
    MaximumSpace,
}

impl SafetyMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            SafetyMode::Safe => "safe",
            SafetyMode::Balanced => "balanced",
            SafetyMode::MaximumSpace => "maximum-space",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<SafetyMode> {
        match s {
            "safe" => Some(SafetyMode::Safe),
            "balanced" => Some(SafetyMode::Balanced),
            "maximum-space" => Some(SafetyMode::MaximumSpace),
            _ => None,
        }
    }
}

/// What to do when the destination already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConflictPolicy {
    /// Ask the user (engine pauses and reports).
    Ask,
    /// Keep the existing file, skip the new one.
    Skip,
    /// Write the new file under a unique name.
    RenameNew,
    /// Replace the existing file.
    Overwrite,
}

impl ConflictPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConflictPolicy::Ask => "ask",
            ConflictPolicy::Skip => "skip",
            ConflictPolicy::RenameNew => "rename-new",
            ConflictPolicy::Overwrite => "overwrite",
        }
    }
}

/// Policy for symlink/hardlink entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SymlinkPolicy {
    /// Skip link entries entirely (default, conservative).
    Skip,
    /// Reserved for future policy extensions.
    SafeLinks,
}

/// Engine settings (persisted in the journal as JSON).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineConfig {
    pub safety_mode: SafetyMode,
    pub conflict_policy: ConflictPolicy,
    pub symlink_policy: SymlinkPolicy,
    /// Run the full archive integrity test before destructive extraction.
    pub pre_test: bool,
    /// Reserved for future manifest output.
    #[serde(default)]
    pub write_manifest: bool,
    /// Reserved for conservative mode buffering.
    #[serde(default = "default_true")]
    pub retain_previous_unit: bool,
    /// Custom reserve override (bytes). `None` = automatic.
    pub custom_reserve: Option<u64>,
    /// Delete source archive shells after a successful destructive extraction.
    pub delete_shells_on_completion: bool,
    /// I/O buffer size for verification reads.
    pub io_buffer_size: usize,
    /// Logging level for structured logs.
    pub log_level: String,
}

fn default_true() -> bool {
    true
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            safety_mode: SafetyMode::Balanced,
            conflict_policy: ConflictPolicy::Overwrite,
            symlink_policy: SymlinkPolicy::Skip,
            pre_test: true,
            write_manifest: false,
            retain_previous_unit: true,
            custom_reserve: None,
            delete_shells_on_completion: false,
            io_buffer_size: 1 << 20,
            log_level: "info".into(),
        }
    }
}

/// Fixed minimum emergency reserve (bytes): never drive the volume to zero.
pub const FIXED_MINIMUM_RESERVE: u64 = 512 * 1024 * 1024;
/// Reserve percentage of the filesystem when large.
pub const RESERVE_PERCENT_OF_FILESYSTEM: f64 = 0.01;
/// Journal/transaction overhead allowance.
pub const JOURNAL_REQUIREMENT: u64 = 64 * 1024 * 1024;

/// Compute the emergency reserve:
/// max(fixed minimum, percentage of filesystem, journal requirement).
pub fn emergency_reserve(_free_space: u64, total_space: u64, config: &EngineConfig) -> u64 {
    if let Some(custom) = config.custom_reserve {
        return custom;
    }
    let percent = (total_space as f64 * RESERVE_PERCENT_OF_FILESYSTEM) as u64;
    let base = FIXED_MINIMUM_RESERVE.max(percent).max(JOURNAL_REQUIREMENT);
    match config.safety_mode {
        SafetyMode::Safe => base.saturating_mul(2),
        SafetyMode::Balanced => base,
        SafetyMode::MaximumSpace => {
            // Smallest safe reserve: still enough for the journal itself.
            JOURNAL_REQUIREMENT.max(FIXED_MINIMUM_RESERVE / 4)
        }
    }
}
