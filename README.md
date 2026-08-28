# ReclaimArc

Transactional in-place archive extraction with progressive disk space reclamation for Windows (NTFS / ReFS).

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE-MIT)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE-APACHE)
[![Platform](https://img.shields.io/badge/platform-Windows%2010%2F11%20x64-lightgrey.svg)]()
[![Tests](https://img.shields.io/badge/tests-179%20passed-brightgreen.svg)]()

> **"I have an archive I need to extract, but I have no storage left. How can I extract it?"**
>
> Standard archive utilities fail when you run out of drive storage because they require holding both the complete compressed archive and all extracted files on disk at the same time.
> **ReclaimArc extracts ZIP and RAR archives progressively and reclaims verified disk sectors from the archive as it proceeds**, allowing you to extract a 100 GB archive with as little as 5 to 10 GB of available disk headroom.

---

## The Problem: The Peak Storage Requirement

When you extract a large `.zip` or `.rar` file using traditional utilities (such as WinRAR, 7-Zip, PeaZip, or Windows Explorer), your drive must accommodate both files simultaneously:

$$\text{Peak Storage Required} = \text{Archive Size} + \text{Extracted Output Size}$$

For an 80 GB archive extracting to an 85 GB output folder:
- **Peak occupied storage** reaches 165 GB.
- Conventional extractors demand **85 GB of additional free space** before extraction can begin, even though the final output only needs 85 GB once the archive is gone.
- If available storage drops below 85 GB, extraction fails midway with a **"Not enough disk space"** error, leaving incomplete files, a full drive, and no usable output.

Users facing this storage barrier are usually stuck choosing between:
1. Deleting personal files to create temporary room,
2. Moving large multi-gigabyte archives to external drives over slow USB connections, or
3. Giving up on extracting the archive altogether.

---

## How ReclaimArc Solves This

ReclaimArc turns extraction into a **progressive in-place storage migration**.

Instead of waiting until the entire extraction finishes to free up space, ReclaimArc extracts file by file (or block by block), verifies each output file on disk, commits it safely, and immediately frees the underlying physical sectors from the source archive in real time.

```
Standard Extraction (Requires ~85 GB additional free headroom for 165 GB peak storage):
Disk: |======== Source Archive (80 GB) ========|  +  |======== Extracted Output (85 GB) ========|

ReclaimArc Low-Space Extraction (Headroom scaled to largest recovery unit + safety reserve):
Step 1: |====== Remaining Archive ======| [Freed] ───►  |* Verified File 1 (15 GB) *|
Step 2: |==== Remaining Archive ====| [Freed]     ───►  |* Verified File 2 (30 GB) *|
Step 3: |== Remaining Archive ==| [Freed]         ───►  |* Verified File 3 (40 GB) *|
Finish: [ Empty Source Cleaned Up ]               ───►  |* Complete 85 GB Output *|
```

By leveraging native Windows NTFS sparse deallocation (`FSCTL_SET_ZERO_DATA`), the filesystem returns deallocated physical clusters directly to your drive's free pool. The newly freed space is recycled immediately to decode subsequent files without running out of disk space.

---

## Comparison: WinRAR / 7-Zip vs. ReclaimArc

| Feature | WinRAR / 7-Zip | ReclaimArc |
|---|---|---|
| **Space Required Upfront** | Full archive + Full uncompressed output | Small headroom (largest file/unit + safety buffer) |
| **Space Reclaimed During Extraction** | None (only after full extraction if manually deleted) | Continuous (sectors reclaimed immediately upon file verification) |
| **Extract 100 GB with 10 GB Free Space** | Impossible (fails with "Not enough disk space") | Supported and verified on real hardware |
| **Crash Recovery & Resume** | None (must start over from 0%) | Deterministic resume from last committed checkpoint |
| **Integrity Verification** | Header CRC only | Header CRC + Full Disk Read-Back BLAKE3 Cryptographic Proof |
| **Antivirus Lock Handling** | Fails on transient file locks | Automatic exponential backoff retries on Win32 sharing locks |
| **Windows Explorer Integration** | Context menu integration | Native right-click integration (0 UAC / no admin required) |
| **Case Collision Disambiguation** | Overwrites or fails on NTFS | Automatic collision disambiguation under RenameNew policy |
| **Verified Source Deletion** | Manual deletion required | Automatic cleanup after 100% verified output validation |

---

## Supported Archive Formats & Recovery Units

ReclaimArc organizes deallocation around format-specific decompression dependencies:

| Format | Recovery Unit Granularity | Progressive In-Place Reclamation |
|---|---|---|
| **ZIP / ZIP64 (Stored & Deflate)** | Per-file | Supported (stream-aligned deallocation, dual-parser descriptor validation) |
| **RAR4 / RAR5 (Non-Solid)** | Per-file | Supported (file-by-file in-place reclaim via UnRAR engine) |
| **RAR4 / RAR5 (Solid Chain)** | Per solid dictionary run | Supported (reclaims source after complete solid chain commits) |
| **Multi-Part RAR Sets (.part01.rar)** | Cross-volume data spans | Supported (tracks physical byte boundaries across multi-volume spans) |
| **Password Encrypted Files (ZIP/RAR)**| Per-file / Per-chain | Supported (passwords held in memory only, zeroed on completion) |
| **Encrypted Archive Headers** | Entire archive | Standard extraction only (metadata unmappable without password) |

---

## Safety Model & Five-Stage Durability Pipeline

Source data is **never deallocated** until its extracted output has been completely decoded, verified from physical storage, synchronized, atomically renamed, and journaled.

```
┌─────────────┐     ┌────────────────┐     ┌───────────────┐     ┌───────────────┐     ┌───────────────────┐
│   Decode    │ ──► │  Disk Read-    │ ──► │ Storage Sync  │ ──► │ Atomic Commit │ ──► │ Journal Commit &  │
│ (UnRAR/ZIP) │     │  Back & Verify │     │(FlushBuffers) │     │ (MoveFileExW) │     │ NTFS Hole Punch   │
└─────────────┘     └────────────────┘     └───────────────┘     └───────────────┘     └───────────────────┘
```

1. **Decode to Staging**: Data is extracted to an isolated temporary staging file (`<name>.sx-partial-<job-id>`) while validating archive CRC32 checksums.
2. **Read-Back Verification**: The staged file is read back from disk to verify length and compute a **BLAKE3 hash**, stored in the journal for tamper-proof provenance.
3. **Storage Sync**: `FlushFileBuffers` enforces physical media persistence before moving files.
4. **Atomic Commit**: The file is atomically moved to its destination path using `MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH`.
5. **ACID Journaling & Physical Hole Punch**: The transaction is committed to an ACID SQLite database (`synchronous=FULL`, WAL mode). Only then is `FSCTL_SET_ZERO_DATA` issued to deallocate source clusters, followed by `FSCTL_QUERY_ALLOCATED_RANGES` to confirm actual physical space release.

If safety cannot be proven for the next step, ReclaimArc stops immediately before touching source bytes. The uncommitted unit remains fully recoverable; previously reclaimed source sectors are irreversible.

---

## Using ReclaimArc

### 1. Windows Explorer Right-Click Integration
ReclaimArc integrates directly with Windows File Explorer:
1. Right-click any supported archive (`.zip`, `.rar`, etc.).
2. Select **"Analyze with ReclaimArc"**.
3. ReclaimArc launches, analyzes the archive, calculates disk headroom, and displays the **Space Feasibility Report**.
4. Click **Start Extraction** (or **Low-Space Extraction**).

*Note: Context menu integration registers in `HKEY_CURRENT_USER` and requires **zero administrator privileges / no UAC elevation**.*

### 2. Desktop GUI Application
- **Visual Feasibility Report**: Displays total uncompressed size, available disk space, and exact headroom requirements before starting.
- **Real-Time Progress**: Live monitoring of written output, verified bytes, physical source space reclaimed, and volume free space.
- **Crash Recovery & Resume**: Automatically discovers interrupted jobs on startup with one-click resume.
- **Settings Control**: Toggle Windows Explorer integration, conflict resolution policies (Rename New, Overwrite, Skip, Ask), integrity pre-tests, and auto-deletion of completed archives.

### 3. Command-Line Interface (CLI)
```cmd
# Analyze archive structure and recovery unit breakdown
reclaimarc.exe inspect "C:\Archives\large_game.zip"

# Simulate space requirements for a target destination
reclaimarc.exe plan "C:\Archives\large_backup.rar" "D:\Extracted"

# Normal extraction (preserves original archive on disk)
reclaimarc.exe extract "C:\Archives\large_backup.rar" "D:\Extracted"

# Low-space progressive extraction (reclaims source in-place)
reclaimarc.exe extract "C:\Archives\large_game.zip" "D:\Extracted" --low-space --yes

# List and resume interrupted jobs
reclaimarc.exe jobs
reclaimarc.exe resume "C:\Archives\large_game.zip"
```

---

## Frequently Asked Questions (FAQ)

### Q: I have a huge archive and almost no free disk space. Can ReclaimArc extract it?
**Yes.** This is the exact scenario ReclaimArc was built for. As long as you have enough free headroom for the largest single file/chunk in the archive plus a small safety buffer (typically 2 to 10 GB), ReclaimArc can extract a 50 GB, 100 GB, or larger archive without running out of disk space.

### Q: Will Low-Space Extraction modify or delete my original archive?
**Yes, by design.** Low-Space Extraction is an in-place deallocation process: it removes physical disk sectors from the source archive as each extracted file is verified, freeing up disk clusters for subsequent files. Because deallocated source sectors cannot be restored once zeroed, you should use **Normal Extraction** if you have enough free space and wish to preserve the original container, or keep an external backup for irreplaceable files.

### Q: What happens if my computer crashes or loses power during extraction?
ReclaimArc utilizes an ACID SQLite Write-Ahead Log (WAL) journal. If power is lost or the process is interrupted, ReclaimArc cleans up any incomplete staging files upon restart, keeps all already-verified output files, and resumes extraction from the last safe committed checkpoint.

### Q: Why does ReclaimArc require the archive and output to be on the same drive for low-space mode?
Progressive space reclamation relies on releasing space from the source container so the destination folder can reuse those newly freed disk clusters. If the archive is on Drive C: and the destination is on Drive D:, freeing space on C: does not increase free space on D:.

### Q: Can ReclaimArc extract solid RAR archives with low disk space?
**Yes, subject to solid chain size.** In solid RAR archives, files within a continuous solid dictionary chain depend on earlier files. ReclaimArc analyzes solid chains and reclaims source space after each complete solid chain finishes and commits. The feasibility analyzer reports the exact headroom needed before extraction starts.

### Q: Does ReclaimArc require administrator rights?
**No.** ReclaimArc operates entirely in standard user space. NTFS sparse file operations and user context menu integration (`HKCU\Software\Classes\SystemFileAssociations`) require no elevated permissions.

---

## Hardware Verification & Benchmarks

In physical hardware testing on storage-constrained volumes, ReclaimArc successfully completed extractions where conventional extractors failed due to disk exhaustion:

| Benchmark Parameter | Result |
|---|---|
| **Archive Container Size** | 55.0 GB (RAR container) |
| **Extracted Dataset Output** | > 55.0 GB uncompressed data |
| **Available Disk Space at Start** | **20.0 GB** *(Standard extractors require $\ge 55\text{ GB}$ free space)* |
| **Extraction Mode** | Low-Space (Progressive In-Place Reclamation) |
| **Duration** | **10–12 minutes** on NVMe SSD |
| **Verification** | 100% verified (CRC32 validated against headers; BLAKE3 hash provenance recorded) |
| **Disk Headroom Outcome** | Free space remained above the emergency reserve at all times |

---

## Building from Source

### Prerequisites
- Windows 10 or 11 (64-bit x86_64)
- Rust toolchain 1.88+ (`rustup default stable-x86_64-pc-windows-msvc`)
- Visual Studio C++ Build Tools (MSVC)
- Node.js 20+ and npm (for Desktop GUI)

### Quick Setup Scripts
- **`setup.bat`**: Audits and installs toolchains, MSVC build dependencies, and Node packages idempotently.
- **`run.bat`**: Interactive launcher for Desktop GUI, CLI, test suite, and release builds.

### Manual Build
```cmd
# Build release CLI binary
cargo build --release -p reclaimarc-cli

# Build release Desktop App and NSIS installer
cd apps/desktop
npm install
npm run tauri build
```

---

## Testing & Quality Assurance

Run the comprehensive test suite across all workspace crates:
```cmd
cargo test --workspace --all-features
```

The automated test harness comprises **179 automated tests**, including:
- **66 Fault-Injection Tests**: Exercising every transaction crash boundary, mid-flight deallocation, and resume reconciliation.
- **5 ZIP Engine Topology Tests**: Validating directory topologies, empty files, and interleaved entries.
- **4 Real-World Stress Tests**: Validating multi-gigabyte ZIP/ZIP64 streams and mid-extraction process interruptions.
- **Case Collision Tests**: Validating conflict-aware path disambiguation under `RenameNew`.
- **Registry Integration Tests**: Validating `SystemFileAssociations` registration and teardown.

---

## License

ReclaimArc is dual-licensed under either:
- **MIT License** ([LICENSE-MIT](LICENSE-MIT))
- **Apache License, Version 2.0** ([LICENSE-APACHE](LICENSE-APACHE))

Vendored UnRAR decoder components are licensed under the official UnRAR license (`unrar-ng-sys/vendor/unrar/license.txt`).