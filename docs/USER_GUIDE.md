# ReclaimArc User Guide

ReclaimArc is an archive extraction tool engineered for Windows systems operating under low disk space conditions. It extracts files progressively and deallocates verified disk sectors from the source archive in real time, enabling successful extraction when there is insufficient free space to store both the compressed container and the uncompressed output simultaneously.

---

## 1. Quick Start Workflows

### Workflow A: Windows Explorer Context Menu (Fastest)
1. In Windows File Explorer, locate any supported archive (`.rar`, `.zip`, `.7z`, `.tar`, `.gz`, etc.).
2. Right-click the archive and select **"Analyze & Extract with ReclaimArc"**.
3. The ReclaimArc desktop application launches, automatically pre-fills the archive and parent destination directory, and runs the space feasibility analysis.
4. Review the space feasibility summary:
   - **Total Uncompressed Size**
   - **Available Free Disk Space**
   - **Feasibility Recommendation** (Normal or Low-Space Progressive Extraction)
5. Click **Start Extraction** (or **Low-Space Extraction**).

### Workflow B: Desktop GUI Application
1. Open `ReclaimArc.exe`.
2. Click **Browse Archive** to select a `.rar` (including multipart volumes like `.part01.rar`) or `.zip` / `.zip64` file.
3. Click **Browse Destination** to select your target extraction folder (or leave blank to extract into the archive's folder).
4. Enter an archive password if the container is encrypted.
5. Click **Analyze Archive** to generate a real-time space simulation plan.
6. Select your preferred extraction mode:
   - **Normal Extraction**: Extracts all files while keeping the source archive completely intact. Available whenever free disk space exceeds total uncompressed output.
   - **Low-Space Extraction**: Progressively deallocates physical source sectors as each file is verified and committed. Available whenever the largest recovery unit fits within available disk headroom.

---

## 2. Real-Time Monitoring & Job Control

During extraction, the interface provides live metrics:
- **Written Output**: Total bytes written to staging.
- **Verified Bytes**: Total bytes confirmed via read-back length and BLAKE3 cryptographic hashes.
- **Source Reclaimed**: Exact physical disk allocation returned to the volume free pool (measured via `FSCTL_QUERY_ALLOCATED_RANGES`).
- **Available Free Space**: Authoritative volume headroom.

### Control Actions
- **Pause**: Suspends extraction at the next recovery unit boundary. The current unit completes cleanly, leaving all source data intact.
- **Stop Safely**: Requests immediate graceful termination. Staged partial files for the active unit are deleted, and journal state remains cleanly resumable.
- **Cancel**: Terminates the active job. All committed output files are preserved, and the job can be resumed at any time.

---

## 3. Crash Recovery & Resuming Interrupted Extractions

ReclaimArc protects against system crashes, power failures, and accidental termination using an ACID SQLite Write-Ahead Log (WAL) journal.

### Resuming an Interrupted Job
1. When launched, ReclaimArc automatically scans for existing journals beside archives in `.reclaimarc/<job-id>/job.db`.
2. Interrupted jobs are presented on the recovery view:
   - **Resume**: Re-verifies source identity, reconciles on-disk files against the journal, purges stale partial files, and resumes extraction from the last committed checkpoint.
   - **Inspect**: Displays committed output size, physical source space reclaimed, remaining source bytes, checkpoint timestamp, and recorded diagnostics.
   - **Abandon Job**: Removes journal files and purges remaining temporary state.

---

## 4. Application Settings & Configuration

Access the **Settings** dialog in the desktop application to configure engine policies:

| Setting | Options | Default | Description |
|---|---|---|---|
| **Safety Preset** | Safe / Balanced / Maximum Space | Balanced | Balanced performs full archive pre-testing and immediate per-unit physical reclamation. |
| **Existing Files (Conflict Policy)** | Overwrite / Skip / Rename New / Ask | Ask | Controls behavior when target files exist on disk. `Rename New` auto-disambiguates case-colliding archive paths on NTFS. |
| **Pre-Test Integrity** | Enabled / Disabled | Enabled | Verifies complete archive checksums prior to starting destructive low-space extraction. |
| **Delete Source Shells on Completion** | Enabled / Disabled | Disabled | Deletes the zero-byte hollowed source container file after 100% verified extraction. |
| **Windows Explorer Integration** | Enabled / Disabled | Enabled | Registers the right-click "Analyze & Extract with ReclaimArc" context menu in `HKCU` (0 admin privileges). |
| **Logging Level** | Error / Warn / Info / Debug | Info | Controls verbosity of the redacted diagnostic log in `%LOCALAPPDATA%\ReclaimArc\logs\reclaimarc.log`. |

---

## 5. Command-Line Interface (CLI)

ReclaimArc provides a full-featured CLI binary (`reclaimarc.exe`) for automation, scripting, and headless environments.

### Command Reference

```cmd
# Inspect archive metadata, compression formats, and recovery units
reclaimarc.exe inspect "C:\Path\To\Archive.rar"

# Simulate and evaluate space feasibility for a target destination
reclaimarc.exe plan "C:\Path\To\Archive.rar" "D:\Extracted"

# Normal extraction (preserves source archive)
reclaimarc.exe extract "C:\Path\To\Archive.rar" "D:\Extracted"

# Low-space progressive extraction (reclaims source sectors in-place)
reclaimarc.exe extract "C:\Path\To\Archive.rar" "D:\Extracted" --low-space --yes

# With password and custom safety preset
reclaimarc.exe extract "C:\Path\To\Archive.rar" "D:\Extracted" --password "secret" --mode safe

# List all tracked and interrupted jobs
reclaimarc.exe jobs

# Resume an interrupted extraction job
reclaimarc.exe resume "C:\Path\To\Archive.rar"

# Display low-level journal diagnostics
reclaimarc.exe diagnostics "C:\Path\To\Archive.rar"

# Abandon and purge an interrupted job
reclaimarc.exe abandon "C:\Path\To\Archive.rar"
```

---

## 6. Windows Batch Scripts

The root repository includes automated batch scripts for quick setup and execution:

- **`setup.bat`**: Audits the host environment and installs prerequisites idempotently (Rust toolchain, MSVC Build Tools, Node.js, npm, WebView2 runtime).
- **`run.bat`**: Interactive launcher with flag support:
  - `run.bat`: Launches the Desktop GUI application.
  - `run.bat --cli`: Launches the interactive command-line interface.
  - `run.bat --test`: Executes the full workspace test suite (175 tests).
  - `run.bat --build`: Compiles optimized release binaries and installers.
  - `run.bat --setup`: Runs dependency verification.

---

## 7. Operational Invariants & Limitations

- **Filesystem Support**: In-place sparse hole punching requires Windows NTFS or ReFS on volumes supporting sparse files.
- **Volume Locality**: Progressive low-space extraction requires the source archive and target destination to reside on the same filesystem volume.
- **In-Place Source Modification**: Low-Space Extraction deallocates physical disk sectors from the source container file as output files commit. Reclaimed source sectors cannot be rolled back once zeroed; users should maintain external backups of irreplaceable data.
- **Reparse Points & Symlinks**: Under default safety policies, archive redirection entries (symlinks, directory junctions) are skipped to prevent directory traversal and symlink-based attacks.
- **Solid Archives**: In solid RAR archives, files within a continuous dictionary chain depend on preceding entries. ReclaimArc deallocates source sectors only after the entire solid chain has been verified and committed.