# ReclaimArc Testing & Quality Assurance Specification

The ReclaimArc test suite provides mathematical and empirical verification of all safety invariants, fault boundaries, format decoders, and platform operations.

---

## 1. Running the Automated Test Suite

Execute the complete test suite across all workspace crates:

```cmd
cargo test --workspace --all-features
```

### Test Suite Structure (179 Automated Tests)

| Crate / Harness | Test Count | Scope & Coverage |
|---|---|---|
| **`reclaimarc-platform`** | 12 tests | Real NTFS sparse deallocation, `FSCTL_QUERY_ALLOCATED_RANGES`, interval arithmetic, AV lock retry loops, shell registry integration |
| **`reclaimarc-journal`** | 11 tests | SQLite WAL durability, linear unit state machines, `PRAGMA wal_checkpoint(TRUNCATE)`, monotonic reclaim metrics |
| **`reclaimarc-archive`** | 53 tests | RAR4/5 header parser, UnRAR FFI decoder, solid chains, multi-part spans, ZIP/ZIP64 dual-parser data descriptors, streaming engine |
| **`reclaimarc-core` (Unit)** | 24 tests | Predictive space planner, hostile path sanitation, emergency reserve calculation, ratio limits |
| **`reclaimarc-core` (Fault Injection)** | 66 tests | Process crash simulation across 9 transaction boundaries, mid-flight interruption, resume reconciliation |
| **`reclaimarc-core` (ZIP Engine)** | 5 tests | Complex ZIP directory topologies, zero-byte file handling, interleaved folder entries |
| **`reclaimarc-core` (ZIP Stress)** | 4 tests | Multi-gigabyte real-world ZIP and ZIP64 stress tests, mid-flight process termination, physical reclaim proofs |
| **`reclaimarc-core` (Case Collisions)**| 3 tests | Conflict-aware auto-disambiguation under `RenameNew` and fail-closed validation |
| **`reclaimarc-desktop-lib`** | 1 test | Desktop backend command dispatch, fallback destination resolution |

---

## 2. Fault-Injection Harness (`crates/core/tests/fault_injection.rs`)

The fault-injection harness simulates unexpected process termination at every transaction boundary:
1. Launches the engine in an isolated child process with `RECLAIMARC_FAULT_AT=<crash_point>`.
2. The child process self-terminates at the designated instruction with exit code 86.
3. The parent test runner asserts that:
   - The child process died at the exact requested transaction point.
   - Per-point safety invariants hold (uncommitted units are intact, committed units are adoptable).
   - `prepare_resume` successfully reconciles on-disk state against the journal Write-Ahead Log.
   - Resumed extraction completes without data corruption, producing 100% byte-identical output confirmed by BLAKE3 hashes.

### Instrumented Crash Boundaries
- `crash_after_partial_write`: Interruption during staged extraction before read-back verification.
- `crash_after_output_flush`: Interruption after `FlushFileBuffers` but before rename.
- `crash_after_rename`: Interruption after atomic rename but before journal commit.
- `crash_after_journal_commit`: Interruption after ACID journal commit but before physical deallocation.
- `crash_before_hole_punch`: Interruption when starting sparse deallocation.
- `crash_during_hole_punch`: Interruption mid-way through `FSCTL_SET_ZERO_DATA`.
- `crash_after_physical_hole_punch`: Interruption after cluster deallocation but before recording query metrics.
- `crash_before_reclaimed_commit`: Interruption prior to final linear transition commit.
- `crash_before_shell_deletion` / `crash_during_multipart_shell_deletion`: Interruption during finalization before or during source container cleanup.

---

## 3. Real-World ZIP & ZIP64 Stress Testing (`crates/core/tests/zip_stress_tests.rs`)

The stress testing harness exercises large-scale streaming decompression and physical deallocation on physical Windows drives:
- `test_real_zip64_large_scale_stress`: Generates multi-gigabyte ZIP64 archives with 64-bit offsets and validates streaming in-place reclamation.
- `test_real_zip_massive_multi_entry_stress_low_space`: Tests thousands of distinct file entries under constrained headroom simulations.
- `test_real_zip_large_payload_reclaim_stress`: Verifies physical cluster deallocation on large single-payload archives.
- `test_real_zip_stress_mid_flight_interruption_and_resume`: Simulates mid-extraction process termination on live ZIP streams and asserts clean resumption.

---

## 4. Hardware Verification & Large-Scale Benchmarks

ReclaimArc undergoes physical drive testing on NVMe and SATA SSDs:
- **Workload**: 55.0 GB RAR container with > 55.0 GB uncompressed payload.
- **Environment**: Volume with only 20.0 GB of initial available free space (traditional extractors demand $\ge 55\text{ GB}$ of free space).
- **Execution**: Completed in **10–12 minutes**.
- **Results**:
  - Sustained continuous throughput across UnRAR decompression, BLAKE3 read-back verification, and cluster deallocation.
  - Free space remained bounded above safety thresholds at all times.
  - 100% of the destination files were verified and committed without corruption.

---

## 5. Continuous Integration & MSRV Validation

The CI matrix on GitHub Actions validates:
1. **Minimum Supported Rust Version (MSRV)**: Rust 1.88.0 on `windows-latest`.
2. **Formatting & Lints**: `cargo fmt --check` and `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
3. **Full Test Execution**: `cargo test --workspace --all-features`.
4. **Desktop Compilation**: Verification of frontend React bundling and Tauri backend integration.
