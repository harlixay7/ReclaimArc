//! Windows Explorer Context Menu integration via SystemFileAssociations in HKCU.
//!
//! Enables "Analyze & Extract with ReclaimArc" in the Windows Explorer right-click
//! context menu for archive files without requiring administrator privileges or
//! overriding default double-click applications.

use std::path::Path;

use windows::core::PCWSTR;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegOpenKeyExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SZ,
};

use crate::error::{PlatformError, PlatformErrorKind};

/// Common archive file extensions supported for right-click Explorer integration.
pub const ARCHIVE_EXTENSIONS: &[&str] = &[
    ".zip", ".rar", ".7z", ".tar", ".gz", ".tgz", ".bz2", ".tbz2", ".xz", ".txz", ".zst",
];

/// Convert a string slice to null-terminated UTF-16 vector.
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Helper to set a string value on an open registry key.
unsafe fn set_reg_string(key: HKEY, name: Option<&str>, value: &str) -> Result<(), u32> {
    let name_w = name.map(to_wide);
    let name_ptr = name_w
        .as_ref()
        .map_or(PCWSTR::null(), |w| PCWSTR(w.as_ptr()));
    let val_bytes: Vec<u8> = value
        .encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(|u| u.to_le_bytes())
        .collect();

    let res = unsafe { RegSetValueExW(key, name_ptr, None, REG_SZ, Some(&val_bytes)) };
    if res.is_ok() {
        Ok(())
    } else {
        Err(res.0)
    }
}

/// Check whether the ReclaimArc context menu is registered in HKCU.
pub fn is_context_menu_enabled() -> bool {
    let path = "Software\\Classes\\SystemFileAssociations\\.zip\\shell\\ReclaimArc";
    let path_w = to_wide(path);
    let mut hkey = HKEY::default();
    let res = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(path_w.as_ptr()),
            None,
            KEY_READ,
            &mut hkey,
        )
    };
    if res.is_ok() {
        let _ = unsafe { RegCloseKey(hkey) };
        true
    } else {
        false
    }
}

/// Enable or disable the Windows Explorer right-click context menu for archives.
pub fn set_context_menu_enabled(enabled: bool) -> Result<(), PlatformError> {
    let current_exe = std::env::current_exe().map_err(|e| {
        PlatformError::from_io(PlatformErrorKind::Io, "get current_exe path", None, &e)
    })?;
    set_context_menu_for_exe(enabled, &current_exe)
}

/// Enable or disable the context menu for a specific executable path.
pub fn set_context_menu_for_exe(enabled: bool, exe_path: &Path) -> Result<(), PlatformError> {
    let exe_str = exe_path.to_string_lossy().into_owned();
    let menu_title = "Analyze & Extract with ReclaimArc";
    let command_str = format!("\"{exe_str}\" \"%1\"");

    for ext in ARCHIVE_EXTENSIONS {
        let base_subpath =
            format!("Software\\Classes\\SystemFileAssociations\\{ext}\\shell\\ReclaimArc");
        let base_subpath_w = to_wide(&base_subpath);

        if enabled {
            // 1. Create base key: HKCU\Software\Classes\SystemFileAssociations\<ext>\shell\ReclaimArc
            let mut base_key = HKEY::default();
            let res = unsafe {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    PCWSTR(base_subpath_w.as_ptr()),
                    None,
                    PCWSTR::null(),
                    windows::Win32::System::Registry::REG_OPEN_CREATE_OPTIONS(0),
                    KEY_WRITE,
                    None,
                    &mut base_key,
                    None,
                )
            };
            if let Err(e) = res.ok() {
                return Err(PlatformError::from_os(
                    PlatformErrorKind::Win32,
                    "RegCreateKeyExW (context menu base)",
                    None,
                    e.code().0 as u32,
                ));
            }

            // Set default label & icon
            let _ = unsafe { set_reg_string(base_key, None, menu_title) };
            let _ = unsafe { set_reg_string(base_key, Some("Icon"), &exe_str) };
            let _ = unsafe { RegCloseKey(base_key) };

            // 2. Create command subkey: ...\shell\ReclaimArc\command
            let cmd_subpath = format!("{base_subpath}\\command");
            let cmd_subpath_w = to_wide(&cmd_subpath);
            let mut cmd_key = HKEY::default();
            let res_cmd = unsafe {
                RegCreateKeyExW(
                    HKEY_CURRENT_USER,
                    PCWSTR(cmd_subpath_w.as_ptr()),
                    None,
                    PCWSTR::null(),
                    windows::Win32::System::Registry::REG_OPEN_CREATE_OPTIONS(0),
                    KEY_WRITE,
                    None,
                    &mut cmd_key,
                    None,
                )
            };
            if let Err(e) = res_cmd.ok() {
                return Err(PlatformError::from_os(
                    PlatformErrorKind::Win32,
                    "RegCreateKeyExW (context menu command)",
                    None,
                    e.code().0 as u32,
                ));
            }

            // Set default command string
            let _ = unsafe { set_reg_string(cmd_key, None, &command_str) };
            let _ = unsafe { RegCloseKey(cmd_key) };
        } else {
            // Delete tree under HKCU\Software\Classes\SystemFileAssociations\<ext>\shell\ReclaimArc
            let _ = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(base_subpath_w.as_ptr())) };
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_menu_toggle_roundtrip() {
        let fake_exe = Path::new("C:\\Program Files\\ReclaimArc\\reclaimarc-desktop.exe");

        // 1. Enable
        set_context_menu_for_exe(true, fake_exe).expect("enabling context menu must succeed");
        assert!(
            is_context_menu_enabled(),
            "context menu must be reported as enabled"
        );

        // 2. Disable
        set_context_menu_for_exe(false, fake_exe).expect("disabling context menu must succeed");
        assert!(
            !is_context_menu_enabled(),
            "context menu must be reported as disabled"
        );
    }
}
