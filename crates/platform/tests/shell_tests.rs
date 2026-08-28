use reclaimarc_platform::shell::{
    is_context_menu_enabled, set_context_menu_for_exe, ARCHIVE_EXTENSIONS,
};
use std::path::Path;

#[test]
fn test_context_menu_integration_all_extensions() {
    let dummy_exe = Path::new("C:\\Program Files\\ReclaimArc\\reclaimarc-desktop.exe");

    // 1. Enable context menu
    set_context_menu_for_exe(true, dummy_exe).expect("set_context_menu_for_exe(true) must succeed");

    assert!(
        is_context_menu_enabled(),
        "is_context_menu_enabled must return true after enabling"
    );

    // Verify each extension is registered in HKCU
    for ext in ARCHIVE_EXTENSIONS {
        let path = format!("Software\\Classes\\SystemFileAssociations\\{ext}\\shell\\ReclaimArc");
        let path_w: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let mut hkey = windows::Win32::System::Registry::HKEY::default();
        let res = unsafe {
            windows::Win32::System::Registry::RegOpenKeyExW(
                windows::Win32::System::Registry::HKEY_CURRENT_USER,
                windows::core::PCWSTR(path_w.as_ptr()),
                None,
                windows::Win32::System::Registry::KEY_READ,
                &mut hkey,
            )
        };
        assert!(
            res.is_ok(),
            "Extension key for '{ext}' must exist in HKCU: {path}"
        );
        let _ = unsafe { windows::Win32::System::Registry::RegCloseKey(hkey) };
    }

    // 2. Disable context menu
    set_context_menu_for_exe(false, dummy_exe)
        .expect("set_context_menu_for_exe(false) must succeed");

    assert!(
        !is_context_menu_enabled(),
        "is_context_menu_enabled must return false after disabling"
    );

    // Verify each extension is removed from HKCU
    for ext in ARCHIVE_EXTENSIONS {
        let path = format!("Software\\Classes\\SystemFileAssociations\\{ext}\\shell\\ReclaimArc");
        let path_w: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let mut hkey = windows::Win32::System::Registry::HKEY::default();
        let res = unsafe {
            windows::Win32::System::Registry::RegOpenKeyExW(
                windows::Win32::System::Registry::HKEY_CURRENT_USER,
                windows::core::PCWSTR(path_w.as_ptr()),
                None,
                windows::Win32::System::Registry::KEY_READ,
                &mut hkey,
            )
        };
        assert!(
            res.is_err(),
            "Extension key for '{ext}' must be deleted from HKCU: {path}"
        );
    }
}
