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
  type ArchiveEntry,
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
  hadRedirections: boolean;
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
  hadRedirections: false,
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
  const [recoveryPassword, setRecoveryPassword] = useState("");
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
        case "range-reclaimed":
          setProgress((p) => ({
            ...p,
            reclaimedBytes: p.reclaimedBytes + Number(ev.bytes),
          }));
          break;
        case "unit-reclaimed":
          // Unit completion marker; physical bytes are tracked incrementally via range-reclaimed.
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
    const hadRedirections = analysis
      ? analysis.info.entries.some((e) => e.redirection !== null)
      : false;
    setProgress({ ...emptyProgress(), hadRedirections });
    try {
      const targetDest = destRef.current || destination.trim() || getParentDirectory(archive);
      const id = await startExtraction(
        archive,
        targetDest,
        lowSpace,
        password || undefined,
      );
      setProgress((p) => ({ ...p, jobId: id, hadRedirections }));
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
    const hadRedirections = analysis
      ? analysis.info.entries.some((e) => e.redirection !== null)
      : false;
    setProgress({
      ...emptyProgress(),
      writtenBytes: initialWritten,
      verifiedBytes: initialWritten,
      reclaimedBytes: initialReclaimed,
      hadRedirections,
    });
    try {
      const id = await resumeExtraction(archive, recoveryPassword || password || undefined);
      setProgress((p) => ({ ...p, jobId: id, hadRedirections }));
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

  const effectiveDest = destination.trim() || (archive ? getParentDirectory(archive) : "");

  return (
    <>
      <header className="command-bar">
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
      </header>

      {error && <div className="error-banner">{error}</div>}

      {view === "home" && (
        <div className="layout">
          <ArchiveSetup
            archive={archive}
            destination={effectiveDest}
            password={password}
            onArchiveChange={(a) => {
              setArchive(a);
              setAnalysis(null);
            }}
            onDestinationChange={(d) => {
              setDestination(d);
              destRef.current = d;
            }}
            onPasswordChange={setPassword}
            onOpenArchive={openArchive}
            onOpenDestination={openDestination}
            onAnalyze={doAnalyze}
            analyzing={analyzing}
          />

          {analysis && (
            <>
              <ArchiveAnalysisPanel
                analysis={analysis}
                onReanalyze={doAnalyze}
                analyzing={analyzing}
              />

              <SpacePlanPanel
                plan={analysis.plan}
                running={running}
                onStartNormal={() => {
                  setPendingChoice("normal");
                  setShowExtractChoice(true);
                }}
                onStartLowSpace={() => {
                  setPendingChoice("low");
                  setShowExtractChoice(true);
                }}
              />
            </>
          )}

          {!analysis && !error && (
            <div className="empty">
              Open a RAR archive and select a destination, then click Analyze.
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
        <RecoveryViewComponent
          archive={archive}
          recovery={recovery}
          interrupted={interrupted}
          recoveryPassword={recoveryPassword}
          onPasswordChange={setRecoveryPassword}
          onSelectJob={selectInterruptedJob}
          onResume={doResume}
          onInspect={inspectRecovery}
          onAbandon={doAbandon}
          onBackToHome={() => setView("home")}
        />
      )}

      {showExtractChoice && (
        <ExtractChoiceModal
          pendingChoice={pendingChoice}
          onClose={() => setShowExtractChoice(false)}
          onConfirm={(lowSpace) => {
            setShowExtractChoice(false);
            void doExtract(lowSpace);
          }}
        />
      )}

      {showSettings && <SettingsDialog onClose={() => setShowSettings(false)} />}
      {showLogs && <LogsDialog onClose={() => setShowLogs(false)} />}
    </>
  );
}

function ArchiveSetup({
  archive,
  destination,
  password,
  onArchiveChange,
  onDestinationChange,
  onPasswordChange,
  onOpenArchive,
  onOpenDestination,
  onAnalyze,
  analyzing,
}: {
  archive: string;
  destination: string;
  password: string;
  onArchiveChange: (a: string) => void;
  onDestinationChange: (d: string) => void;
  onPasswordChange: (p: string) => void;
  onOpenArchive: () => void;
  onOpenDestination: () => void;
  onAnalyze: () => void;
  analyzing: boolean;
}) {
  const [hasPassword, setHasPassword] = useState(false);

  return (
    <section className="panel">
      <h2>Source Archive and Destination</h2>
      <div className="setup-fields">
        <div className="field-row">
          <label className="field-label">RAR Archive:</label>
          <div className="path-field">
            <input
              type="text"
              value={archive}
              placeholder="Select RAR archive (.rar, .part1.rar)"
              onChange={(e) => onArchiveChange(e.target.value)}
            />
            <button onClick={onOpenArchive}>Browse…</button>
          </div>
        </div>

        <div className="field-row">
          <label className="field-label">Destination Folder:</label>
          <div className="path-field">
            <input
              type="text"
              value={destination}
              placeholder="Extraction destination folder"
              onChange={(e) => onDestinationChange(e.target.value)}
            />
            <button onClick={onOpenDestination}>Browse…</button>
          </div>
        </div>

        <div className="field-row">
          <label className="field-label" />
          <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
            <label style={{ display: "flex", alignItems: "center", gap: 6, cursor: "pointer", fontSize: 12.5 }}>
              <input
                type="checkbox"
                checked={hasPassword}
                onChange={(e) => setHasPassword(e.target.checked)}
              />
              <span>Archive is password-protected</span>
            </label>
            {hasPassword && (
              <input
                type="password"
                value={password}
                placeholder="Enter password (held in memory only)"
                onChange={(e) => onPasswordChange(e.target.value)}
                style={{ width: 320 }}
              />
            )}
          </div>
        </div>
      </div>

      <div style={{ display: "flex", justifyContent: "flex-end", marginTop: 12 }}>
        <button
          className="primary"
          onClick={onAnalyze}
          disabled={!archive || analyzing}
        >
          {analyzing ? "Analyzing Archive…" : "Analyze Archive"}
        </button>
      </div>
    </section>
  );
}

function ArchiveAnalysisPanel({
  analysis,
  onReanalyze,
  analyzing,
}: {
  analysis: AnalyzeResult;
  onReanalyze: () => void;
  analyzing: boolean;
}) {
  const [filterText, setFilterText] = useState("");

  const filteredEntries = analysis.info.entries.filter((e) =>
    e.name.toLowerCase().includes(filterText.toLowerCase()),
  );

  return (
    <section className="panel">
      <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", marginBottom: 8 }}>
        <h2>Archive Summary</h2>
        <button onClick={onReanalyze} disabled={analyzing}>
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
          placeholder="Filter files by name…"
          onChange={(e) => setFilterText(e.target.value)}
        />
        {filterText && (
          <button onClick={() => setFilterText("")}>Clear</button>
        )}
        <span style={{ fontSize: 12, color: "var(--text-dim)", whiteSpace: "nowrap" }}>
          Showing {filteredEntries.length} of {analysis.info.entries.length}
        </span>
      </div>

      <div style={{ maxHeight: 240, overflow: "auto" }}>
        <FilesTable entries={filteredEntries} analysis={analysis} filterText={filterText} />
      </div>
    </section>
  );
}

function FilesTable({
  entries,
  analysis,
  filterText,
}: {
  entries: ArchiveEntry[];
  analysis: AnalyzeResult;
  filterText: string;
}) {
  return (
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
        {entries.map((e) => {
          const unit = unitForIndex(analysis, e.index);
          let statusText = "file";
          let statusClass = "pending";

          if (e.redirection !== null) {
            statusText = "Skipped (link policy)";
            statusClass = "pending";
          } else if (e.is_directory) {
            statusText = "dir";
            statusClass = "pending";
          } else if (e.is_solid) {
            statusText = "solid";
            statusClass = "running";
          }

          return (
            <tr key={e.index}>
              <td>{e.name}</td>
              <td className="num">{formatBytes(e.packed_size)}</td>
              <td className="num">{formatBytes(e.unpacked_size)}</td>
              <td className="num">{ratio(e.packed_size, e.unpacked_size)}</td>
              <td className="num">{unit ?? "—"}</td>
              <td>
                <span className={`status ${statusClass}`}>{statusText}</span>
              </td>
            </tr>
          );
        })}
        {entries.length === 0 && (
          <tr>
            <td colSpan={6} style={{ textAlign: "center", color: "var(--text-dim)", padding: 16 }}>
              No entries matching "{filterText}"
            </td>
          </tr>
        )}
      </tbody>
    </table>
  );
}

function SpacePlanPanel({
  plan,
  running,
  onStartNormal,
  onStartLowSpace,
}: {
  plan: AnalyzeResult["plan"];
  running: boolean;
  onStartNormal: () => void;
  onStartLowSpace: () => void;
}) {
  return (
    <section className="panel">
      <h2>Space Plan & Feasibility</h2>
      <div className="plan-grid">
        <span className="label">Free Disk Space Now</span>
        <span className="value">{formatBytes(plan.free_now)}</span>
        <span />
        <span className="label">Normal Extraction Needed</span>
        <span className="value">{formatBytes(plan.unpacked_total)}</span>
        <span />
        <span className="label">Progressive Peak Requirement</span>
        <span className="value">
          {plan.progressive_peak_requirement === 0
            ? "Fits without reclamation"
            : formatBytes(plan.progressive_peak_requirement)}
        </span>
        <span />
        <span className="label">Safety Reserve</span>
        <span className="value">{formatBytes(plan.reserve)}</span>
        <span />
        <span className="label">Largest Recovery Unit</span>
        <span className="value">{formatBytes(plan.largest_unit_bytes)}</span>
        <span />
        <span className="label">Estimated Space Reclaimed</span>
        <span className="value">{formatBytes(plan.estimated_source_reclaim)}</span>
        <span />
      </div>

      {plan.progressive_feasible ? (
        <div className="verdict ok">
          <strong>
            {plan.normal_feasible
              ? "Normal and Low-Space extraction are feasible."
              : "Low-Space extraction is feasible with the current space plan."}
          </strong>
        </div>
      ) : (
        <div className="verdict bad">
          <strong>Progressive extraction is not feasible with the current space plan.</strong>
          {plan.reason && (
            <div style={{ marginTop: 4, color: "var(--text-dim)" }}>
              Reason: {plan.reason}
            </div>
          )}
        </div>
      )}

      <div style={{ display: "flex", gap: 10, justifyContent: "flex-end", marginTop: 14 }}>
        <button
          disabled={running || !plan.normal_feasible}
          onClick={onStartNormal}
        >
          Normal Extraction
        </button>
        <button
          className="primary"
          disabled={running || !plan.progressive_feasible}
          onClick={onStartLowSpace}
        >
          Low-Space Extraction
        </button>
      </div>
    </section>
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
  const isFinishedSuccess =
    progress.finished && !progress.error && !progress.paused && !progress.cancelled;

  if (isFinishedSuccess) {
    const completionCopy = progress.hadRedirections
      ? "Extraction completed. All extractable files were verified and committed. Link/redirection entries were skipped according to the configured safety policy."
      : "Extraction completed. All extractable files were verified and committed.";

    return (
      <div className="layout">
        <section className="panel" style={{ borderTop: "3px solid var(--ok)" }}>
          <h2 style={{ color: "var(--ok)", fontSize: 15 }}>
            Extraction Completed
          </h2>
          <div className="summary" style={{ margin: "6px 0 16px" }}>
            {completionCopy}
          </div>
          <div className="plan-grid" style={{ marginBottom: 16 }}>
            <span className="label">Destination</span>
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
            <span className="label">Current Free Space</span>
            <span className="value">
              {progress.freeSpace !== null ? formatBytes(progress.freeSpace) : "—"}
            </span>
            <span />
          </div>
          <div style={{ display: "flex", gap: 10, justifyContent: "flex-end" }}>
            <button onClick={() => openFolder(destination)}>Open Output Folder</button>
            <button className="primary" onClick={onDone}>
              Extract Another Archive
            </button>
          </div>
        </section>
      </div>
    );
  }

  if (progress.paused) {
    return (
      <div className="layout">
        <section className="panel" style={{ borderTop: "3px solid var(--warning)" }}>
          <h2 style={{ color: "var(--warning)", fontSize: 15 }}>
            Extraction Paused
          </h2>
          <div className="summary" style={{ margin: "6px 0 16px" }}>
            The current recovery unit remains recoverable. Source ranges reclaimed by completed units cannot be restored.
          </div>
          <div style={{ display: "flex", gap: 10, justifyContent: "flex-end" }}>
            <button onClick={onDone}>Back to Home</button>
            <button className="primary" onClick={onResume}>
              Resume Extraction
            </button>
          </div>
        </section>
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
      <section className="panel">
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
              <span>Verifying archive integrity before progressive extraction…</span>
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
              <span>{progress.currentEntry || "Preparing files…"}</span>
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
          Pause and Stop Safely finish or safely abort at the current recovery unit and keep its
          source intact. Cancel keeps the job resumable; previously reclaimed source data cannot be restored.
        </div>
      </section>
    </div>
  );
}

function RecoveryViewComponent({
  archive,
  recovery,
  interrupted,
  recoveryPassword,
  onPasswordChange,
  onSelectJob,
  onResume,
  onInspect,
  onAbandon,
  onBackToHome,
}: {
  archive: string;
  recovery: RecoveryView | null;
  interrupted: JobListEntry[];
  recoveryPassword: string;
  onPasswordChange: (p: string) => void;
  onSelectJob: (j: JobListEntry) => void;
  onResume: () => void;
  onInspect: () => void;
  onAbandon: () => void;
  onBackToHome: () => void;
}) {
  return (
    <div className="layout">
      <section className="panel">
        <h2>Interrupted Extractions</h2>
        {recovery ? (
          <>
            <div className="summary">
              Archive: <span className="strong">{recovery.archive}</span>
              <br />
              Destination: <span className="strong">{recovery.destination}</span>
            </div>
            <div className="recovery-stats">
              <span className="label">Committed output</span>
              <span className="value">{formatBytes(recovery.committed_output_bytes)}</span>
              <span className="label">Source reclaimed</span>
              <span className="value">{formatBytes(recovery.source_reclaimed_bytes)}</span>
              <span className="label">Remaining source</span>
              <span className="value">{formatBytes(recovery.remaining_source_bytes)}</span>
              <span className="label">Last safe checkpoint</span>
              <span className="value">{recovery.last_checkpoint}</span>
            </div>
            {recovery.units.length > 0 && (
              <div style={{ maxHeight: 140, overflow: "auto", marginTop: 8 }}>
                <table>
                  <thead>
                    <tr>
                      <th>Unit</th>
                      <th>State</th>
                    </tr>
                  </thead>
                  <tbody>
                    {recovery.units.map((u) => (
                      <tr key={u.seq}>
                        <td>{u.seq}</td>
                        <td>
                          <span
                            className={
                              u.state.includes("COMMITTED") || u.state.includes("RECLAIMED")
                                ? "status done"
                                : "status pending"
                            }
                          >
                            {u.state}
                          </span>
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}
            {recovery.errors.length > 0 && (
              <div style={{ marginTop: 8, fontSize: 12 }}>
                <span className="warning-text">Recorded errors:</span>
                {recovery.errors.map((e, i) => (
                  <div key={i} className="mono" style={{ color: "var(--text-dim)" }}>
                    {e}
                  </div>
                ))}
              </div>
            )}
          </>
        ) : (
          <div className="summary">
            Select an interrupted job to inspect its recovery state.
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
                    onClick={() => onSelectJob(j)}
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
        <div style={{ marginTop: 12 }}>
          <label style={{ display: "block", fontSize: 12, marginBottom: 4, color: "var(--text-dim)" }}>
            Archive Password (if encrypted):
          </label>
          <input
            type="password"
            placeholder="Enter password to resume…"
            value={recoveryPassword}
            onChange={(e) => onPasswordChange(e.target.value)}
            style={{ width: "100%", maxWidth: 360 }}
          />
        </div>
        <div style={{ display: "flex", gap: 8, justifyContent: "space-between", marginTop: 12 }}>
          <button onClick={onBackToHome}>Back to Extract</button>
          <div style={{ display: "flex", gap: 8 }}>
            <button className="primary" onClick={onResume} disabled={!archive}>
              Resume
            </button>
            <button onClick={onInspect} disabled={!archive}>
              Inspect
            </button>
            <button className="danger" onClick={onAbandon} disabled={!archive}>
              Abandon Job
            </button>
          </div>
        </div>
        <div className="summary" style={{ color: "var(--text-dim)", fontSize: 12, marginTop: 10 }}>
          Previously reclaimed source data cannot be restored.
        </div>
      </section>
    </div>
  );
}

function ExtractChoiceModal({
  pendingChoice,
  onClose,
  onConfirm,
}: {
  pendingChoice: "normal" | "low" | null;
  onClose: () => void;
  onConfirm: (lowSpace: boolean) => void;
}) {
  return (
    <div className="overlay">
      <div className="dialog">
        <h2>Confirm Extraction</h2>
        <div className="body">
          <p>
            <strong>Normal Extraction</strong> preserves the original source archive on disk.
          </p>
          <p>
            <strong>Low-Space Extraction</strong> progressively deallocates verified source byte ranges
            during extraction to fit within constrained disk space.
          </p>
          {pendingChoice === "low" && (
            <p className="danger-text">
              Low-Space Extraction is irreversible for completed units. Previously reclaimed source
              ranges cannot be restored.
            </p>
          )}
        </div>
        <div className="actions">
          <button onClick={onClose}>Cancel</button>
          {pendingChoice === "low" ? (
            <>
              <button className="primary" onClick={() => onConfirm(true)}>
                Start Low-Space Extraction
              </button>
              <button onClick={() => onConfirm(false)}>
                Normal Extraction
              </button>
            </>
          ) : (
            <button className="primary" onClick={() => onConfirm(false)}>
              Start Extraction
            </button>
          )}
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
    try {
      await setSettings(settings);
      onClose();
    } catch (e) {
      alert(`Failed to save settings: ${e}`);
    }
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


