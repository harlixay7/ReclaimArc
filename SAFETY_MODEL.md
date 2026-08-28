# ReclaimArc Safety Model & Durability Specification

ReclaimArc converts physical disk allocation occupied by a compressed archive into allocation occupied by verified extracted output files. This document states the formal invariants governing when source bytes may be deallocated, how crash recovery is structured and executed, and how fail-closed boundaries are enforced.

---

## 1. The Core Safety Invariant

> **SOURCE BYTES MAY ONLY BE RECLAIMED AFTER THE OUTPUT DEPENDING ON THEM IS:**
> 1. Completely decoded without CRC errors,
> 2. Read back from physical storage and cryptographically verified (BLAKE3),
> 3. Flushed and synchronized durably to non-volatile storage (`FlushFileBuffers`),
> 4. Atomically committed into its final target path (`MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH`),
> 5. Recorded in the ACID SQLite Write-Ahead Log (WAL) journal.
>
> If safety cannot be proven for the next destructive operation, ReclaimArc stops immediately before reclaiming any source data. The current uncommitted unit remains fully recoverable; previously reclaimed source ranges are irreversible.

---

## 2. Recovery Unit Structures by Format

The engine never infers reclaimability from a decoder's file pointer. Every deallocation is governed by explicit **Recovery Units** constructed from structural metadata:

- **ZIP / ZIP64 (Stored & Deflate)**: Each entry is an independent recovery unit. Its exact packed byte span (local header + compressed payload + data descriptor) is retired only after that individual file commits.
- **Non-Solid RAR (RAR4 / RAR5)**: Each file constitutes an independent recovery unit.
- **Solid RAR Chains**: Files sharing a continuous solid compression dictionary form a single atomic recovery unit. Source bytes covering the entire solid run are deallocated only after all files in that run are committed.
- **Multi-Part RAR Volumes**: Spanned files across volume boundaries are tracked as atomic multi-part units.
- **Archive-Level Solid RAR**: The entire archive constitutes a single recovery unit. Progressive reclamation is not possible, and ReclaimArc reports standard extraction requirements.

---

## 3. Unit Lifecycle & Transactional Pipeline

```
PENDING -> EXTRACTING -> OUTPUT_WRITTEN -> OUTPUT_VERIFIED -> OUTPUT_DURABLE
        -> COMMITTED -> RECLAIM_INTENT -> RECLAIMED
```

Every transition is committed to the SQLite journal (`synchronous=FULL`, WAL) *before* the corresponding filesystem action is initiated:

| State Transition | Journal Precondition | Ensuing Filesystem Action |
|---|---|---|
| **EXTRACTING** | Unit marked active in journal | Decoder extracts entry to staged file `<name>.sx-partial-<job-id>` |
| **OUTPUT_WRITTEN** | Output length journaled | Staged file closed; read-back verification initiated |
| **OUTPUT_VERIFIED** | BLAKE3 cryptographic hash recorded | `FlushFileBuffers` enforces hardware disk synchronization |
| **OUTPUT_DURABLE** | Durability state marked | Atomic rename from staging to final output path |
| **COMMITTED** | Output commit marked in journal | Periodic directory handle flush |
| **RECLAIM_INTENT** | Physical byte range intent recorded | `FSCTL_SET_ZERO_DATA` issues sparse deallocation |
| **RECLAIMED** | Physical allocation query recorded | `FSCTL_QUERY_ALLOCATED_RANGES` confirms actual cluster release |

---

## 4. Crash Window Recovery Matrix

A crash or power loss between **any two operations** is deterministically resolved during restart via `prepare_resume`:

| Crash Point Window | State at Restart | Automatic Recovery Action |
|---|---|---|
| **Before COMMITTED** | Incomplete staging file | Staging file deleted via RAII/reconciliation; unit re-extracted from intact source |
| **After Rename, Before COMMITTED** | Final file on disk, not journaled | Engine verifies file against stored BLAKE3 hash: adopts if intact, re-extracts if mismatched |
| **After COMMITTED, Before Hole Punch** | Final file durable, source intact | Unit is already committed; engine proceeds directly to deallocation |
| **During / After Hole Punch, Before RECLAIMED** | Source partially deallocated | `FSCTL_QUERY_ALLOCATED_RANGES` queries actual physical allocation and reconciles journal state |
| **During Shell Removal on Completion** | Source zeroed, output committed | Re-verifies source file identity and volume size; safely removes hollowed shell |

---

## 5. Antivirus & Filter Driver Resilience

Background filter drivers (such as Windows Defender `WdFilter.sys` or search indexing services) frequently inspect newly closed `.sx-partial-*` files, causing transient Win32 file locks.

ReclaimArc handles this via exponential backoff in `longpath::rename_existing` and `longpath::remove_file_existing`:
- Catches `ERROR_SHARING_VIOLATION` (32), `ERROR_LOCK_VIOLATION` (33), and `ERROR_ACCESS_DENIED` (5).
- Retries across up to 7 backoff intervals (`0ms, 5ms, 15ms, 30ms, 60ms, 100ms, 140ms`).
- Non-transient errors fail immediately on attempt 1 without masking genuine filesystem failures.

---

## 6. Case Collision Protection on NTFS

Archives created on case-sensitive filesystems (such as Linux) may contain entries differing only by case (e.g. `file.txt` and `FILE.TXT`). On Windows NTFS (case-preserving, case-insensitive by default), extracting both directly would cause one to overwrite the other.

- Under `ConflictPolicy::RenameNew`: The engine detects case collisions before extraction and automatically disambiguates target paths (e.g. `file (case-collision-1).TXT`). Both entries extract, verify, and commit independently without data loss.
- Under `ConflictPolicy::Ask`, `Overwrite`, or `Skip`: The engine fails closed before modifying disk state, presenting an actionable error requiring explicit user configuration.

---

## 7. Predictive Space Planning & Emergency Reserve

Before any source byte is modified, the space planner simulates the extraction step by step:

$$\text{Available Space} = \text{Free Volume Space} + \text{Reclaimable Source Bytes from Committed Units}$$
$$\text{Required Space} = \text{Unit Output Size} + \text{Staging Scratch Space} + \text{Emergency Reserve}$$

The **Emergency Reserve** is defined as $\max(\text{Fixed Reserve}, 1\%\text{ of Volume Capacity}, \text{Journal Space})$. If at any point the required space exceeds available space, extraction is refused with the exact byte deficit. The engine never allows disk space to drop below the emergency reserve.
