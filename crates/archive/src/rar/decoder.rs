//! FFI wrapper around the official UnRAR library (`unrar_sys`, which vendors
//! the official RARLab source).
//!
//! This module owns all `unsafe` calls into the C library. The license
//! boundary is explicit: the vendored UnRAR source keeps its own license
//! (`unrar_sys/vendor/unrar/license.txt`) and is never re-licensed.
//!
//! Safety model:
//! - The library is driven strictly sequentially from the first volume.
//! - Entries before the current unit are processed with `RAR_SKIP`, which the
//!   library implements as a seek in non-solid archives â€” it never reads
//!   reclaimed (zeroed) data.
//! - Callbacks run on the same thread as the caller; the context is a
//!   `Box<CallbackCtx>` leaked to C and freed on close.

use std::ffi::c_int;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::ArchiveError;
use crate::model::ProgressEvent;

use unrar_sys::{
    Handle, HeaderDataEx, OpenArchiveDataEx, LPARAM, RARCloseArchive, RAROpenArchiveEx,
    RARProcessFileW, RARReadHeaderEx, UINT, UCM_CHANGEVOLUMEW, UCM_NEEDPASSWORDW, UCM_PROCESSDATA,
    ERAR_BAD_ARCHIVE, ERAR_BAD_DATA, ERAR_BAD_PASSWORD, ERAR_ECREATE, ERAR_END_ARCHIVE, ERAR_EOPEN,
    ERAR_EREAD, ERAR_EREFERENCE, ERAR_EWRITE, ERAR_MISSING_PASSWORD, ERAR_UNKNOWN, ERAR_UNKNOWN_FORMAT,
    RAR_EXTRACT, RAR_OM_EXTRACT, RAR_OM_LIST_INCSPLIT, RAR_SKIP, RAR_TEST,
};

/// One header as reported by the C library (ground truth for validation).
#[derive(Debug, Clone)]
pub struct RawHeader {
    pub file_name_w: String,
    pub flags: u32,
    pub pack_size: u64,
    pub unp_size: u64,
    pub file_crc: u32,
    pub method: u32,
}

/// Open mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    /// List entries (no data processing; reports split-before files too).
    List,
    /// Process entries (extract/test/skip).
    Process,
}

/// Operation for `process_file`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Skip,
    Test,
    Extract,
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Context shared with the C callback. Lives on the caller's thread only.
///
/// The progress pointer is only dereferenced while `process_file` is running
/// (it is cleared afterwards), so the lifetime of the caller's closure is
/// erased at the boundary and restored on use.
struct CallbackCtx {
    cancel: Option<Arc<AtomicBool>>,
    progress: Option<*mut dyn FnMut(ProgressEvent) -> bool>,
    current_entry: u64,
    current_entry_total: u64,
    current_entry_bytes: u64,
    password: Option<String>,
    /// Volume names as the library opens them (diagnostics / journaling).
    opened_volumes: Vec<String>,
}

// The callback context is only touched from the thread that drives the
// library, so these markers are safe.
unsafe impl Send for CallbackCtx {}
unsafe impl Sync for CallbackCtx {}

extern "C" fn unrar_callback(msg: UINT, user_data: LPARAM, p1: LPARAM, p2: LPARAM) -> c_int {
    // SAFETY: the context is a leaked Box that lives until the handle closes,
    // and the callback is invoked synchronously on the driving thread.
    let ctx: &mut CallbackCtx = unsafe { &mut *(user_data as *mut CallbackCtx) };
    match msg {
        UCM_PROCESSDATA => {
            // p1 = unpacked data buffer, p2 = byte count.
            if let Some(cancel) = &ctx.cancel {
                if cancel.load(Ordering::Relaxed) {
                    return -1;
                }
            }
            if let Some(progress) = ctx.progress.as_mut() {
                ctx.current_entry_bytes = ctx.current_entry_bytes.saturating_add(p2 as u64);
                // SAFETY: the raw pointer was created from a valid
                // `&mut dyn FnMut` whose lifetime is the enclosing call.
                let f: &mut dyn FnMut(ProgressEvent) -> bool = unsafe { &mut **progress };
                if !f(ProgressEvent::EntryProgress {
                    entry_index: ctx.current_entry,
                    current: ctx.current_entry_bytes,
                    total: ctx.current_entry_total,
                }) {
                    return -1;
                }
            }
            0
        }
        UCM_NEEDPASSWORDW => {
            // p1 = wide password buffer, p2 = capacity in elements.
            if let Some(pwd) = &ctx.password {
                let cap = p2 as usize;
                if cap == 0 {
                    return -1;
                }
                let buf = unsafe { std::slice::from_raw_parts_mut(p1 as *mut u16, cap) };
                let wide: Vec<u16> = pwd.encode_utf16().collect();
                let n = wide.len().min(cap.saturating_sub(1));
                buf[..n].copy_from_slice(&wide[..n]);
                buf[n] = 0;
                0
            } else {
                -1
            }
        }
        UCM_CHANGEVOLUMEW => {
            // p1 = next volume name (wide, NUL-terminated). Accept the name.
            if p1 != 0 {
                let name = unsafe { std::slice::from_raw_parts(p1 as *const u16, 4096) };
                let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
                let vol = String::from_utf16_lossy(&name[..end]);
                if !vol.is_empty() && !ctx.opened_volumes.iter().any(|v| *v == vol) {
                    ctx.opened_volumes.push(vol);
                }
            }
            0
        }
        _ => 0,
    }
}

/// Open archive handle.
pub struct Unrar {
    handle: *const Handle,
    ctx: *mut CallbackCtx,
    open_mode: u32,
}

// The handle is only used from one thread at a time.
unsafe impl Send for Unrar {}

impl Unrar {
    /// Open the first volume of an archive.
    ///
    /// `password` is kept in memory only (never persisted, zeroed on drop).
    pub fn open(
        first_volume: &Path,
        mode: OpenMode,
        password: Option<String>,
        cancel: Option<Arc<AtomicBool>>,
    ) -> Result<Unrar, ArchiveError> {
        let ctx = Box::new(CallbackCtx {
            cancel,
            progress: None,
            current_entry: 0,
            current_entry_total: 0,
            current_entry_bytes: 0,
            password,
            opened_volumes: vec![first_volume.to_string_lossy().into_owned()],
        });
        let ctx_ptr = Box::into_raw(ctx);

        let wide_name = to_wide(&first_volume.to_string_lossy());
        let mut data = OpenArchiveDataEx::new(wide_name.as_ptr(), if mode == OpenMode::List {
            RAR_OM_LIST_INCSPLIT
        } else {
            RAR_OM_EXTRACT
        });
        data.callback = Some(unrar_callback);
        data.user_data = ctx_ptr as LPARAM;

        let handle = unsafe { RAROpenArchiveEx(&data) };
        if handle.is_null() {
            let code = data.open_result as c_int;
            let _ = unsafe { Box::from_raw(ctx_ptr) };
            return Err(map_open_error(code, first_volume));
        }

        Ok(Unrar {
            handle,
            ctx: ctx_ptr,
            open_mode: if mode == OpenMode::List {
                RAR_OM_LIST_INCSPLIT
            } else {
                RAR_OM_EXTRACT
            }
        })
    }

/// Read the next file header. Returns `Ok(None)` at end of archive.
    pub fn read_header(&mut self) -> Result<Option<RawHeader>, ArchiveError> {
        let mut h = HeaderDataEx::default();
        let code = unsafe { RARReadHeaderEx(self.handle, &mut h) };
                if code == ERAR_END_ARCHIVE {
            return Ok(None);
        }
        if code != 0 {
            return Err(map_process_error(code, "read header"));
        }
        let name = read_wide(&h.filename_w);
        let pack = ((h.pack_size_high as u64) << 32) | h.pack_size as u64;
        let unp = ((h.unp_size_high as u64) << 32) | h.unp_size as u64;
        Ok(Some(RawHeader {
            file_name_w: name,
            flags: h.flags,
            pack_size: pack,
            unp_size: unp,
            file_crc: h.file_crc,
            method: h.method,
        }))
    }

    /// Process the current file (the one whose header was just read).
    ///
    /// `dest_path` / `dest_name`: wide strings; pass `None` to let the library
    /// use its defaults. `dest_name` replaces the output path entirely and
    /// disables the library's own path processing (the caller guarantees the
    /// path is safe â€” this is the documented contract of the DLL).
    pub fn process_file<'p, 'c>(
        &mut self,
        operation: Operation,
        dest_path: Option<&str>,
        dest_name: Option<&str>,
        progress: Option<&'p mut (dyn FnMut(ProgressEvent) -> bool + 'c)>,
        entry_index: u64,
        entry_total: u64,
    ) -> Result<(), ArchiveError> {
        let ctx = unsafe { &mut *self.ctx };
        ctx.current_entry = entry_index;
        ctx.current_entry_total = entry_total;
        ctx.current_entry_bytes = 0;
        ctx.progress = progress.map(|f| {
            // SAFETY: the lifetime of the caller's closure is erased here and
            // restored when the callback dereferences it. The pointer is only
            // valid while process_file is executing (cleared below).
            let erased: *mut dyn FnMut(ProgressEvent) -> bool = unsafe {
                std::mem::transmute::<*mut dyn FnMut(ProgressEvent) -> bool, _>(f)
            };
            erased
        });

        let op = match operation {
            Operation::Skip => RAR_SKIP,
            Operation::Test => RAR_TEST,
            Operation::Extract => RAR_EXTRACT,
        };
        let path_w = dest_path.map(|s| to_wide(s));
        let name_w = dest_name.map(|s| to_wide(s));
        let code = unsafe {
            RARProcessFileW(
                self.handle,
                op,
                path_w.as_ref().map(|v| v.as_ptr()).unwrap_or(std::ptr::null()),
                name_w.as_ref().map(|v| v.as_ptr()).unwrap_or(std::ptr::null()),
            )
        };

        let ctx = unsafe { &mut *self.ctx };
        ctx.progress = None;

        if code == 0 {
            return Ok(());
        }
        // The DLL reports a callback abort (our progress/pause callback
        // returning false) as ERAR_UNKNOWN (RARX_USERBREAK has no dedicated
        // DLL code). Treat it as cancellation.
        if code == ERAR_UNKNOWN {
            return Err(ArchiveError::Cancelled);
        }
        Err(map_process_error(code, "process file"))
    }

    /// The wide names of volumes the library actually opened.
    pub fn opened_volumes(&self) -> Vec<String> {
        let ctx = unsafe { &*self.ctx };
        ctx.opened_volumes.clone()
    }

    /// The open mode.
    pub fn is_process_mode(&self) -> bool {
        self.open_mode == RAR_OM_EXTRACT
    }
}

impl Drop for Unrar {
    fn drop(&mut self) {
        unsafe {
            RARCloseArchive(self.handle);
            let ctx = Box::from_raw(self.ctx);
            // Zero the password in memory when the handle closes.
            if let Some(pwd) = ctx.password {
                let mut bytes = pwd.into_bytes();
                bytes.iter_mut().for_each(|b| *b = 0);
            }
        }
    }
}

fn read_wide(arr: &[u16]) -> String {
    let end = arr.iter().position(|&c| c == 0).unwrap_or(arr.len());
    String::from_utf16_lossy(&arr[..end])
}

fn map_open_error(code: c_int, path: &Path) -> ArchiveError {
    match code {
        ERAR_BAD_PASSWORD | ERAR_MISSING_PASSWORD => {
            ArchiveError::Password("archive requires a password".into())
        }
        _ => ArchiveError::open(format!("cannot open archive '{}' (code {code})", path.display())),
    }
}

fn map_process_error(code: c_int, op: &str) -> ArchiveError {
    match code {
        ERAR_END_ARCHIVE => ArchiveError::open(format!("{op}: unexpected end of archive")),
        ERAR_BAD_DATA => ArchiveError::corrupt(format!("{op}: data or checksum error")),
        ERAR_BAD_ARCHIVE => ArchiveError::corrupt(format!("{op}: bad archive")),
        ERAR_UNKNOWN_FORMAT => ArchiveError::unsupported(format!("{op}: unknown format")),
        ERAR_EOPEN => ArchiveError::missing_volume(format!("{op}: could not open a volume")),
        ERAR_ECREATE => ArchiveError::open(format!("{op}: could not create output file")),
        ERAR_EREAD => ArchiveError::corrupt(format!("{op}: read error")),
        ERAR_EWRITE => ArchiveError::open(format!("{op}: write error")),
        ERAR_MISSING_PASSWORD | ERAR_BAD_PASSWORD => {
            ArchiveError::Password(format!("{op}: password missing or incorrect"))
        }
        ERAR_EREFERENCE => ArchiveError::unsupported(format!("{op}: reference record unsupported")),
        other => ArchiveError::Decoder(format!("{op}: unrar error {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_conversion_roundtrip() {
        let s = "hÃ©llo wÃ¶rld";
        let v = to_wide(s);
        let back = String::from_utf16_lossy(&v[..v.len() - 1]);
        assert_eq!(back, s);
    }
}

