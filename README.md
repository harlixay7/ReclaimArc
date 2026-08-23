# SpaceExtract

Extract archives when there is enough disk capacity for the final files, but
not enough free space to keep the full archive and the full output at the
same time.

SpaceExtract is a transactional storage engine: it progressively converts
disk allocation occupied by an archive into allocation occupied by verified
extracted files — **source bytes are only reclaimed after their output is
decoded, verified, flushed durably, atomically committed and recorded in the
durable recovery journal.** If safety cannot be proven, the source is never
reclaimed.

## Highlights

- Windows-first (NTFS/ReFS), Rust engine + Tauri 2 desktop + CLI on the
  **same engine**.
- RAR 4/5 support via the official UnRAR library (license boundary kept)
  with exact packed-range analysis: non-solid files, solid chains, split
  files, multipart volumes.
- Crash-safe by construction: a fault-injection harness kills the process at
  every durable transition and proves resume produces byte-identical output.
- Truthful space planning: if SpaceExtract says an extraction is safe, the
  engine can prove why.
- Hostile archive paths are rejected (traversal, ADS, device names,
  case collisions). Passwords are memory-only.

## Layout

```
crates/core       engine, planner, safety, path security, recovery
crates/archive    ArchiveBackend + RAR parser + official UnRAR adapter
crates/journal    durable SQLite journal + app-data registry
crates/platform   Windows sparse reclamation, allocation queries, flushes
apps/cli          spacextract (inspect / plan / extract / resume / …)
apps/desktop      Tauri 2 + React + strict TypeScript
```

## Build & test

```
cargo test --workspace        # 66 tests incl. crash-at-every-transition
cargo build --release -p spacextract-cli
cd apps/desktop && npm run tauri build   # MSI + NSIS installers
```

## Documentation

- `docs/SAFETY_MODEL.md` — the invariant and the unit lifecycle
- `docs/RECOVERY.md` — journals, reconciliation, resume semantics
- `docs/ARCHITECTURE.md` — crate layout and backend design
- `docs/TESTING.md` — fault injection, integration and property tests
- `docs/USER_GUIDE.md` — how to use the app and CLI

## License

The SpaceExtract code is MIT OR Apache-2.0. The vendored UnRAR source keeps
its own license (`unrar_sys/vendor/unrar/license.txt`) and is never
re-licensed.