# SpaceExtract — Safety Model

SpaceExtract converts disk allocation occupied by an archive into allocation
occupied by verified extracted files. This document states exactly when the
engine is allowed to destroy source bytes — and when it is not.

## The invariant

> SOURCE BYTES MAY ONLY BE RECLAIMED AFTER THE OUTPUT DEPENDING ON THEM IS:
> 1. completely decoded,
> 2. integrity-verified,
> 3. flushed durably to disk,
> 4. atomically committed,
> 5. recorded in the durable recovery journal.
>
> If safety cannot be proven, DO NOT reclaim the source.

The engine never infers reclaimability from the decoder's file pointer. Every
retirement is justified by an explicit `RetirementProof` produced by the
archive backend from format-level knowledge:

- a **non-solid** RAR file is independently decodable → its packed data range
  is one restartable unit;
- a **solid chain** (consecutive solid-flagged files) shares one dictionary →
  the entire chain is one unit;
- an archive with the **archive-level solid flag** is one unit, because the
  decoder decompresses (never seeks) skipped files in such archives, so no
  earlier byte may ever be missing;
- a file **split across volumes** spans one unit covering all its parts.

## Unit lifecycle

```
PENDING → EXTRACTING → OUTPUT_WRITTEN → OUTPUT_VERIFIED → OUTPUT_DURABLE
→ COMMITTED → RECLAIM_INTENT → RECLAIMED
```

Every transition is written to the SQLite journal (`synchronous=FULL`, WAL)
*before* the corresponding filesystem action:

| Transition | Journaled before | Filesystem action follows |
|---|---|---|
| EXTRACTING | unit marked extracting | decoder writes `<name>.sx-partial-<job>` |
| OUTPUT_WRITTEN | unit marked written | BLAKE3 + size verification |
| OUTPUT_VERIFIED | per-entry BLAKE3 stored | `FlushFileBuffers` on partials |
| OUTPUT_DURABLE | unit marked durable | atomic rename to final name |
| COMMITTED | per-entry commit + unit committed | directory flush |
| RECLAIM_INTENT | per-range intent persisted | `FSCTL_SET_ZERO_DATA` |
| RECLAIMED | measured allocation persisted | — |

## Crash windows

A crash between **any** two operations is recovered by `prepare_resume`:

- before COMMITTED: partial files are deleted and the unit re-extracted;
- after rename, before commit: the final file is verified against its stored
  BLAKE3 and adopted, or discarded and re-extracted;
- after commit, before reclaim: nothing to do — the unit is committed;
- after RECLAIM_INTENT, before RECLAIMED: the engine inspects the actual
  filesystem allocation, completes the punch, and records RECLAIMED;
- after reclaim: allocation queries reconcile the journal.

Rollback is never advertised after source data has been reclaimed.

## Space planning

Before anything is touched, the planner simulates the extraction unit by
unit:

```
available = free + source safely reclaimable from committed units
requirement = unit output + scratch + emergency reserve
```

If any unit's requirement exceeds the available space, extraction is reported
NOT SAFE with the exact deficit ("Additional space required: N bytes"). The
planner never guesses and never weakens the model to make a demo work.

The emergency reserve is `max(fixed minimum, 1% of filesystem, journal
requirement)`. The engine stops before consuming the reserve; the current
unit's source is intact, so its partial output can be discarded and retried.

## Integrity

Destructive extraction runs a full archive integrity test first (the
official decoder verifies every checksum). The archive fingerprint (volume
identities + sizes) is journaled; on resume the source is re-validated
against it and the job fails precisely if the archive was modified.

## Passwords

Passwords live in memory only, are never logged or journaled, are delivered
to the decoder through the wide-string callback, and are zeroed when the
decoder closes.

## File-system trust

Reclamation is disabled unless a behavioral probe proves the volume supports
sparse deallocation (sparse mark + zero-data + allocation query + byte
integrity). Destructive extraction is refused when the archive and
destination are on different volumes, because reclaiming source space cannot
increase capacity available to the destination there.