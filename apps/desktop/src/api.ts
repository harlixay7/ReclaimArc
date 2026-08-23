// Typed bridge to the Tauri backend.

import { invoke } from "@tauri-apps/api/core";

export interface VolumeInfo {
  index: number;
  path: string;
  logical_size: number;
}

export interface ArchiveEntry {
  index: number;
  name: string;
  packed_size: number;
  unpacked_size: number;
  crc32: number | null;
  is_directory: boolean;
  is_solid: boolean;
  split_before: boolean;
  split_after: boolean;
  encrypted: boolean;
  redirection: null | { kind: string; target: string };
}

export interface PackedRange {
  volume_index: number;
  start: number;
  len: number;
}

export interface RecoveryUnit {
  seq: number;
  first_entry: number;
  last_entry: number;
  packed_ranges: PackedRange[];
  unpacked_bytes: number;
}

export interface ArchiveInfo {
  format: string;
  packed_size: number;
  unpacked_size: number;
  solid_archive: boolean;
  encrypted_headers: boolean;
  volumes: VolumeInfo[];
  entries: ArchiveEntry[];
  recovery_units: RecoveryUnit[];
  capability: {
    format: string;
    supports_test_integrity: boolean;
    restartable_units: boolean;
    progressive_reclaim: boolean;
    supports_encryption: boolean;
    supports_multipart: boolean;
    notes: string[];
  };
}

export interface SpacePlan {
  progressive_feasible: boolean;
  normal_feasible: boolean;
  free_now: number;
  unpacked_total: number;
  progressive_peak_requirement: number;
  reserve: number;
  scratch: number;
  largest_unit_bytes: number;
  estimated_source_reclaim: number;
  reason: string | null;
}

export interface AnalyzeResult {
  info: ArchiveInfo;
  plan: SpacePlan;
}

export interface JobListEntry {
  job_id: string;
  archive: string;
  destination: string;
  status: string;
  updated_at: string;
}

export interface RecoveryView {
  job_id: string;
  archive: string;
  destination: string;
  committed_output_bytes: number;
  source_reclaimed_bytes: number;
  remaining_source_bytes: number;
  last_checkpoint: string;
  units: { seq: number; state: string }[];
  errors: string[];
}

export interface SettingsDto {
  safety_mode: string;
  conflict_policy: string;
  pre_test: boolean;
  write_manifest: boolean;
  retain_previous_unit: boolean;
  delete_shells_on_completion: boolean;
  log_level: string;
}

export interface SxEvent {
  type: string;
  [key: string]: unknown;
}

export async function analyze(
  archive: string,
  destination: string,
  password?: string,
): Promise<AnalyzeResult> {
  return invoke("analyze", { archive, destination, password: password ?? null });
}

export async function startExtraction(
  archive: string,
  destination: string,
  lowSpace: boolean,
  password?: string,
): Promise<string> {
  return invoke("start_extraction", {
    archive,
    destination,
    lowSpace,
    password: password ?? null,
  });
}

export async function resumeExtraction(archive: string): Promise<string> {
  return invoke("resume_extraction", { archive });
}

export async function pauseJob(): Promise<void> {
  return invoke("pause_job");
}

export async function stopJob(): Promise<void> {
  return invoke("stop_job");
}

export async function cancelJob(): Promise<void> {
  return invoke("cancel_job");
}

export async function currentJob(): Promise<string | null> {
  return invoke("current_job");
}

export async function listJobs(): Promise<JobListEntry[]> {
  return invoke("list_jobs");
}

export async function recoveryView(archive: string): Promise<RecoveryView> {
  return invoke("recovery_view", { archive });
}

export async function abandonJob(archive: string): Promise<void> {
  return invoke("abandon_job", { archive });
}

export async function getSettings(): Promise<SettingsDto> {
  return invoke("get_settings");
}

export async function setSettings(s: SettingsDto): Promise<void> {
  return invoke("set_settings", { settings: s });
}

export async function openLogsDir(): Promise<void> {
  return invoke("open_logs_dir");
}

export async function readLogs(last: number): Promise<string> {
  return invoke("read_logs", { last });
}

export async function pickArchive(): Promise<string | null> {
  const { open } = await import("@tauri-apps/plugin-dialog");
  const picked = await open({
    title: "Choose an archive",
    multiple: false,
    filters: [
      { name: "Archives", extensions: ["rar", "zip", "7z", "tar"] },
      { name: "All files", extensions: ["*"] },
    ],
  });
  if (typeof picked === "string") return picked;
  return null;
}

export async function pickDirectory(): Promise<string | null> {
  const { open } = await import("@tauri-apps/plugin-dialog");
  const picked = await open({ directory: true, multiple: false });
  if (typeof picked === "string") return picked;
  return null;
}

export function formatBytes(n: number): string {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = n;
  let u = 0;
  while (v >= 1024 && u < units.length - 1) {
    v /= 1024;
    u++;
  }
  return u === 0 ? `${n} B` : `${v.toFixed(1)} ${units[u]}`;
}

export function ratio(packed: number, unpacked: number): string {
  if (unpacked === 0) return "—";
  const r = (packed / unpacked) * 100;
  return `${r.toFixed(1)}%`;
}