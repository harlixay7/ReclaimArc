//! ReclaimArc command-line interface.
//!
//! The CLI drives the same engine as the desktop app.
//!
//! Subcommands:
//!   inspect <archive>                 Show archive structure and capabilities
//!   plan <archive> <destination>      Simulate space requirements
//!   extract <archive> <destination>   Extract (--low-space for destructive)
//!   jobs                              List interrupted jobs
//!   resume <journal-or-archive>       Resume an interrupted job
//!   abandon <job-id|archive>          Discard a job (source data is gone)
//!   diagnostics <job>                 Dump the recovery report

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

use reclaimarc_core::{
    abandon_job, discover_interrupted_jobs, Engine, EngineConfig, Event, ExtractionMode, JobHandle,
    JobOutcome, SafetyMode,
};

fn main() {
    // Ensure UTF-8 console output on Windows (CP65001).
    let _ = unsafe { windows::Win32::System::Console::SetConsoleOutputCP(65001) };
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("help");
    let result = match cmd {
        "inspect" => cmd_inspect(&args[2..]),
        "plan" => cmd_plan(&args[2..]),
        "extract" => cmd_extract(&args[2..]),
        "jobs" => cmd_jobs(),
        "resume" => cmd_resume(&args[2..]),
        "abandon" => cmd_abandon(&args[2..]),
        "diagnostics" => cmd_diagnostics(&args[2..]),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        other => {
            eprintln!("unknown command '{other}'");
            print_help();
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn print_help() {
    println!(
        "ReclaimArc — low-space archive extraction\n\
         \n\
         USAGE:\n\
         \x20 reclaimarc <command> [options]\n\
         \n\
         COMMANDS:\n\
         \x20 inspect <archive>                 Show archive structure, recovery units, capabilities\n\
         \x20 plan <archive> <destination>      Simulate extraction and report feasibility\n\
         \x20 extract <archive> <destination>   Extract (normal)\n\
         \x20 extract --low-space <archive> <destination>   Extract destructively, reclaiming source\n\
         \x20 jobs                              List interrupted jobs\n\
         \x20 resume <archive>                  Resume the interrupted job for an archive\n\
         \x20 abandon <archive>                 Discard the job (reclaimed source cannot be restored)\n\
         \x20 diagnostics <archive>             Dump the recovery report\n\
         \n\
         GLOBAL OPTIONS:\n\
         \x20 --password <pwd>   password for encrypted archives\n\
         \x20 --mode <safe|balanced|maximum-space>   safety preset (default balanced)\n\
         \x20 --delete-source    auto-delete source archive on 100% verified completion (default: enabled)\n\
         \x20 --keep-source      preserve source archive shells after extraction\n\
         \x20 --yes               skip confirmations"
    );
}

fn parse_password(args: &[String]) -> Option<String> {
    args.iter()
        .position(|a| a == "--password")
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn parse_mode(args: &[String], config: &mut EngineConfig) {
    if let Some(i) = args.iter().position(|a| a == "--mode") {
        if let Some(m) = args.get(i + 1).and_then(|s| SafetyMode::from_str(s)) {
            config.safety_mode = m;
        } else {
            eprintln!("warning: unknown --mode (use safe|balanced|maximum-space)");
        }
    }
}

fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn cmd_inspect(args: &[String]) -> Result<(), String> {
    let archive = PathBuf::from(args.first().ok_or("usage: inspect <archive>")?);
    let password = parse_password(args);
    let mut backend = reclaimarc_archive::backend_for(&archive).map_err(|e| e.to_string())?;
    let info = backend
        .inspect(&reclaimarc_archive::OpenOptions { password })
        .map_err(|e| e.to_string())?;

    println!("Archive: {}", archive.display());
    let is_rar = info.format.starts_with("rar");
    let summary_solid = if is_rar {
        if info.solid_archive {
            " · Solid"
        } else {
            " · Non-solid"
        }
    } else {
        ""
    };
    println!(
        "{} · {} packed · {} unpacked{}",
        info.format.to_uppercase(),
        format_bytes(info.packed_size),
        format_bytes(info.unpacked_size),
        summary_solid
    );
    println!("Volumes: {}", info.volumes.len());
    println!("Entries: {}", info.entries.len());
    println!(
        "Recovery units: {} ({} = largest, {} unpacked)",
        info.recovery_units.len(),
        format_bytes(
            info.recovery_units
                .iter()
                .map(|u| u.unpacked_bytes)
                .max()
                .unwrap_or(0)
        ),
        format_bytes(
            info.recovery_units
                .iter()
                .map(|u| u.unpacked_bytes)
                .sum::<u64>()
        )
    );
    if is_rar {
        println!(
            "\n{:<6} {:<40} {:>10} {:>10}  Solid",
            "Unit", "Name", "Packed", "Size"
        );
        for e in &info.entries {
            let unit = info
                .recovery_units
                .iter()
                .find(|u| e.index >= u.first_entry && e.index <= u.last_entry)
                .map(|u| u.seq.to_string())
                .unwrap_or_else(|| "-".into());
            let name = if e.name.len() > 40 {
                format!("{}…", &e.name[..39])
            } else {
                e.name.clone()
            };
            println!(
                "{:<6} {:<40} {:>10} {:>10}  {}",
                unit,
                name,
                format_bytes(e.packed_size),
                format_bytes(e.unpacked_size),
                if e.is_solid { "solid" } else { "" }
            );
        }
    } else {
        println!(
            "\n{:<6} {:<40} {:>10} {:>10}  CRC32",
            "Unit", "Name", "Packed", "Size"
        );
        for e in &info.entries {
            let unit = info
                .recovery_units
                .iter()
                .find(|u| e.index >= u.first_entry && e.index <= u.last_entry)
                .map(|u| u.seq.to_string())
                .unwrap_or_else(|| "-".into());
            let name = if e.name.len() > 40 {
                format!("{}…", &e.name[..39])
            } else {
                e.name.clone()
            };
            let crc_str = e
                .crc32
                .map(|c| format!("0x{c:08X}"))
                .unwrap_or_else(|| "—".into());
            println!(
                "{:<6} {:<40} {:>10} {:>10}  {}",
                unit,
                name,
                format_bytes(e.packed_size),
                format_bytes(e.unpacked_size),
                crc_str
            );
        }
    }
    println!("\nCapabilities:");
    println!("  format: {}", info.capability.format);
    println!(
        "  test_integrity: {}",
        info.capability.supports_test_integrity
    );
    println!("  restartable_units: {}", info.capability.restartable_units);
    println!(
        "  progressive_reclaim: {}",
        info.capability.progressive_reclaim
    );
    for note in &info.capability.notes {
        println!("  note: {note}");
    }
    Ok(())
}

fn cmd_plan(args: &[String]) -> Result<(), String> {
    let archive = PathBuf::from(args.first().ok_or("usage: plan <archive> <destination>")?);
    let destination = PathBuf::from(args.get(1).ok_or("usage: plan <archive> <destination>")?);
    let password = parse_password(args);
    let mut config = EngineConfig::default();
    parse_mode(args, &mut config);
    let engine = Engine::new(config);
    let (info, plan) = engine
        .analyze(&archive, &destination, password)
        .map_err(|e| e.to_string())?;

    println!("Space plan for: {}", archive.display());
    println!("Destination: {}", destination.display());
    println!(
        "  Free now:                    {}",
        format_bytes(plan.free_now)
    );
    println!(
        "  Normal extraction requirement: {}",
        format_bytes(plan.normal_requirement())
    );
    println!(
        "  Total unpacked:              {}",
        format_bytes(plan.unpacked_total)
    );
    println!(
        "  Progressive peak requirement: {}",
        format_bytes(plan.progressive_peak_requirement)
    );
    println!(
        "  Safety reserve:              {}",
        format_bytes(plan.reserve)
    );
    println!(
        "  Largest recovery unit:       {}",
        format_bytes(plan.largest_unit_bytes)
    );
    println!(
        "  Estimated source reclaim:    {}",
        format_bytes(plan.estimated_source_reclaim)
    );
    println!(
        "  Archive packed (logical):    {}",
        format_bytes(info.packed_size)
    );
    if plan.progressive_feasible {
        println!(
            "\nNormal extraction:      {}",
            if plan.normal_feasible {
                "POSSIBLE"
            } else {
                "IMPOSSIBLE"
            }
        );
        println!("Progressive extraction: POSSIBLE");
    } else {
        println!("\nProgressive extraction: NOT SAFE");
        if let Some(reason) = &plan.reason {
            println!("Reason: {reason}");
        }
    }
    Ok(())
}

fn cmd_extract(args: &[String]) -> Result<(), String> {
    let low_space = args.iter().any(|a| a == "--low-space");
    let yes = args.iter().any(|a| a == "--yes");
    let positional: Vec<String> = args
        .iter()
        .filter(|a| !a.starts_with("--"))
        .cloned()
        .collect();
    let archive = PathBuf::from(
        positional
            .first()
            .ok_or("usage: extract <archive> <destination>")?,
    );
    let destination = PathBuf::from(
        positional
            .get(1)
            .ok_or("usage: extract <archive> <destination>")?,
    );
    let password = parse_password(args);
    let mut config = EngineConfig::default();
    parse_mode(args, &mut config);
    if args
        .iter()
        .any(|a| a == "--keep-source" || a == "--keep-shells" || a == "--no-delete-source")
    {
        config.delete_shells_on_completion = false;
    }
    if args
        .iter()
        .any(|a| a == "--delete-source" || a == "--delete-shells")
    {
        config.delete_shells_on_completion = true;
    }

    if low_space && !yes {
        println!(
            "Low-Space extraction PROGRESSIVELY DESTROYS verified portions of the source archive\n\
             to reclaim space. Previously reclaimed source data cannot be restored.\n\
             Continue? [y/N]"
        );
        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .map_err(|e| e.to_string())?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("aborted");
            return Ok(());
        }
    }

    let (tx, rx) = mpsc::channel();
    let mut engine = Engine::new(config);
    let (handle, mut job) = engine
        .start_job(
            &archive,
            &destination,
            if low_space {
                ExtractionMode::LowSpace
            } else {
                ExtractionMode::Normal
            },
            password,
            tx,
        )
        .map_err(|e| e.to_string())?;

    let handle_ui = handle.clone();
    let ui_thread = std::thread::spawn(move || {
        while let Ok(event) = rx.recv() {
            match event {
                Event::JobStarted { .. } => println!("job started"),
                Event::PreTestStarted { bytes_total } => {
                    print!("\rtesting integrity: 0.0% of {}", format_bytes(bytes_total));
                    use std::io::Write;
                    std::io::stdout().flush().ok();
                }
                Event::PreTestProgress { current, total } => {
                    if total > 0 {
                        let pct = current as f64 * 100.0 / total as f64;
                        print!("\rtesting integrity: {pct:.1}% of {}", format_bytes(total));
                        use std::io::Write;
                        std::io::stdout().flush().ok();
                    }
                }
                Event::PreTestFinished { ok, .. } => {
                    println!("\rintegrity test: {}", if ok { "OK" } else { "FAILED" });
                }
                Event::UnitStarted { seq, .. } => {
                    println!("[unit {seq}] extracting…");
                }
                Event::EntryProgress {
                    index,
                    current,
                    total,
                } => {
                    if total > 0 {
                        let pct = current as f64 * 100.0 / total as f64;
                        print!(
                            "\r  entry {index}: {pct:.1}% ({}/{})",
                            format_bytes(current),
                            format_bytes(total)
                        );
                        use std::io::Write;
                        std::io::stdout().flush().ok();
                    }
                }
                Event::EntryCommitted { index, path } => {
                    println!("\r  entry {index} committed: {}", path.display());
                }
                Event::UnitCommitted { seq, bytes } => {
                    println!("[unit {seq}] committed ({})", format_bytes(bytes));
                }
                Event::RangeReclaimed {
                    volume_index,
                    bytes,
                } => {
                    println!(
                        "  source reclaimed: vol {volume_index} -{}",
                        format_bytes(bytes)
                    );
                }
                Event::UnitReclaimed { seq, bytes } => {
                    println!("[unit {seq}] source reclaimed ({})", format_bytes(bytes));
                }
                Event::FreeSpace { bytes } => println!("  free space: {}", format_bytes(bytes)),
                Event::JobPaused { .. } => {
                    println!("job paused (resume with: reclaimarc resume <archive>)")
                }
                Event::JobCancelled { .. } => println!("job cancelled (resumable)"),
                Event::JobFinished {
                    committed_bytes,
                    reclaimed_bytes,
                    ..
                } => {
                    println!(
                        "job finished: {} committed, {} source reclaimed",
                        format_bytes(committed_bytes),
                        format_bytes(reclaimed_bytes)
                    );
                }
                Event::JobFailed {
                    message,
                    recommended_action,
                    ..
                } => {
                    eprintln!("job failed: {message}");
                    eprintln!("recommended: {recommended_action}");
                }
                Event::LowSpace { free, reserve } => {
                    println!(
                        "warning: free space {} below comfortable reserve {} — pausing soon",
                        format_bytes(free),
                        format_bytes(reserve)
                    );
                }
                _ => {}
            }
        }
    });

    // Allow Ctrl+C to pause safely.
    let pause_flag = handle_ui.pause.clone();
    ctrlc::set_handler(move || {
        eprintln!("\nSIGINT: pausing safely after the current unit…");
        pause_flag.store(true, Ordering::SeqCst);
    })
    .map_err(|e| format!("cannot install Ctrl+C handler: {e}"))?;

    let outcome = engine
        .run_job(&mut job, &handle_ui)
        .map_err(|e| e.to_string())?;
    drop(job);
    let _ = rx;
    ui_thread.join().map_err(|_| "ui thread panicked")?;
    match outcome {
        JobOutcome::Completed { .. } => Ok(()),
        JobOutcome::Paused => Err("paused — run `reclaimarc resume <archive>` to continue".into()),
        JobOutcome::Cancelled => {
            Err("cancelled — run `reclaimarc resume <archive>` to continue".into())
        }
        JobOutcome::Failed { failure } => Err(format!(
            "{} — {}",
            failure.message, failure.recommended_action
        )),
    }
}

fn cmd_jobs() -> Result<(), String> {
    let jobs = discover_interrupted_jobs().map_err(|e| e.to_string())?;
    if jobs.is_empty() {
        println!("no interrupted jobs found");
        return Ok(());
    }
    println!("{:<38} {:<40} {:<40}", "Job", "Archive", "Destination");
    for j in jobs {
        println!(
            "{:<38} {:<40} {:<40}",
            &j.job_id[..j.job_id.len().min(38)],
            j.archive.to_string_lossy(),
            j.destination.to_string_lossy()
        );
    }
    Ok(())
}

fn cmd_resume(args: &[String]) -> Result<(), String> {
    let mut archive: Option<PathBuf> = None;
    let mut password: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--password" | "-p" => {
                i += 1;
                password = args.get(i).cloned();
            }
            s if !s.starts_with('-') && archive.is_none() => {
                archive = Some(PathBuf::from(s));
            }
            _ => {}
        }
        i += 1;
    }
    let archive = archive.ok_or("usage: resume <archive> [--password <pass>]")?;
    let journal_path = find_journal_for(&archive)?;
    let (tx, rx) = mpsc::channel();
    let mut engine = Engine::new(EngineConfig::default());
    let (handle, mut job) = engine
        .resume_job(&journal_path, password, tx)
        .map_err(|e| e.to_string())?;
    println!(
        "resuming job {} → {}",
        job.job_id,
        job.destination.display()
    );
    let handle_ui = handle.clone();
    let ui_thread = std::thread::spawn(move || {
        while let Ok(event) = rx.recv() {
            match event {
                Event::UnitStarted { seq, .. } => println!("[unit {seq}] extracting…"),
                Event::UnitCommitted { seq, bytes } => {
                    println!("[unit {seq}] committed ({})", format_bytes(bytes))
                }
                Event::UnitReclaimed { seq, bytes } => {
                    println!("[unit {seq}] source reclaimed ({})", format_bytes(bytes))
                }
                Event::EntryCommitted { index, path } => {
                    println!("  entry {index}: {}", path.display())
                }
                Event::JobPaused { .. } => println!("job paused"),
                Event::JobFinished {
                    committed_bytes,
                    reclaimed_bytes,
                    ..
                } => {
                    println!(
                        "job finished: {} committed, {} source reclaimed",
                        format_bytes(committed_bytes),
                        format_bytes(reclaimed_bytes)
                    );
                }
                Event::JobFailed {
                    message,
                    recommended_action,
                    ..
                } => {
                    eprintln!("job failed: {message}");
                    eprintln!("recommended: {recommended_action}");
                }
                _ => {}
            }
        }
    });
    let outcome = engine
        .run_job(&mut job, &handle_ui)
        .map_err(|e| e.to_string())?;
    drop(job);
    let _ = rx;
    ui_thread.join().map_err(|_| "ui thread panicked")?;
    match outcome {
        JobOutcome::Completed { .. } => Ok(()),
        JobOutcome::Paused => {
            Err("paused again — run `reclaimarc resume <archive>` to continue".into())
        }
        JobOutcome::Cancelled => {
            Err("cancelled — run `reclaimarc resume <archive>` to continue".into())
        }
        JobOutcome::Failed { failure } => Err(format!(
            "{} — {}",
            failure.message, failure.recommended_action
        )),
    }
}

fn cmd_abandon(args: &[String]) -> Result<(), String> {
    let archive = PathBuf::from(args.first().ok_or("usage: abandon <archive>")?);
    let journal_path = find_journal_for(&archive)?;
    let job_id = {
        let journal =
            reclaimarc_journal::JobJournal::open(&journal_path).map_err(|e| e.to_string())?;
        journal.job_meta().map_err(|e| e.to_string())?.job_id
    };
    println!(
        "Abandoning job {job_id}. Previously reclaimed source data CANNOT be restored.\n\
         Continue? [y/N]"
    );
    let mut input = String::new();
    std::io::stdin()
        .read_line(&mut input)
        .map_err(|e| e.to_string())?;
    if !input.trim().eq_ignore_ascii_case("y") {
        println!("aborted");
        return Ok(());
    }
    abandon_job(&journal_path, &job_id).map_err(|e| e.to_string())?;
    println!("job abandoned");
    Ok(())
}

fn cmd_diagnostics(args: &[String]) -> Result<(), String> {
    let archive = PathBuf::from(args.first().ok_or("usage: diagnostics <archive>")?);
    let journal_path = find_journal_for(&archive)?;
    let journal = reclaimarc_journal::JobJournal::open(&journal_path).map_err(|e| e.to_string())?;
    let summary = reclaimarc_core::summarize(&journal).map_err(|e| e.to_string())?;
    println!("Recovery report for job {}", summary.job_id);
    println!("  archive:      {}", summary.archive.display());
    println!("  destination:  {}", summary.destination.display());
    println!("  job state:    {:?}", summary.job_state);
    println!(
        "  committed:    {}",
        format_bytes(summary.committed_output_bytes)
    );
    println!(
        "  reclaimed:    {}",
        format_bytes(summary.source_reclaimed_bytes)
    );
    println!(
        "  remaining:    {}",
        format_bytes(summary.remaining_source_bytes)
    );
    println!("  checkpoint:   {}", summary.last_checkpoint);
    println!("\nUnits:");
    for (seq, state) in &summary.units {
        println!("  unit {seq}: {state:?}");
    }
    if !summary.errors.is_empty() {
        println!("\nRecorded errors:");
        for e in &summary.errors {
            println!("  {e}");
        }
    }
    Ok(())
}

/// Find the newest journal for an archive (beside it, in .reclaimarc/).
fn find_journal_for(archive: &std::path::Path) -> Result<PathBuf, String> {
    let state = archive
        .parent()
        .map(|p| p.join(".reclaimarc"))
        .ok_or_else(|| "archive has no parent directory".to_string())?;
    if !state.exists() {
        return Err(format!(
            "no ReclaimArc state found beside '{}' (nothing interrupted?)",
            archive.display()
        ));
    }
    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&state)
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| e.path().join("job.db"))
        .filter(|p| p.exists())
        .filter_map(|p| {
            std::fs::metadata(&p)
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|t| (t, p))
        })
        .collect();
    candidates.sort_by_key(|(t, _)| *t);
    candidates
        .last()
        .map(|(_, p)| p.clone())
        .ok_or_else(|| format!("no journal found beside '{}'", archive.display()))
}

#[allow(dead_code)]
fn _assert_send(_: &JobHandle) {}
#[allow(dead_code)]
fn _assert_send2(_: &Arc<AtomicBool>) {}
