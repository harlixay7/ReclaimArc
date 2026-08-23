# Security Policy — ReclaimArc

## Security Overview & Threat Model

ReclaimArc is destructive-by-design filesystem software: it reclaims disk space by progressively deallocating verified source archive sectors while extracting onto the same filesystem. Because an errant write or premature deallocation could cause irrecoverable data loss, ReclaimArc implements an uncompromising **fail-closed** security architecture:

1. **Zero Source Destruction Without Cryptographic Proof**:
   - Source clusters are deallocated via `FSCTL_SET_ZERO_DATA` only after destination files are decoded, integrity-validated against archive header checksums, durably flushed via `FlushFileBuffers`, atomically committed, and recorded in an ACID SQLite Write-Ahead Log.
   - Prior to unit extraction and deallocation, raw source bytes are cryptographically verified against journaled BLAKE3 manifests to prevent corrupted or tampered inputs from being reclaimed.

2. **Strict Fail-Closed Guarantees**:
   - If any anomaly, CRC32 mismatch, BLAKE3 digest discrepancy, filesystem error, or format ambiguity is encountered, execution terminates immediately with zero further deallocations.
   - Resuming an interrupted job verifies volume structural digests and uncommitted range manifests; modified sources or invalid journals abort immediately.

3. **Malicious Archive Mitigations**:
   - **Path Traversal**: Absolute paths, `..` directory traversal, and path normalization collisions are strictly rejected before files are written.
   - **Windows Device Names**: Reserved DOS/Windows device names (`CON`, `PRN`, `AUX`, `NUL`, `COM1..9`, `LPT1..9`) are sanitized and rejected.
   - **Alternate Data Streams (ADS)**: NTFS stream paths (`file:stream`) and trailing dots/spaces are rejected.
   - **Reparse Points & Symlinks**: Symbolic links and directory junctions are skipped by default to prevent link redirection and arbitrary target overwrite attacks.

4. **Credential & Password Safety**:
   - Passwords for encrypted archives are held strictly in memory and are never persisted in the journal, SQLite tables, error reports, or telemetry logs.

---

## Reporting a Vulnerability

If you discover a security vulnerability or potential data-safety bug in ReclaimArc, please report it privately:

- **Security Contact**: Open a private advisory on GitHub via the [Security Advisories](https://github.com/harlixay7/ReclaimArc/security/advisories) tab.
- **Response SLA**: We aim to acknowledge vulnerability reports within 48 hours and provide remediation timelines within 7 business days.
- **Public Disclosure**: Please do not file public issues for critical security or data-loss vulnerabilities until a patch and advisory have been released.
