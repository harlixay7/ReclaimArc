# ReclaimArc

Transactional, in-place archive extraction with progressive disk space reclamation for Windows (NTFS / ReFS).

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE-MIT)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE-APACHE)
[![Platform](https://img.shields.io/badge/platform-Windows%2010%2F11%20x64-lightgrey.svg)]()

> **Extract a 100 GB RAR with as little as ~5 GB of free disk space.**
>
> Required headroom depends on the archive's recovery-unit structure.
> ReclaimArc analyzes the archive first and reports whether progressive
> extraction is feasible.

---

## The Problem: Peak Occupied Storage

Standard archive utilities (WinRAR, 7-Zip, PeaZip, and Windows Explorer) require keeping the entire source archive and the uncompressed output on disk simultaneously:

$$\text{Peak Occupied Storage} = \text{Archive Size} + \text{Extracted Output Size}$$

For an 80 GB archive extracting to an 85 GB dataset:
- **Peak occupied storage** reaches ≈ 165 GB;
- A conventional extractor requires **≈ 85 GB of additional free space** before extraction can begin, even though the final dataset only takes 85 GB once the compressed container is removed.

On capacity-constrained drives, standard extractions fail midway, leaving locked partial files, redlined storage, and zero usable data.

---

## How ReclaimArc Works

ReclaimArc converts extraction into a **progressive in-place storage migration**.

Instead of retaining the complete archive until extraction concludes, ReclaimArc frees verified sectors of the source archive progressively throughout the extraction process:

```
Standard Extraction (requires ~85 GB additional free headroom for 165 GB peak storage):
Disk: |======== Source Archive (80 GB) ========|  +  |======== Extracted Output (85 GB) ========|

ReclaimArc Low-Space Extraction (headroom scaled to recovery units + safety reserve):
Step 1: |====== Remaining Archive ======| [Hole]  ───►  |* Verified File 1 (15 GB) *|
Step 2: |==== Remaining Archive ====| [Holes]   ───►  |* Verified File 2 (30 GB) *|
Step 3: |== Remaining Archive ==| [Holes]       ───►  |* Verified File 3 (40 GB) *|
Finish: [ Hollow Source Deleted ]               ───►  |* Complete 85 GB Output *|
```

Using Windows NTFS sparse file deallocation (`FSCTL_SET_ZERO_DATA`), ReclaimArc deallocates physical disk clusters from verified byte ranges of the source archive in real time. The filesystem immediately returns those clusters to the free pool, recycling that disk space to decode subsequent files.

---

## Safety Model & Durability Pipeline

Source data is never deallocated unless the extracted output depending on it is verified, synchronized, and committed. Every recovery unit passes 5 strict safety gates:

```
┌─────────────┐     ┌────────────────┐     ┌───────────────┐     ┌───────────────┐     ┌───────────────────┐
│   Decode    │ ──► │  Disk Read-    │ ──► │ Storage Sync  │ ──► │ Atomic Commit │ ──► │ Journal Commit &  │
│  (UnRAR)    │     │  Back & Verify │     │(FlushBuffers) │     │ (MoveFileExW) │     │ NTFS Hole Punch   │
└─────────────┘     └────────────────┘     └───────────────┘     └───────────────┘     └───────────────────┘
```

1. **Decode**: The file is extracted to an isolated staging path (`<filename>.sx-partial-<job-id>`) via the official C++ UnRAR engine (`unrar-ng-sys`), verifying archive header CRC32 checksums.
2. **Read-Back Verification**: ReclaimArc reads the staged file from disk, validates exact byte length against archive metadata, and computes a **BLAKE3 hash** stored in the ACID journal for durable content provenance.
3. **Storage Sync**: `FlushFileBuffers` requests durable filesystem/device synchronization before commit.
4. **Atomic Commit**: The file is atomically moved into its final path using `MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH`.
5. **Transactional Journal & Hole Punch**: The commit is recorded in an ACID SQLite journal (`synchronous=FULL`, WAL). Only then does ReclaimArc issue `FSCTL_SET_ZERO_DATA` to deallocate source clusters, followed by `FSCTL_QUERY_ALLOCATED_RANGES` to record exact physical space release.

If safety cannot be proven for the next destructive operation, ReclaimArc stops before reclaiming any additional source data. The current uncommitted unit remains recoverable; previously reclaimed source ranges are irreversible.

---

## Crash Resilience & Deterministic Resume

ReclaimArc is engineered to survive unexpected interruptions, power loss, and process termination:

- **Fault-Injection Coverage**: Automated test suites exercise every instrumented transaction boundary, including the crash window after physical deallocation but before journal reconciliation.
- **Deterministic Resume**: Upon restart, the engine reconciles on-disk state against the Write-Ahead Log, discards incomplete staging files, adopts all verified outputs, and resumes seamlessly from the last safe checkpoint.
- **Fail-Closed Integrity**: If an already-committed output file is modified or missing and its source data was already deallocated, the engine halts immediately with an explicit error rather than silently proceeding.

---

## Supported Archives & Recovery Units

| Format | Recovery Unit Granularity | Progressive Reclamation |
|---|---|---|
| **RAR4 / RAR5 (Non-Solid)** | Per-file | Supported (file-by-file in-place reclaim) |
| **RAR4 / RAR5 (Solid Chain)** | Per continuous solid chain | Supported (reclaims after entire chain commits) |
| **Multi-Part RAR Volumes** | Across volume parts | Supported (tracks cross-volume data spans) |
| **Encrypted File Data** | Per-file / Per-chain | Supported (passwords handled in-memory only) |
| **Encrypted Archive Headers** | Entire archive | Normal extraction only (headers unmappable without key) |

> **Note on Solid Archives**: Because solid compression shares a decompression dictionary across consecutive files, earlier packed bytes cannot be deallocated until the entire solid chain is fully decoded and verified. Space planning simulates the largest solid chain requirement before extraction begins.

---

## Limitations

- **Filesystem Support**: In-place sparse hole punching requires Windows NTFS or ReFS on a drive supporting sparse files.
- **Same-Volume Requirement**: Progressive low-space extraction requires the archive and destination to reside on the same filesystem volume so that freed source space immediately increases destination capacity.
- **Symlinks & Reparse Points**: Under default safe policies, archive redirection entries (symlinks, junctions, hardlinks) are skipped to prevent link manipulation attacks.

---

## Applications: Desktop GUI & CLI

ReclaimArc includes both a Windows desktop GUI application and a command-line interface.

### Desktop GUI (Tauri 2 + React)
- Real-time space planning and feasibility analysis.
- Live progress monitoring with exact physical bytes reclaimed.
- Interrupted job discovery, inspection, and one-click resumption.
- Restrained, Windows-native interface built with Segoe UI and accessible diagnostics.

### Command-Line Interface (CLI)
```cmd
# Inspect archive structure and recovery units
reclaimarc.exe inspect "C:\Path\To\Archive.part01.rar"

# Simulate and evaluate space requirements
reclaimarc.exe plan "C:\Path\To\Archive.part01.rar" "D:\Destination"

# Normal extraction (keeps original archive)
reclaimarc.exe extract "C:\Path\To\Archive.part01.rar" "D:\Destination"

# Low-space progressive extraction (reclaims source in-place)
reclaimarc.exe extract "C:\Path\To\Archive.part01.rar" "D:\Destination" --low-space --yes

# List and resume interrupted jobs
reclaimarc.exe jobs
reclaimarc.exe resume "C:\Path\To\Archive.part01.rar"
```

---

## Building from Source

### Prerequisites
- Windows 10 or 11 (64-bit x86_64)
- Visual Studio C++ Build Tools (MSVC with C++ CMake / Clang tools)
- Rust toolchain 1.80+ (`rustup default stable-x86_64-pc-windows-msvc`)
- Node.js 18+ and npm (for desktop frontend)

### Automated Bootstrap
Run the root setup script to verify and configure all dependencies:
```cmd
setup.bat
```

### Manual Build
```cmd
# Build release CLI binary
cargo build --release -p reclaimarc-cli

# Build release Desktop App and installers
cd apps/desktop
npm install
npm run tauri build
```

---

## Verification & Testing

### Automated Test Suite
Run the full workspace automated test suite:
```cmd
cargo test --workspace
```

The test harness exercises:
- Real Windows NTFS sparse deallocation and allocation range queries.
- Fault-injection testing across 8 instrumented process crash boundaries.
- Property-based testing for space planning and state machine transitions.
- Archive parser cross-validation against official UnRAR DLL decoder headers.

### Real-World Hardware Benchmarks
In empirical testing on capacity-constrained systems, ReclaimArc successfully completed large-scale extractions where traditional extractors fail due to insufficient space:

| Parameter | Observed Test Run |
|---|---|
| **Archive Size** | 55.0 GB (RAR container) |
| **Extracted Dataset Size** | > 55.0 GB |
| **Available Free Disk Space at Start** | **20.0 GB** *(Traditional extractors require $\ge 55\text{ GB}$ free space to hold the output)* |
| **Extraction Mode** | Low-Space (Progressive In-Place Reclamation) |
| **Elapsed Duration** | **10–12 minutes** |
| **Integrity & Verification** | 100% verified (Archive CRC32 validated against headers; BLAKE3 provenance recorded in journal) |
| **Disk Space Outcome** | Free space stayed above safety thresholds; source container fully reclaimed |

---

## License

ReclaimArc source code is dual-licensed under either:
- **MIT License** ([LICENSE-MIT](LICENSE-MIT) or [http://opensource.org/licenses/MIT](http://opensource.org/licenses/MIT))
- **Apache License, Version 2.0** ([LICENSE-APACHE](LICENSE-APACHE) or [http://www.apache.org/licenses/LICENSE-2.0](http://www.apache.org/licenses/LICENSE-2.0))

The vendored UnRAR library source code is owned by Alexander Roshal and licensed under the official UnRAR license (`unrar-ng-sys/vendor/unrar/license.txt`).