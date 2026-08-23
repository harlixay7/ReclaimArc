import { useCallback, useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  abandonJob,
  analyze,
  cancelJob,
  currentJob,
  formatBytes,
  getSettings,
  listJobs,
  openFolder,
  openLogsDir,
  pauseJob,
  pickArchive,
  pickDirectory,
  ratio,
  readLogs,
  recoveryView,
  resumeExtraction,
  setSettings,
  startExtraction,
  stopJob,
  type AnalyzeResult,
  type JobListEntry,
  type RecoveryView,
  type SettingsDto,
  type SxEvent,
} from "./api";

type View = "home" | "extracting" | "recovery";

interface ProgressState {
  jobId: string;
  currentUnit: number | null;
  currentEntry: string;
  entryCurrent: number;
  entryTotal: number;
  unitBytes: number;
  writtenBytes: number;
  verifiedBytes: number;
  reclaimedBytes: number;
  freeSpace: number | null;
  preTest: { current: number; total: number } | null;
  preTestOk: boolean | null;
  finished: boolean;
  paused: boolean;
  cancelled: boolean;
  error: string | null;
}

const emptyProgress = (): ProgressState => ({
  jobId: "",
  currentUnit: null,
  currentEntry: "",
  entryCurrent: 0,
  entryTotal: 0,
  unitBytes: 0,
  writtenBytes: 0,
  verifiedBytes: 0,
  reclaimedBytes: 0,
  freeSpace: null,
  preTest: null,
  preTestOk: null,
  finished: false,
  paused: false,
  cancelled: false,
  error: null,
});

/// O(1) recovery-unit lookup for the file table. For archives with tens of
/// thousands of entries a per-row linear search would freeze the UI.
function unitForIndex(analysis: AnalyzeResult, index: number): number | null {
  const units = analysis.info.recovery_units;
  // Binary search over units (sorted by seq/first_entry).
  let lo = 0;
  let hi = units.length - 1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    const u = units[mid]!;
    if (index < u.first_entry) {
      hi = mid - 1;
    } else if (index > u.last_entry) {
      lo = mid + 1;
    } else {
      return u.seq;
    }
  }
  return null;
}

function getParentDirectory(filePath: string): string {
  if (!filePath) return "";
  const lastSlash = Math.max(filePath.lastIndexOf("/"), filePath.lastIndexOf("\\"));
  if (lastSlash > 0) {
    return filePath.slice(0, lastSlash);
  }
  return "";
}

export default function App() {
  const [view, setView] = useState<View>("home");
  const [archive, setArchive] = useState("");
  const [destination, setDestination] = useState("");
  const [password, setPassword] = useState("");
  const [analyzing, setAnalyzing] = useState(false);
  const [analysis, setAnalysis] = useState<AnalyzeResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [progress, setProgress] = useState<ProgressState>(emptyProgress());
  const [recovery, setRecovery] = useState<RecoveryView | null>(null);
  const [interrupted, setInterrupted] = useState<JobListEntry[]>([]);
  const [showSettings, setShowSettings] = useState(false);
  const [showLogs, setShowLogs] = useState(false);
  const [showExtractChoice, setShowExtractChoice] = useState(false);
  const [pendingChoice, setPendingChoice] = useState<"normal" | "low" | null>(null);
  const [running, setRunning] = useState(false);
  const destRef = useRef("");

  const refreshInterrupted = useCallback(async () => {
    try {
      const jobs = await listJobs();
      setInterrupted(jobs);
    } catch {
      /* not fatal */
    }
  }, []);

  useEffect(() => {
    refreshInterrupted();
    const unlisten = listen<SxEvent>("sx://event", (e) => {
      const ev = e.payload;
      switch (ev.type) {
        case "job-started":
          setProgress((p) => ({
            ...p,
            jobId: String(ev.job_id || ev.payload || p.jobId),
            finished: false,
            paused: false,
            cancelled: false,
            error: null,
          }));
          break;
        case "pre-test-started":
          setProgress((p) => ({
            ...p,
            preTest: { current: 0, total: Number(ev.total) || 1 },
          }));
          break;
        case "pre-test-progress":
          setProgress((p) => ({
            ...p,
            preTest: {
              current: Number(ev.current),
              total: Number(ev.total) || (p.preTest ? p.preTest.total : 1),
            },
          }));
          break;
        case "pre-test-finished":
          setProgress((p) => ({
            ...p,
            preTest: null,
            preTestOk: Boolean(ev.ok),
          }));
          break;
        case "unit-started":
          setProgress((p) => ({
            ...p,
            currentUnit: Number(ev.seq),
            currentEntry: "",
            entryCurrent: 0,
            entryTotal: 0,
          }));
          break;
        case "entry-started":
          setProgress((p) => ({
            ...p,
            currentEntry: String(ev.name),
            entryCurrent: 0,
          }));
          break;
        case "entry-progress":
          setProgress((p) => ({
            ...p,
            entryCurrent: Number(ev.current),
            entryTotal: Number(ev.total),
          }));
          break;
        case "entry-committed":
          setProgress((p) => ({
            ...p,
            writtenBytes: p.writtenBytes + (p.entryTotal || p.entryCurrent || 0),
          }));
          break;
        case "unit-committed":
          setProgress((p) => ({
            ...p,
            verifiedBytes: p.verifiedBytes + Number(ev.bytes),
          }));
          break;
        case "unit-reclaimed":
          setProgress((p) => ({
            ...p,
            reclaimedBytes: p.reclaimedBytes + Number(ev.bytes),
          }));
          break;
        case "range-reclaimed":
          setProgress((p) => ({
            ...p,
            reclaimedBytes: p.reclaimedBytes + Number(ev.bytes),
          }));
          break;
        case "free-space":
          setProgress((p) => ({
            ...p,
            freeSpace: Number(ev.bytes),
          }));
          break;
        case "job-paused":
          setProgress((p) => ({ ...p, finished: true, paused: true }));
          setRunning(false);
          refreshInterrupted();
          break;
        case "job-cancelled":
          setProgress((p) => ({ ...p, finished: true, cancelled: true }));
          setRunning(false);
          refreshInterrupted();
          break;
        case "job-finished":
          setProgress((p) => ({
            ...p,
            finished: true,
            writtenBytes: Number(ev.committed) || p.writtenBytes,
            verifiedBytes: Number(ev.committed) || p.verifiedBytes,
            reclaimedBytes: Number(ev.reclaimed) || p.reclaimedBytes,
          }));
          setRunning(false);
          refreshInterrupted();
          break;
        case "job-failed":
          setProgress((p) => ({
            ...p,
            finished: true,
            error: String(ev.message || ev.recommended || "Extraction failed"),
          }));
          setRunning(false);
          refreshInterrupted();
          break;
        default:
          break;
      }
    });
    return () => {
      void unlisten.then((f) => f());
    };
  }, [refreshInterrupted]);

  useEffect(() => {
    currentJob().then((id) => setRunning(!!id));
  }, [view]);

  const doAnalyze = async () => {
    if (!archive) return;
    setAnalyzing(true);
    setError(null);
    const targetDest = destination.trim() || getParentDirectory(archive);
    try {
      const result = await analyze(archive, targetDest, password || undefined);
      setAnalysis(result);
      if (!destination.trim() && targetDest) {
        setDestination(targetDest);
      }
      destRef.current = targetDest;
      setView("home");
    } catch (e) {
      setError(String(e));
      setView("home");
    } finally {
      setAnalyzing(false);
    }
  };

  const doExtract = async (lowSpace: boolean) => {
    if (!archive) return;
    setError(null);
    setProgress(emptyProgress());
    try {
      const targetDest = destRef.current || destination.trim() || getParentDirectory(archive);
      const id = await startExtraction(
        archive,
        targetDest,
        lowSpace,
        password || undefined,
      );
      setProgress((p) => ({ ...p, jobId: id }));
      setRunning(true);
      setView("extracting");
    } catch (e) {
      setError(String(e));
    }
  };

  const doResume = async () => {
    if (!archive) return;
    setError(null);
    let initialWritten = 0;
    let initialReclaimed = 0;
    try {
      const r = recovery || (await recoveryView(archive));
      if (r) {
        initialWritten = r.committed_output_bytes;
        initialReclaimed = r.source_reclaimed_bytes;
      }
    } catch {
      // not fatal
    }
    setProgress({
      ...emptyProgress(),
      writtenBytes: initialWritten,
      verifiedBytes: initialWritten,
      reclaimedBytes: initialReclaimed,
    });
    try {
      const id = await resumeExtraction(archive);
      setProgress((p) => ({ ...p, jobId: id }));
      setRunning(true);
      setView("extracting");
    } catch (e) {
      setError(String(e));
    }
  };

  const doAbandon = async () => {
    if (!archive) return;
    try {
      await abandonJob(archive);
      setRecovery(null);
      await refreshInterrupted();
      setView("home");
    } catch (e) {
      setError(String(e));
    }
  };

  const inspectRecovery = async () => {
    if (!archive) return;
    try {
      const r = await recoveryView(archive);
      setRecovery(r);
      setView("recovery");
    } catch (e) {
      setError(String(e));
    }
  };

  const selectInterruptedJob = async (j: JobListEntry) => {
    setArchive(j.archive);
    setDestination(j.destination);
    destRef.current = j.destination;
    try {
      const r = await recoveryView(j.archive);
      setRecovery(r);
      setView("recovery");
    } catch {
      setRecovery(null);
    }
  };

  const openArchive = async () => {
    const p = await pickArchive();
    if (p) {
      setArchive(p);
      setAnalysis(null);
      setError(null);
      const parentDir = getParentDirectory(p);
      if (!destination.trim() && parentDir) {
        setDestination(parentDir);
        destRef.current = parentDir;
      }
      // Auto-probe recovery for this archive.
      try {
        const r = await recoveryView(p);
        setRecovery(r);
        setView("recovery");
      } catch {
        setRecovery(null);
        setView("home");
      }
    }
  };

  const openDestination = async () => {
    const p = await pickDirectory();
    if (p) {
      setDestination(p);
      destRef.current = p;
    }
  };

  const [filterText, setFilterText] = useState("");
  const [hasPassword, setHasPassword] = useState(false);

  const filteredEntries = analysis
    ? analysis.info.entries.filter((e) =>
        e.name.toLowerCase().includes(filterText.toLowerCase()),
      )
    : [];

  const effectiveDest = destination.trim() || (archive ? getParentDirectory(archive) : "");
  const archiveFileName = archive ? archive.split(/[/\\]/).pop() || archive : "";

  return (
    <>
      <div className="command-bar">
        <span className="title">ReclaimArc</span>
        <button
          className={view === "home" ? "tab-active" : ""}
          onClick={() => setView("home")}
        >
          Extract
        </button>
        {interrupted.length > 0 && (
          <button
            className={view === "recovery" ? "tab-active" : ""}
            onClick={() => setView("recovery")}
          >
            Recovery <span className="badge">{interrupted.length}</span>
          </button>
        )}
        <button onClick={openArchive}>Open Archive…</button>
        <button onClick={openDestination}>Destination…</button>
        <button className="primary" onClick={doAnalyze} disabled={!archive || analyzing}>
          {analyzing ? "Analyzing…" : "Analyze"}
        </button>
        <div className="spacer" />
        <button onClick={() => setShowLogs(true)}>Logs</button>
        <button onClick={() => setShowSettings(true)}>Settings</button>
      </div>

      {error && <div className="error-banner">{error}</div>}

      {view === "home" && (
        <div className="layout">
          {/* Step 1: Select Archive */}
          <div className={`step-card ${!archive ? "active" : ""}`}>
            <div className="card-header">
              <div className="card-title">
                <span className="step-badge">1</span>
                <span>Select Archive File</span>
              </div>
              <span className="tag">RAR4 · RAR5 · Solid · Multi-part</span>
            </div>
            <div className="card-subtitle">
              Choose the RAR or compressed archive you want to inspect and extract.
            </div>

            {archive ? (
              <div className="file-selected-chip">
                <div className="file-info">
                  <span className="file-name">📦 {archiveFileName}</span>
                  <span className="file-path">{archive}</span>
                </div>
                <div style={{ display: "flex", gap: 6 }}>
                  <button onClick={openArchive}>Change…</button>
                  <button
                    onClick={() => {
                      setArchive("");
                      setAnalysis(null);
                    }}
                  >
                    Clear
                  </button>
                </div>
              </div>
            ) : (
              <div className="dropzone-box" onClick={openArchive}>
                <div className="icon">📦</div>
                <div className="primary-text">Click to browse archive (.rar, .zip, .7z)</div>
                <div className="secondary-text">
                  Or select multi-part sets (.part1.rar, .r00)
                </div>
              </div>
            )}

            <div style={{ marginTop: 8 }}>
              <div className="path-field">
                <input
                  type="text"
                  value={archive}
                  placeholder="Or enter/paste full archive path here (e.g. C:\Downloads\archive.rar)"
                  onChange={(e) => {
                    setArchive(e.target.value);
                    setAnalysis(null);
                  }}
                />
                <button onClick={openArchive}>Browse…</button>
              </div>
            </div>
          </div>

          {/* Step 2: Choose Destination */}
          <div className="step-card">
            <div className="card-header">
              <div className="card-title">
                <span className="step-badge">2</span>
                <span>Extraction Destination</span>
              </div>
              <span className="tag">Destination Folder</span>
            </div>
            <div className="card-subtitle">
              Where the unpacked files will be saved. Defaults to the archive's folder.
            </div>

            <div className="path-field">
              <input
                type="text"
                value={effectiveDest}
                placeholder="Choose extraction destination folder"
                onChange={(e) => setDestination(e.target.value)}
              />
              <button onClick={openDestination}>Browse Folder…</button>
            </div>

            <div style={{ marginTop: 10, display: "flex", alignItems: "center", gap: 12 }}>
              <label style={{ display: "flex", alignItems: "center", gap: 6, cursor: "pointer", fontSize: 12.5 }}>
                <input
                  type="checkbox"
                  checked={hasPassword}
                  onChange={(e) => setHasPassword(e.target.checked)}
                />
                <span>Archive is password-protected</span>
              </label>
            </div>

            {hasPassword && (
              <div style={{ marginTop: 8 }}>
                <input
                  type="password"
                  value={password}
                  placeholder="Enter archive password (held in memory only, never saved to disk)"
                  onChange={(e) => setPassword(e.target.value)}
                />
              </div>
            )}
          </div>

          {/* Step 3: Analyze & Verification Action */}
          {!analysis && (
            <div className="action-card">
              <div className="action-desc">
                <strong>Step 3: Analyze & Check Safety</strong>
                <div style={{ marginTop: 4, color: "var(--text-dim)", fontSize: 12 }}>
                  Inspects compression structure, calculates recovery units, and checks if your destination volume has enough space for normal or low-space progressive extraction.
                </div>
              </div>
              <button
                className="primary"
                style={{ minWidth: 160, padding: "8px 20px", fontSize: 13.5, fontWeight: 600 }}
                onClick={doAnalyze}
                disabled={!archive || analyzing}
              >
                {analyzing ? "Analyzing Archive…" : "🔍 Analyze Archive"}
              </button>
            </div>
          )}

          {/* Analysis Dashboard */}
          {analysis && (
            <>
              <div className="panel">
                <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", marginBottom: 8 }}>
                  <h2>Archive Summary</h2>
                  <button onClick={doAnalyze} disabled={analyzing}>
                    {analyzing ? "Re-analyzing…" : "Re-analyze"}
                  </button>
                </div>
                <div className="summary">
                  <span className="strong">{analysis.info.format.toUpperCase()}</span>
                  {" · "}
                  {formatBytes(analysis.info.packed_size)} packed ·{" "}
                  {formatBytes(analysis.info.unpacked_size)} unpacked ·{" "}
                  {analysis.info.solid_archive ? "Solid Archive" : "Non-solid"}
                  {analysis.info.encrypted_headers && " · Encrypted headers"}
                  {analysis.info.volumes.length > 1 && ` · ${analysis.info.volumes.length} volumes`}
                  {` · ${analysis.info.entries.length} items`}
                </div>

                <div className="search-bar">
                  <input
                    type="text"
                    value={filterText}
                    placeholder="Search / filter files in archive by name..."
                    onChange={(e) => setFilterText(e.target.value)}
                  />
                  {filterText && (
                    <button onClick={() => setFilterText("")}>Clear Filter</button>
                  )}
                  <span style={{ fontSize: 12, color: "var(--text-dim)", whiteSpace: "nowrap" }}>
                    Showing {filteredEntries.length} of {analysis.info.entries.length}
                  </span>
                </div>

                <div style={{ maxHeight: 240, overflow: "auto" }}>
                  <table>
                    <thead>
                      <tr>
                        <th>Name</th>
                        <th className="num">Packed</th>
                        <th className="num">Size</th>
                        <th className="num">Ratio</th>
                        <th className="num">Unit</th>
                        <th>Status</th>
                      </tr>
                    </thead>
                    <tbody>
                      {filteredEntries.map((e) => {
                        const unit = unitForIndex(analysis, e.index);
                        return (
                          <tr key={e.index}>
                            <td>{e.name}</td>
                            <td className="num">{formatBytes(e.packed_size)}</td>
                            <td className="num">{formatBytes(e.unpacked_size)}</td>
                            <td className="num">{ratio(e.packed_size, e.unpacked_size)}</td>
                            <td className="num">{unit ?? "—"}</td>
                            <td>
                              {e.is_directory ? (
                                <span className="status pending">dir</span>
                              ) : e.is_solid ? (
                                <span className="status running">solid</span>
                              ) : (
                                <span className="status pending">file</span>
                              )}
                            </td>
                          </tr>
                        );
                      })}
                      {filteredEntries.length === 0 && (
                        <tr>
                          <td colSpan={6} style={{ textAlign: "center", color: "var(--text-dim)", padding: 16 }}>
                            No entries matching "{filterText}"
                          </td>
                        </tr>
                      )}
                    </tbody>
                  </table>
                </div>
              </div>

              <div className="panel">
                <h2>Space Plan & Feasibility</h2>
                <div className="plan-grid">
                  <span className="label">Free Disk Space Now</span>
                  <span className="value">{formatBytes(analysis.plan.free_now)}</span>
                  <span />
                  <span className="label">Normal Extraction Needed</span>
                  <span className="value">{formatBytes(analysis.plan.unpacked_total)}</span>
                  <span />
                  <span className="label">Progressive Peak Requirement</span>
                  <span className="value">
                    {analysis.plan.progressive_peak_requirement === 0
                      ? "Fits without reclamation"
                      : formatBytes(analysis.plan.progressive_peak_requirement)}
                  </span>
                  <span />
                  <span className="label">Safety Reserve</span>
                  <span className="value">{formatBytes(analysis.plan.reserve)}</span>
                  <span />
                  <span className="label">Largest Recovery Unit</span>
                  <span className="value">{formatBytes(analysis.plan.largest_unit_bytes)}</span>
                  <span />
                  <span className="label">Estimated Space Reclaimed</span>
                  <span className="value">{formatBytes(analysis.plan.estimated_source_reclaim)}</span>
                  <span />
                </div>
                {analysis.plan.progressive_feasible ? (
                  <div className="verdict ok">
                    <strong>
                      {analysis.plan.normal_feasible
                        ? "✅ Ready for Extraction: Both Normal and Progressive extraction are SAFE on this drive."
                        : "⚡ Low-Space Feasible: Normal extraction exceeds capacity, but Progressive Low-Space extraction is SAFE."}
                    </strong>
                  </div>
                ) : (
                  <div className="verdict bad">
                    <strong>⚠️ Progressive extraction is NOT SAFE on this volume.</strong>
                    {analysis.plan.reason && (
                      <div style={{ marginTop: 6, color: "var(--text-dim)" }}>
                        Reason: {analysis.plan.reason}
                      </div>
                    )}
                  </div>
                )}
              </div>

              <div style={{ display: "flex", gap: 10, justifyContent: "flex-end", alignItems: "center" }}>
                <button
                  className="primary"
                  disabled={running || !analysis.plan.normal_feasible}
                  onClick={() => {
                    setPendingChoice("normal");
                    setShowExtractChoice(true);
                  }}
                >
                  Extract
                </button>
                <button
                  className="primary"
                  disabled={running || !analysis.plan.progressive_feasible}
                  onClick={() => {
                    setPendingChoice("low");
                    setShowExtractChoice(true);
                  }}
                >
                  Extract (Low-Space)
                </button>
              </div>
            </>
          )}

          {!analysis && !error && (
            <div className="empty">
              Open an archive and choose a destination, then press Analyze.
            </div>
          )}
        </div>
      )}

      {view === "extracting" && (
        <ProgressView
          progress={progress}
          destination={effectiveDest}
          onPause={() => void pauseJob()}
          onStop={() => void stopJob()}
          onCancel={() => void cancelJob()}
          onDone={() => {
            setView("home");
            setAnalysis(null);
          }}
          onResume={() => void doResume()}
        />
      )}

      {view === "recovery" && (
        <div className="layout">
          <div className="panel">
            <h2>Extraction was interrupted</h2>
            {(recovery ?? null) ? (
              <>
                <div className="summary">
                  Archive: <span className="strong">{recovery!.archive}</span>
                  <br />
                  Destination: <span className="strong">{recovery!.destination}</span>
                </div>
                <div className="recovery-stats">
                  <span className="label">Committed output</span>
                  <span className="value">{formatBytes(recovery!.committed_output_bytes)}</span>
                  <span className="label">Source reclaimed</span>
                  <span className="value">{formatBytes(recovery!.source_reclaimed_bytes)}</span>
                  <span className="label">Remaining source</span>
                  <span className="value">{formatBytes(recovery!.remaining_source_bytes)}</span>
                  <span className="label">Last safe checkpoint</span>
                  <span className="value">{recovery!.last_checkpoint}</span>
                </div>
                {recovery!.units.length > 0 && (
                  <div style={{ maxHeight: 140, overflow: "auto", marginTop: 8 }}>
                    <table>
                      <thead>
                        <tr>
                          <th>Unit</th>
                          <th>State</th>
                        </tr>
                      </thead>
                      <tbody>
                        {recovery!.units.map((u) => (
                          <tr key={u.seq}>
                            <td>{u.seq}</td>
                            <td>
                              <span className={u.state.includes("COMMITTED") || u.state.includes("RECLAIMED") ? "status done" : "status pending"}>
                                {u.state}
                              </span>
                            </td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                  </div>
                )}
                {recovery!.errors.length > 0 && (
                  <div style={{ marginTop: 8, fontSize: 12 }}>
                    <span className="warning-text">Recorded errors:</span>
                    {recovery!.errors.map((e, i) => (
                      <div key={i} className="mono" style={{ color: "var(--text-dim)" }}>
                        {e}
                      </div>
                    ))}
                  </div>
                )}
              </>
            ) : (
              <div className="summary">
                Interrupted jobs found. Select the archive in the command bar to inspect its job.
              </div>
            )}
            {interrupted.length > 0 && (
              <div style={{ marginTop: 10 }}>
                <table>
                  <thead>
                    <tr>
                      <th>Job</th>
                      <th>Archive</th>
                      <th>Destination</th>
                    </tr>
                  </thead>
                  <tbody>
                    {interrupted.map((j) => (
                      <tr
                        key={j.job_id}
                        style={{ cursor: "pointer" }}
                        onClick={() => void selectInterruptedJob(j)}
                        title="Click to select this job"
                      >
                        <td className="mono">{j.job_id.slice(0, 8)}</td>
                        <td>{j.archive}</td>
                        <td>{j.destination}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}
            <div style={{ display: "flex", gap: 8, justifyContent: "space-between", marginTop: 12 }}>
              <button onClick={() => setView("home")}>Back to Extract</button>
              <div style={{ display: "flex", gap: 8 }}>
                <button className="primary" onClick={doResume} disabled={!archive}>
                  Resume
                </button>
                <button onClick={inspectRecovery} disabled={!archive}>
                  Inspect
                </button>
                <button className="danger" onClick={doAbandon} disabled={!archive}>
                  Abandon Job
                </button>
              </div>
            </div>
            <div className="summary" style={{ color: "var(--text-dim)", fontSize: 12 }}>
              Previously reclaimed source data cannot be restored. ReclaimArc never offers rollback
              when the source no longer exists.
            </div>
          </div>
        </div>
      )}

      {showExtractChoice && (
        <div className="overlay">
          <div className="dialog">
            <h2>Extract</h2>
            <div className="body">
              <p>
                <strong>Normal Extraction</strong> keeps the original archive and requires enough
                free space for the full unpacked output.
              </p>
              <p>
                <strong>Low-Space Extraction</strong> progressively destroys verified portions of
                the source archive to reclaim space during extraction.
              </p>
              {pendingChoice === "low" && (
                <p className="danger-text">
                  Low-Space Extraction permanently destroys parts of the archive as it proceeds.
                  Previously reclaimed source data cannot be restored. Continue?
                </p>
              )}
            </div>
            <div className="actions">
              <button onClick={() => setShowExtractChoice(false)}>Cancel</button>
              {pendingChoice === "low" ? (
                <>
                  <button
                    className="primary"
                    onClick={() => {
                      setShowExtractChoice(false);
                      void doExtract(true);
                    }}
                  >
                    Start Low-Space Extraction
                  </button>
                  <button
                    onClick={() => {
                      setShowExtractChoice(false);
                      void doExtract(false);
                    }}
                  >
                    Normal Extraction
                  </button>
                </>
              ) : (
                <button
                  className="primary"
                  onClick={() => {
                    setShowExtractChoice(false);
                    void doExtract(false);
                  }}
                >
                  Start Extraction
                </button>
              )}
            </div>
          </div>
        </div>
      )}

      {showSettings && <SettingsDialog onClose={() => setShowSettings(false)} />}

      {showLogs && <LogsDialog onClose={() => setShowLogs(false)} />}
    </>
  );
}

function ProgressView({
  progress,
  destination,
  onPause,
  onStop,
  onCancel,
  onDone,
  onResume,
}: {
  progress: ProgressState;
  destination: string;
  onPause: () => void;
  onStop: () => void;
  onCancel: () => void;
  onDone: () => void;
  onResume: () => void;
}) {
  const isFinishedSuccess = progress.finished && !progress.error && !progress.paused && !progress.cancelled;

  if (isFinishedSuccess) {
    return (
      <div className="layout">
        <div className="panel" style={{ borderTop: "3px solid var(--ok)" }}>
          <h2 style={{ color: "var(--ok)", fontSize: 16 }}>
            🎉 Extraction Completed Successfully!
          </h2>
          <div className="summary" style={{ fontSize: 13.5, margin: "6px 0 16px" }}>
            All archive items have been extracted, verified with BLAKE3 cryptographic hashes, and safely committed.
          </div>
          <div className="plan-grid" style={{ marginBottom: 16 }}>
            <span className="label">Destination Folder</span>
            <span className="value mono" style={{ textAlign: "left", wordBreak: "break-all" }}>
              {destination}
            </span>
            <span />
            <span className="label">Total Output Written</span>
            <span className="value">{formatBytes(progress.writtenBytes || progress.verifiedBytes)}</span>
            <span />
            <span className="label">Verified Output</span>
            <span className="value">{formatBytes(progress.verifiedBytes || progress.writtenBytes)}</span>
            <span />
            <span className="label">Source Reclaimed</span>
            <span className="value">{formatBytes(progress.reclaimedBytes)}</span>
            <span />
            <span className="label">Current Free Disk Space</span>
            <span className="value">
              {progress.freeSpace !== null ? formatBytes(progress.freeSpace) : "—"}
            </span>
            <span />
          </div>
          <div style={{ display: "flex", gap: 10, justifyContent: "flex-end" }}>
            <button onClick={() => openFolder(destination)}>📁 Open Output Folder</button>
            <button className="primary" onClick={onDone}>
              Extract Another Archive
            </button>
          </div>
        </div>
      </div>
    );
  }

  if (progress.paused) {
    return (
      <div className="layout">
        <div className="panel" style={{ borderTop: "3px solid var(--warning)" }}>
          <h2 style={{ color: "var(--warning)", fontSize: 15 }}>
            ⏸️ Extraction Paused
          </h2>
          <div className="summary" style={{ margin: "6px 0 16px" }}>
            The extraction paused at a safe transaction boundary. Reclaimed source data remains intact and the job is ready to resume.
          </div>
          <div style={{ display: "flex", gap: 10, justifyContent: "flex-end" }}>
            <button onClick={onDone}>Back to Home</button>
            <button className="primary" onClick={onResume}>
              Resume Extraction
            </button>
          </div>
        </div>
      </div>
    );
  }

  const preTestPct =
    progress.preTest && progress.preTest.total > 0
      ? Math.min(100, (progress.preTest.current / progress.preTest.total) * 100)
      : 0;

  const entryPct =
    progress.entryTotal > 0
      ? Math.min(100, (progress.entryCurrent / progress.entryTotal) * 100)
      : 0;

  return (
    <div className="layout">
      <div className="panel">
        <h2>Extraction in Progress</h2>
        <div className="summary">
          Job <span className="strong">{progress.jobId.slice(0, 8)}</span>
          {progress.currentUnit !== null && (
            <> · Recovery unit <span className="strong">{progress.currentUnit}</span></>
          )}
        </div>
        {progress.preTest ? (
          <>
            <div className="summary" style={{ display: "flex", justifyContent: "space-between" }}>
              <span>🔍 Verifying archive integrity before progressive extraction…</span>
              <span>
                {formatBytes(progress.preTest.current)} / {formatBytes(progress.preTest.total)} ({preTestPct.toFixed(1)}%)
              </span>
            </div>
            <div className="progress-track">
              <div
                className="progress-fill"
                style={{
                  width: `${preTestPct}%`,
                  transition: "width 0.2s ease",
                }}
              />
            </div>
          </>
        ) : (
          <>
            <div className="summary" style={{ display: "flex", justifyContent: "space-between" }}>
              <span>📄 {progress.currentEntry || "Preparing files…"}</span>
              {progress.entryTotal > 0 && (
                <span>
                  {formatBytes(progress.entryCurrent)} / {formatBytes(progress.entryTotal)} ({entryPct.toFixed(1)}%)
                </span>
              )}
            </div>
            <div className="progress-track">
              <div
                className="progress-fill"
                style={{ width: `${entryPct}%`, transition: "width 0.15s ease" }}
              />
            </div>
            <div className="progress-row">
              <span className="label">Written</span>
              <span className="value">{formatBytes(progress.writtenBytes || progress.verifiedBytes)}</span>
              <span className="label">Verified</span>
              <span className="value">{formatBytes(progress.verifiedBytes)}</span>
              <span className="label">Source reclaimed</span>
              <span className="value">{formatBytes(progress.reclaimedBytes)}</span>
              <span className="label">Current free space</span>
              <span className="value">
                {progress.freeSpace !== null ? formatBytes(progress.freeSpace) : "—"}
              </span>
            </div>
          </>
        )}
        {progress.error && (
          <div className="error-banner" style={{ marginTop: 12 }}>
            <strong>Extraction Error:</strong> {progress.error}
          </div>
        )}
        <div style={{ display: "flex", gap: 8, justifyContent: "flex-end", marginTop: 14 }}>
          {progress.error ? (
            <button onClick={onDone}>Back to Home</button>
          ) : (
            <>
              <button onClick={onPause}>Pause</button>
              <button onClick={onStop}>Stop Safely</button>
              <button onClick={onCancel}>Cancel</button>
            </>
          )}
        </div>
        <div className="summary" style={{ color: "var(--text-dim)", fontSize: 12, marginTop: 10 }}>
          Pause and Stop Safely finish or safely abort at the current recovery unit and keep the
          source intact. Cancel keeps the job resumable; previously reclaimed source data cannot be restored.
        </div>
      </div>
    </div>
  );
}

function SettingsDialog({ onClose }: { onClose: () => void }) {
  const [settings, setSettingsState] = useState<SettingsDto | null>(null);
  useEffect(() => {
    getSettings().then(setSettingsState).catch(() => undefined);
  }, []);

  if (!settings) return null;

  const save = async () => {
    await setSettings(settings);
    onClose();
  };

  return (
    <div className="overlay">
      <div className="dialog">
        <h2>Settings</h2>
        <div className="body">
          <div className="settings-grid">
            <span>Safety preset</span>
            <select
              value={settings.safety_mode}
              onChange={(e) => setSettingsState({ ...settings, safety_mode: e.target.value })}
            >
              <option value="safe">Safe</option>
              <option value="balanced">Balanced</option>
              <option value="maximum-space">Maximum Space</option>
            </select>
            <span>Existing files</span>
            <select
              value={settings.conflict_policy}
              onChange={(e) => setSettingsState({ ...settings, conflict_policy: e.target.value })}
            >
              <option value="overwrite">Overwrite</option>
              <option value="skip">Skip</option>
              <option value="rename-new">Rename new</option>
              <option value="ask">Ask</option>
            </select>
            <span>Pre-test archive before destructive extraction</span>
            <input
              type="checkbox"
              checked={settings.pre_test}
              onChange={(e) => setSettingsState({ ...settings, pre_test: e.target.checked })}
            />
            <span>Write BLAKE3 checksum manifest</span>
            <input
              type="checkbox"
              checked={settings.write_manifest}
              onChange={(e) => setSettingsState({ ...settings, write_manifest: e.target.checked })}
            />
            <span>Retain previous recovery unit (Safe mode)</span>
            <input
              type="checkbox"
              checked={settings.retain_previous_unit}
              onChange={(e) =>
                setSettingsState({ ...settings, retain_previous_unit: e.target.checked })
              }
            />
            <span>Delete source shells on completion</span>
            <input
              type="checkbox"
              checked={settings.delete_shells_on_completion}
              onChange={(e) =>
                setSettingsState({ ...settings, delete_shells_on_completion: e.target.checked })
              }
            />
            <span>Logging level</span>
            <select
              value={settings.log_level}
              onChange={(e) => setSettingsState({ ...settings, log_level: e.target.value })}
            >
              <option value="error">Error</option>
              <option value="warn">Warning</option>
              <option value="info">Info</option>
              <option value="debug">Debug</option>
            </select>
          </div>
        </div>
        <div className="actions">
          <button onClick={onClose}>Cancel</button>
          <button className="primary" onClick={() => void save()}>
            Save
          </button>
        </div>
      </div>
    </div>
  );
}

function LogsDialog({ onClose }: { onClose: () => void }) {
  const [logs, setLogs] = useState<string>("");
  useEffect(() => {
    readLogs(200).then(setLogs).catch(() => undefined);
  }, []);
  return (
    <div className="overlay">
      <div className="dialog" style={{ width: 720 }}>
        <h2>Logs</h2>
        <div className="body">
          <div className="logs mono">{logs}</div>
        </div>
        <div className="actions">
          <button onClick={() => void openLogsDir()}>Open Logs Folder</button>
          <button className="primary" onClick={onClose}>
            Close
          </button>
        </div>
      </div>
    </div>
  );
}


