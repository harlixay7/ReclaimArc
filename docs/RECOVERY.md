# ReclaimArc — Recovery

## Where the journal lives

- Beside the archive: `<archive folder>\.reclaimarc\<job-id>\job.db`
  (SQLite, WAL, `synchronous=FULL`).
- Mirrored registry: `%LOCALAPPDATA%\ReclaimArc\registry.db`
  (overridable via `RECLAIMARC_APP_DATA`, used by tests).

## What is recorded

Job metadata (archive, destination, fingerprint, mode, settings), every
volume with a durable identity snapshot (volume serial, file id, size,
mtime), recovery units with their state, every entry with packed/unpacked
sizes, CRC32, final and partial paths and BLAKE3, every packed source range
with ACTIVE / RECLAIM_INTENT / RECLAIMED state, every state transition, and
every error with operation/path/OS error/recommended action.
Passwords are never stored.

## On startup

1. Interrupted jobs are discovered from the registry and by scanning
   `.reclaimarc` folders beside archives.
2. The recovery screen shows: committed output, source reclaimed, remaining
   source, last safe checkpoint, unit states, and recorded errors. It never
   says "rollback" unless the source actually exists.
3. **Resume** runs the full reconciliation:
   - source identity is re-validated (modified archives fail precisely);
   - renamed-but-uncommitted finals are verified against their stored BLAKE3
     and adopted, or deleted;
   - incomplete partial files are deleted;
   - RECLAIM_INTENT ranges are reconciled against the actual filesystem
     allocation and completed;
   - the job resumes from the first uncommitted unit.
4. **Inspect** shows the same recovery report without touching anything.
5. **Abandon** deletes the journal and registry entry. The confirmation
   explains that previously reclaimed source data cannot be restored.

## Partial files

Output is written as `<final name>.sx-partial-<job>[-.try-<nonce>]` and only
renamed to its final name after verification and durable flush. Each attempt
uses a fresh nonce because the unrar DLL leaves the output file of an aborted
extraction locked until the process exits; leftover attempts are cleaned on
the next process start.

## Exit states

A job is always either RESUMABLE or explicitly FAILED with a precise reason.
Failures include: CRC errors, disk-full risk (stopped before the reserve),
locked destinations, decoder errors, power loss, application crash and
filesystem operation failures. The engine never silently continues after a
failed integrity or reclamation operation.

## Diagnostics

`reclaimarc diagnostics <archive>` prints the full recovery report.
Structured logs live in `%LOCALAPPDATA%\ReclaimArc\logs\reclaimarc.log`
(redacted — passwords never appear).