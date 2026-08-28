# ReclaimArc System Architecture

```
┌─────────────────────────────── Applications ───────────────────────────────┐
│  apps/cli         reclaimarc (inspect / plan / extract / resume / jobs)     │
│  apps/desktop     Tauri 2 + React + TypeScript GUI                         │
└───────────────────────────────────────▲────────────────────────────────────┘
                                        │ events / commands
┌──────────────────────────────── crates/core ───────────────────────────────┐
│  engine     transactional per-unit lifecycle, pause / stop / cancel        │
│  planner    space simulation, headroom calculation, feasibility verdicts   │
│  safety     emergency reserve, capacity gates, live space monitor          │
│  paths      hostile-path validation, case collision disambiguation         │
│  recovery   discovery, source identity validation, state reconciliation    │
│  state      PENDING -> ... -> RECLAIMED linear state machine               │
│  fault      crash-point injection harness for fault-tolerance tests        │
└───────────────▲───────────────────────┬──────────────────────┬─────────────┘
                │                       │                      │
     ┌──────────┴────────┐      ┌───────┴────────┐     ┌───────┴──────────┐
     │  crates/archive   │      │ crates/journal │     │ crates/platform  │
     │  ArchiveBackend   │      │ SQLite, WAL,   │     │ Windows: sparse  │
     │  RAR (UnRAR FFI)  │      │ synchronous=   │     │ FSCTL_SET_ZERO_  │
     │  ZIP / ZIP64      │      │ FULL, schema   │     │ DATA, allocation │
     │  streaming engine │      │ registry,      │     │ queries, AV lock │
     │  data descriptors │      │ checkpoint     │     │ retries, shell   │
     └───────────────────┘      └────────────────┘     └──────────────────┘
```

---

## 1. Architectural Principles

1. **Decoupled Core Engine**: `crates/core` contains all extraction coordination, space simulation, recovery, and safety policies. Both `apps/cli` and `apps/desktop` are thin drivers consuming engine APIs and event streams.
2. **Fail-Closed Invariants**: If safety cannot be proven before a destructive operation, the engine halts immediately. Source sectors are never deallocated on speculative or unverified data.
3. **Transactional Storage Migration**: Every step in the extraction process follows an ACID state machine backed by an SQLite Write-Ahead Log (WAL).

---

## 2. Archive Backends (`crates/archive`)

All archive formats implement the common `ArchiveBackend` trait:

```rust
pub trait ArchiveBackend: Send + Sync {
    fn inspect(&mut self, options: &OpenOptions) -> Result<ArchiveInfo, ArchiveError>;
    fn test_integrity(&mut self, options: &OpenOptions, tx: &Sender<Event>) -> Result<bool, ArchiveError>;
    fn extract_unit(&mut self, unit: &RecoveryUnit, target_dir: &Path, tx: &Sender<Event>) -> Result<UnitExtractResult, ArchiveError>;
    fn cancel(&mut self);
    fn retirement_proofs(&self, unit: &RecoveryUnit) -> Result<Vec<PackedRange>, ArchiveError>;
}
```

### RAR Engine (`crates/archive/src/rar/`)
- Uses the **official C++ UnRAR library** (`unrar-ng-sys`) for all decompression, ensuring complete fidelity with RAR4 and RAR5 formats.
- Cross-validates proprietary header offsets against UnRAR internal structures during initial inspection.
- Partitions archives into recovery units:
  - **Non-Solid RAR**: Each file constitutes an independent recovery unit.
  - **Solid RAR**: Continuous solid dictionary chains form atomic recovery units.
  - **Multi-Part RAR**: Spanned volume parts are tracked across volume file boundaries.

### ZIP & ZIP64 Engine (`crates/archive/src/zip/`)
- Native Rust decoder supporting standard ZIP and ZIP64 archives with **Deflate** and **Stored** compression.
- **Dual-Parser Data Descriptor Validation**: Handles both standard (16-byte signed/unsigned) and legacy (12-byte without signature) data descriptors, with specific disambiguation for CRC-equals-signature collision cases (`0x08074B50`).
- **1-to-1 Streaming Deallocation**: Streams file bytes directly to verified staging files and generates exact physical retirement proofs.
- **RAII Partial File Guards**: Automatically ensures staged `.sx-partial-*` files are deleted if extraction fails before ownership transfers to the core engine.

---

## 3. ACID Journal & State Registry (`crates/journal`)

- **Per-Job SQLite Journal**: Stored beside the archive in `.reclaimarc/<job-id>/job.db`.
- **Durability Modes**: Configured with `PRAGMA synchronous = FULL` and Write-Ahead Logging (`PRAGMA journal_mode = WAL`).
- **Checkpoint Truncation**: ReclaimArc executes `PRAGMA wal_checkpoint(TRUNCATE)` upon job completion and safe state transitions to prevent WAL file bloating.
- **Global Job Registry**: Tracks active and interrupted jobs in `%LOCALAPPDATA%\ReclaimArc\registry.db` for instant discovery on application startup.
- **Monotonic Space Accounting**: Query-verified physical reclaim metrics ensure recorded physical reclamation never retroactively decreases.

---

## 4. Windows Platform Subsystem (`crates/platform`)

- **Sparse File Deallocation**: Issues `FSCTL_SET_SPARSE` and `FSCTL_SET_ZERO_DATA` aligned inward to 64 KiB NTFS cluster boundaries.
- **Authoritative Allocation Measurement**: Uses `FSCTL_QUERY_ALLOCATED_RANGES` rather than cached metadata (`GetCompressedFileSizeW`) to verify actual physical sector release.
- **Antivirus & Minifilter Lock Resilience**: `longpath::rename_existing` and `longpath::remove_file_existing` implement exponential backoff retry loops (`0ms, 5ms, 15ms, 30ms, 60ms, 100ms, 140ms`) for transient locks (`ERROR_SHARING_VIOLATION` 32, `ERROR_LOCK_VIOLATION` 33, `ERROR_ACCESS_DENIED` 5) caused by Windows Defender (`WdFilter.sys`) or search indexers.
- **Shell & Registry Integration (`crates/platform/src/shell.rs`)**: Manages per-user `SystemFileAssociations` in `HKCU\Software\Classes\SystemFileAssociations\<ext>\shell\ReclaimArc`, enabling right-click Explorer integration without requiring administrative elevation.
- **Atomic File Commits**: Uses `MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH` coupled with `FlushFileBuffers` on file and directory handles.

---

## 5. Core Engine & Safety Planner (`crates/core`)

- **Predictive Space Planner**: Simulates extraction unit by unit prior to starting:
  $$\text{Available Headroom} = \text{Free Space} + \text{Reclaimable Source Bytes} - \text{Unit Output} - \text{Emergency Reserve}$$
- **Case Collision Disambiguation**: Under `ConflictPolicy::RenameNew`, archives containing case-colliding filenames (such as `file.txt` and `FILE.TXT` from Linux systems) are automatically disambiguated to unique paths (e.g. `file (case-collision-1).txt`), allowing full extraction on NTFS without silent overwrites.
- **Configurable Ratio Wiring**: Enforces maximum compression ratio boundaries (`max_compression_ratio`) to defend against decompression bombs.
- **Source Identity Validation**: On resume and finalization, cryptographically confirms source file identity and volume size against journaled fingerprints before removing hollowed source files.