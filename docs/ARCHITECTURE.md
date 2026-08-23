# ReclaimArc — Architecture

```
┌─────────────────────────────── apps ───────────────────────────────┐
│  apps/cli         reclaimarc  (inspect / plan / extract / resume) │
│  apps/desktop     Tauri 2 + React + strict TypeScript              │
└───────────────────────────────▲───────────────────────────────────┘
                                 │ events / commands
┌─────────────────────────── crates/core ────────────────────────────┐
│  engine     transactional per-unit lifecycle, pause/stop/cancel    │
│  planner    recovery-unit space simulation + feasibility verdicts  │
│  safety     emergency reserve, capacity gates, space monitor       │
│  paths      hostile-name validation (traversal, ADS, collisions)   │
│  recovery   discovery, identity validation, reconciliation, resume │
│  state      PENDING→…→RECLAIMED linear state machine               │
│  fault      crash-point injection for the test harness             │
└───────────────▲──────────────────┬───────────────────┬─────────────┘
                 │                  │                   │
      ┌──────────┴───────┐  ┌──────┴────────┐  ┌───────┴──────────┐
      │ crates/archive   │  │ crates/journal│  │ crates/platform  │
      │ ArchiveBackend   │  │ SQLite, WAL,  │  │ Windows: sparse  │
      │ RAR parser       │  │ synchronous=  │  │ FSCTL_SET_ZERO_  │
      │ unrar_sys FFI    │  │ FULL, schema  │  │ DATA, allocation │
      │ (official UnRAR) │  │ registry      │  │ queries, flushes │
      └──────────────────┘  └──────────────┘  └──────────────────┘
```

## The core does not depend on the UI

`crates/core` exposes the engine; both the CLI and the desktop app are thin
drivers over it. The CLI and GUI use the same tested engine.

## Archive backends

`ArchiveBackend` (crates/archive) is the only way the engine talks to an
archive:

```
inspect()  test_integrity()  entries()  recovery_units()  extract_unit()
cancel()   decoder_requirements()  retirement_proofs()
```

The RAR backend (v1 target) uses the **official UnRAR source** (vendored by
`unrar_sys`, RARLab license kept at `unrar_sys/vendor/unrar/license.txt`) for
all decoding, plus our own header parser for exact packed ranges, solid
chains and split-file parts. The parser is cross-validated against the C
library's own header stream during inspection — a mismatch fails the job
rather than guessing.

- `RAR_TEST` verifies checksums (integrity pre-test);
- `RAR_EXTRACT` writes each file to the engine-validated partial path
  (the DLL's `dest_name` contract: the caller guarantees path safety);
- `RAR_SKIP` seeks past committed units in non-solid archives, so reclaimed
  (zeroed) regions are never read.

Future formats (ZIP, 7z, tar, zstd/tar.zst) plug in behind the same trait;
the capability matrix is per-format and honest about what progressive
reclamation supports.

## Journal

Per-job SQLite database beside the archive (`.reclaimarc/<job>/job.db`),
WAL mode, `synchronous=FULL` for trust-critical commits, plus a mirrored
registry in `%LOCALAPPDATA%\ReclaimArc`. Every durable transition is
committed before the corresponding filesystem action.

## Platform

Windows-first (NTFS/ReFS): behavioral capability probe, `FSCTL_SET_SPARSE`,
`FSCTL_SET_ZERO_DATA` (aligned inward to the 64 KiB NTFS deallocation unit —
verified empirically; `GetCompressedFileSizeW`/`FILE_STANDARD_INFO` are
unreliable for sparse files and are not used), `FSCTL_QUERY_ALLOCATED_RANGES`
for authoritative allocation measurement, `FlushFileBuffers` on files and
directories (directory flushes require `FILE_WRITE_DATA` on the handle),
atomic renames with `MOVEFILE_REPLACE_EXISTING|MOVEFILE_WRITE_THROUGH`.
Linux hole-punching slots in behind the same trait.

## RAR header parsing

The parser (format structure only — no compression) computes per-volume
packed data ranges from RAR4/RAR5 headers, tracks split files across
volumes, and builds recovery units:

- non-solid file → one unit;
- maximal run of solid files → one unit (dictionary chain);
- archive-level solid flag → whole archive is one unit;
- split file → one unit across all its parts.