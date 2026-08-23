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
  error: null,
});

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
      if (jobs.length > 0) setView("recovery");
    } catch {
      /* not fatal */
    }
  }, []);

  useEffect(() => {
    refreshInterrupted();
    const unlisten = listen<SxEvent>("sx://event", (e) => {
      const ev = e.payload;
      switch (ev.type) {
        case "pre-test-started":
          setProgress((p) => ({ ...p, preTest: { current: 0, total: Number(ev.total) } }));
          break;
        case "pre-test-progress":
          setProgress((p) => ({ ...p, preTest: { current: Number(ev.current), total: Number(ev.total) } }));
          break;
        case "pre-test-finished":
          setProgress((p) => ({ ...p, preTest: null, preTestOk: Boolean(ev.ok) }));
          break;
        case "unit-started":
          setProgress((p) => ({ ...p, currentUnit: Number(ev.seq), currentEntry: "", entryCurrent: 0, entryTotal: 0 }));
          break;
        case "entry-started":
          setProgress((p) => ({ ...p, currentEntry: String(ev.name), entryCurrent: 0 }));
          break;
        case "entry-progress":
          setProgress((p) => ({ ...p, entryCurrent: Number(ev.current), entryTotal: Number(ev.total) }));
          break;
        case "entry-committed":
          setProgress((p) => ({ ...p, writtenBytes: p.writtenBytes + p.unitBytes }));
          break;
        case "unit-committed":
          setProgress((p) => ({ ...p, verifiedBytes: p.verifiedBytes + Number(ev.bytes) }));
          break;
        case "unit-reclaimed":
          setProgress((p) => ({ ...p, reclaimedBytes: p.reclaimedBytes + Number(ev.bytes) }));
          break;
        case "range-reclaimed":
          setProgress((p) => ({ ...p, reclaimedBytes: p.reclaimedBytes + Number(ev.bytes) }));
          break;
        case "free-space":
          setProgress((p) => ({ ...p, freeSpace: Number(ev.bytes) }));
          break;
        case "job-paused":
          setProgress((p) => ({ ...p, finished: true }));
          setRunning(false);
          setView("recovery");
          refreshInterrupted();
          break;
        case "job-cancelled":
          setProgress((p) => ({ ...p, finished: true }));
          setRunning(false);
          setView("recovery");
          refreshInterrupted();
          break;
        case "job-finished":
          setProgress((p) => ({ ...p, finished: true }));
          setRunning(false);
          setView("home");
          refreshInterrupted();
          break;
        case "job-failed":
          setProgress((p) => ({ ...p, finished: true, error: String(ev.message) }));
          setRunning(false);
          setView("home");
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
    try {
      const result = await analyze(archive, destination, password || undefined);
      setAnalysis(result);
      destRef.current = destination;
    } catch (e) {
      setError(String(e));
    } finally {
      setAnalyzing(false);
    }
  };

  const doExtract = async (lowSpace: boolean) => {
    if (!archive) return;
    setError(null);
    setProgress(emptyProgress());
    try {
      const id = await startExtraction(
        archive,
        destRef.current || destination,
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
    setProgress(emptyProgress());
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
      setInterrupted([]);
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
    } catch (e) {
      setError(String(e));
    }
  };

  const openArchive = async () => {
    const p = await pickArchive();
    if (p) {
      setArchive(p);
      setAnalysis(null);
      // Auto-probe recovery for this archive.
      try {
        const r = await recoveryView(p);
        setRecovery(r);
        setView("recovery");
      } catch {
        setRecovery(null);
      }
    }
  };

  const openDestination = async () => {
    const p = await pickDirectory();
    if (p) setDestination(p);
  };

  return (
    <>
      <div className="command-bar">
        <span className="title">SpaceExtract</span>
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
          <div className="panel">
            <h2>Archive</h2>
            <div className="path-field">
              <input
                type="text"
                value={archive}
                placeholder="Path to a .rar archive"
                onChange={(e) => setArchive(e.target.value)}
              />
              <button onClick={openArchive}>Browse…</button>
            </div>
            <div style={{ height: 8 }} />
            <div className="path-field">
              <input
                type="text"
                value={destination}
                placeholder="Destination folder"
                onChange={(e) => setDestination(e.target.value)}
              />
              <button onClick={openDestination}>Browse…</button>
            </div>
            <div style={{ height: 8 }} />
            <input
              type="password"
              value={password}
              placeholder="Password (optional, memory only)"
              onChange={(e) => setPassword(e.target.value)}
            />
          </div>

          {analysis && (
            <>
              <div className="panel">
                <h2>Archive summary</h2>
                <div className="summary">
                  <span className="strong">{analysis.info.format.toUpperCase()}</span>
                  {" · "}
                  {formatBytes(analysis.info.packed_size)} packed ·{" "}
                  {formatBytes(analysis.info.unpacked_size)} unpacked ·{" "}
                  {analysis.info.solid_archive ? "Solid" : "Non-solid"}
                  {analysis.info.encrypted_headers && " · Encrypted headers"}
                  {analysis.info.volumes.length > 1 && ` · ${analysis.info.volumes.length} volumes`}
                </div>
                <div style={{ maxHeight: 260, overflow: "auto" }}>
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
                      {analysis.info.entries.map((e) => {
                        const unit = analysis.info.recovery_units.find(
                          (u) => e.index >= u.first_entry && e.index <= u.last_entry,
                        );
                        return (
                          <tr key={e.index}>
                            <td>{e.name}</td>
                            <td className="num">{formatBytes(e.packed_size)}</td>
                            <td className="num">{formatBytes(e.unpacked_size)}</td>
                            <td className="num">{ratio(e.packed_size, e.unpacked_size)}</td>
                            <td className="num">{unit?.seq ?? "—"}</td>
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
                    </tbody>
                  </table>
                </div>
              </div>

              <div className="panel">
                <h2>Space plan</h2>
                <div className="plan-grid">
                  <span className="label">Free now</span>
                  <span className="value">{formatBytes(analysis.plan.free_now)}</span>
                  <span />
                  <span className="label">Normal extraction requirement</span>
                  <span className="value">{formatBytes(analysis.plan.unpacked_total)}</span>
                  <span />
                  <span className="label">Progressive peak requirement</span>
                  <span className="value">
                    {analysis.plan.progressive_peak_requirement === 0
                      ? "fits without reclaim"
                      : formatBytes(analysis.plan.progressive_peak_requirement)}
                  </span>
                  <span />
                  <span className="label">Safety reserve</span>
                  <span className="value">{formatBytes(analysis.plan.reserve)}</span>
                  <span />
                  <span className="label">Largest recovery unit</span>
                  <span className="value">{formatBytes(analysis.plan.largest_unit_bytes)}</span>
                  <span />
                  <span className="label">Estimated source reclaim</span>
                  <span className="value">{formatBytes(analysis.plan.estimated_source_reclaim)}</span>
                  <span />
                </div>
                {analysis.plan.progressive_feasible ? (
                  <div className="verdict ok">
                    {analysis.plan.normal_feasible
                      ? "Normal extraction: POSSIBLE · Progressive extraction: POSSIBLE"
                      : "Normal extraction: IMPOSSIBLE · Progressive extraction: POSSIBLE"}
                  </div>
                ) : (
                  <div className="verdict bad">
                    Progressive extraction: NOT SAFE
                    {analysis.plan.reason && (
                      <div style={{ marginTop: 6, color: "var(--text-dim)" }}>
                        {analysis.plan.reason}
                      </div>
                    )}
                  </div>
                )}
              </div>

              <div style={{ display: "flex", gap: 8, justifyContent: "flex-end" }}>
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
          onPause={() => void pauseJob()}
          onStop={() => void stopJob()}
          onCancel={() => void cancelJob()}
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
                      <tr key={j.job_id}>
                        <td className="mono">{j.job_id.slice(0, 8)}</td>
                        <td>{j.archive}</td>
                        <td>{j.destination}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}
            <div style={{ display: "flex", gap: 8, justifyContent: "flex-end", marginTop: 12 }}>
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
            <div className="summary" style={{ color: "var(--text-dim)", fontSize: 12 }}>
              Previously reclaimed source data cannot be restored. SpaceExtract never offers rollback
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
  onPause,
  onStop,
  onCancel,
}: {
  progress: ProgressState;
  onPause: () => void;
  onStop: () => void;
  onCancel: () => void;
}) {
  const entryPct = progress.entryTotal > 0 ? (progress.entryCurrent / progress.entryTotal) * 100 : 0;
  return (
    <div className="layout">
      <div className="panel">
        <h2>Extraction</h2>
        <div className="summary">
          Job <span className="strong">{progress.jobId.slice(0, 8)}</span>
          {progress.currentUnit !== null && (
            <> · Recovery unit <span className="strong">{progress.currentUnit}</span></>
          )}
        </div>
        {progress.preTest ? (
          <>
            <div className="summary">Testing archive integrity…</div>
            <div className="progress-track">
              <div
                className="progress-fill"
                style={{
                  width: `${progress.preTest.total > 0 ? (progress.preTest.current / progress.preTest.total) * 100 : 0}%`,
                }}
              />
            </div>
          </>
        ) : (
          <>
            <div className="summary">
              {progress.currentEntry || "Preparing…"}
              {progress.entryTotal > 0 &&
                ` (${formatBytes(progress.entryCurrent)} / ${formatBytes(progress.entryTotal)})`}
            </div>
            <div className="progress-track">
              <div className="progress-fill" style={{ width: `${entryPct}%` }} />
            </div>
            <div className="progress-row">
              <span className="label">Written</span>
              <span className="value">{formatBytes(progress.writtenBytes)}</span>
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
        {progress.error && <div className="error-banner">{progress.error}</div>}
        <div style={{ display: "flex", gap: 8, justifyContent: "flex-end", marginTop: 14 }}>
          <button onClick={onPause}>Pause</button>
          <button onClick={onStop}>Stop Safely</button>
          <button onClick={onCancel}>Cancel</button>
        </div>
        <div className="summary" style={{ color: "var(--text-dim)", fontSize: 12 }}>
          Pause and Stop Safely finish or safely abort the current recovery unit and keep the
          source for it intact. Cancel keeps the job resumable; previously reclaimed source data
          cannot be restored.
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


