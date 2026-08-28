# ReclaimArc Recovery & Journaling Specification

ReclaimArc provides deterministic crash recovery across all phases of extraction, space planning, and in-place deallocation.

---

## 1. Storage of Recovery Journals

1. **Per-Job SQLite Journal**: Stored directly beside the archive in `<archive_dir>\.reclaimarc\<job-id>\job.db`.
   - SQLite Write-Ahead Logging (`PRAGMA journal_mode = WAL`).
   - Strict synchronization (`PRAGMA synchronous = FULL`).
   - Automatic WAL checkpoint truncation (`PRAGMA wal_checkpoint(TRUNCATE)`) to prevent journal bloat.
2. **Mirrored Global Registry**: Stored in `%LOCALAPPDATA%\ReclaimArc\registry.db` (overridable via `RECLAIMARC_APP_DATA` for isolated integration testing).

---

## 2. Information Recorded in the Journal

- **Job Metadata**: Target archive path, destination folder, archive fingerprint (file ID, volume serial, creation/modification times, byte length), extraction mode (`Normal` or `LowSpace`), and active configuration.
- **Volume & Source Identity**: Durable snapshots for every volume in multi-part sets.
- **Recovery Units**: Sequence order, entry spans, packed byte ranges, and linear state transitions (`PENDING` through `RECLAIMED`).
- **Entry Metadata**: Original relative paths, unpacked sizes, packed lengths, header CRC32, committed target paths, and **BLAKE3 cryptographic hashes**.
- **Packed Ranges**: Volume index, byte start offset, range length, and status (`ACTIVE`, `RECLAIM_INTENT`, `RECLAIMED`).
- **Telemetry & Diagnostics**: Recorded OS error codes, failed operations, and recommended recovery actions.

*Note: Passwords and encryption keys are strictly held in volatile memory only and are never persisted to the journal.*

---

## 3. Startup Discovery & Recovery Workflow

On application launch (both GUI and CLI):
1. **Discovery**: Scans `%LOCALAPPDATA%\ReclaimArc\registry.db` and local `.reclaimarc` folders beside archives for interrupted jobs.
2. **Recovery View**: Presents the recovery dashboard displaying committed output bytes, source space reclaimed, remaining source bytes, last checkpoint timestamp, and unit states.
3. **Resume Execution (`reclaimarc resume <archive>`)**:
   - **Source Identity Verification**: Re-validates the source archive against the stored fingerprint (volume ID, serial, size). If the archive was modified or replaced, extraction halts fail-closed.
   - **Adoption of Durable Outputs**: Files renamed to their final destination prior to a crash are verified against their stored BLAKE3 hash: if intact, they are adopted; if corrupted, they are re-extracted.
   - **Partial Staging Cleanup**: Stale `.sx-partial-*` temporary files are removed using retry-resilient deletion (`longpath::remove_file_existing`).
   - **Hole-Punch Reconciliation**: Ranges marked `RECLAIM_INTENT` are reconciled against physical disk allocation (`FSCTL_QUERY_ALLOCATED_RANGES`) and completed.
   - **Seamless Resumption**: Decompression resumes from the exact first uncommitted unit.
4. **Inspect (`reclaimarc diagnostics <archive>`)**: Generates an authoritative diagnostic report without modifying filesystem state.
5. **Abandon (`reclaimarc abandon <archive>`)**: Purges journal records and temporary staging files. The user is explicitly warned that previously deallocated source bytes cannot be restored.

---

## 4. Temporary Staging Isolation

Extracted output is initially written to an isolated staging file:
```
<final_name>.sx-partial-<job-id>[-.try-<nonce>]
```
- The file is closed and its contents read back to compute the BLAKE3 hash.
- `FlushFileBuffers` enforces physical storage synchronization.
- The file is atomically renamed to its final target path (`MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH`).
- Antivirus and search indexer transient locks are automatically resolved via exponential backoff retries.

---

## 5. Source Shell Removal & Finalization

When `delete_shells_on_completion` is enabled:
1. ReclaimArc asserts that 100% of all archive entries have been verified and committed.
2. The engine re-validates the physical identity of the source file to prevent deleting an unrelated file placed at the same path.
3. The hollowed source container file is removed from disk.