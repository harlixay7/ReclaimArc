# ReclaimArc — User Guide

ReclaimArc extracts archives when there is enough disk space for the final
files but not enough to hold the archive **and** the output at the same time.
It does this by converting verified archive bytes back into free space as it
goes.

## Quick start

1. **Open Archive** — choose a `.rar` file (any volume of a multipart set).
2. **Destination** — choose where the files go.
3. **Analyze** — the app shows the archive summary, every file with its
   recovery unit, and a space plan:
   - *Free now*
   - *Normal extraction requirement* (everything at once)
   - *Progressive peak requirement* (extra space needed beyond free space)
   - *Safety reserve*
   - *Largest recovery unit*
   - *Estimated source reclaim*
4. **Extract** — two choices:

   - **Normal Extraction** keeps the original archive. It is only enabled
     when there is enough space.
   - **Low-Space Extraction** progressively destroys verified portions of
     the source archive to reclaim space. It requires a confirmed warning
     before it starts and is only enabled when it is provably safe.

## During extraction

- Overall progress, current file, current recovery unit, written, verified,
  source reclaimed, current free space.
- **Pause** — safely aborts the current recovery unit (source for it stays
  intact).
- **Stop Safely** — same, immediately.
- **Cancel** — stops the job; it stays resumable. Previously reclaimed
  source data cannot be restored.

## After an interruption

On startup the app lists interrupted jobs. Select the archive, then:

- **Resume** — validates the source, reconciles everything, and continues
  from the last safe checkpoint.
- **Inspect** — shows committed output, source reclaimed, remaining source,
  last checkpoint, and recorded errors.
- **Abandon Job** — deletes the job (with a warning that reclaimed source
  cannot be restored).

## Settings

- **Safety preset** — Safe / Balanced / Maximum Space. Balanced is the
  default: full pre-test, immediate reclamation after each durable unit.
- **Existing files** — Overwrite / Skip / Rename new / Ask.
- **Pre-test archive** — full integrity test before destructive extraction.
- **Delete source shells on completion** — removes the (already reclaimed)
  archive after a successful Low-Space extraction.
- **Logging level** — controls the redacted log file.

## Command line

```
reclaimarc inspect <archive>
reclaimarc plan <archive> <destination>
reclaimarc extract <archive> <destination>            # normal
reclaimarc extract --low-space <archive> <destination> # destructive
reclaimarc jobs
reclaimarc resume <archive>
reclaimarc abandon <archive>
reclaimarc diagnostics <archive>
```

Options: `--password <pwd>`, `--mode safe|balanced|maximum-space`, `--yes`.

## Windows Batch Scripts

ReclaimArc provides two root batch scripts for quick setup and execution:

- **`setup.bat`**: Audits and installs all system dependencies (Rust, MSVC Build Tools, Node.js, npm, WebView2 runtime) idempotently.
- **`run.bat`**: Interactive launcher. Supports:
  * `run.bat` (launches Desktop GUI)
  * `run.bat --cli` (interactive CLI)
  * `run.bat --test` (runs automated test suite)
  * `run.bat --build` (builds release binaries and installers)
  * `run.bat --setup` (runs dependency setup)


## What ReclaimArc is honest about

- If the app says an extraction is safe, the engine can prove why (see
  SAFETY_MODEL.md).
- Solid archives may be reported NOT SAFE for progressive extraction — a
  solid chain must complete before its source can be reclaimed. The reason
  and the exact additional space required are shown.
- Rollback is never offered when the source no longer exists.
- The engine never drives the disk below its emergency reserve.
- Large-scale throughput is verified on physical drives: for example, successfully extracting a 55 GB archive (>55 GB output) on a volume with only 20 GB available free space (where a traditional extractor would require $\ge 55\text{ GB}$ of free space for the output) in ~10–12 minutes while keeping disk usage bounded.

## Limitations

- First-class target: RAR 4/5 on NTFS/ReFS. Other formats are not claimed.
- ZIP/7z/tar backends are future work behind the same interface.
- Encrypted archives are supported (password, memory-only) but encrypted
  fixtures are not synthesized by the test suite.