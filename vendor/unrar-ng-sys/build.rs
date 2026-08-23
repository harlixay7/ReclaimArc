fn main() {
    // Watch the whole vendored UnRAR tree so additions/removals — and edits
    // to files outside the `#include` graph that `cc` already tracks via
    // its own per-source `rerun-if-changed` emissions — trigger a rebuild.
    // (Cargo automatically tracks `build.rs` itself, so listing it here is
    // unnecessary.) Without this directive, an upstream upgrade that adds
    // a new `.cpp` we forget to register, or a `.hpp` not yet picked up by
    // any compiled translation unit, would silently use stale object files.
    println!("cargo:rerun-if-changed=vendor/unrar");

    if cfg!(windows) {
        println!("cargo:rustc-flags=-lpowrprof");
        println!("cargo:rustc-link-lib=shell32");
        println!("cargo:rustc-link-lib=advapi32"); 
        if cfg!(target_env = "gnu") {
            println!("cargo:rustc-link-lib=pthread");
        }
    } else {
        println!("cargo:rustc-link-lib=pthread");
    }
    let files: Vec<String> = [
        "strlist",
        "strfn",
        "pathfn",
        "smallfn",
        "global",
        "file",
        "filefn",
        "filcreat",
        "archive",
        "arcread",
        "unicode",
        "system",
        #[cfg(windows)]
        "isnt",
        "crypt",
        "crc",
        "rawread",
        "encname",
        "match",
        "timefn",
        "rdwrfn",
        "consio",
        "options",
        "errhnd",
        "rarvm",
        "secpassword",
        "rijndael",
        "getbits",
        "sha1",
        "sha256",
        "blake2s",
        "hash",
        "extinfo",
        "extract",
        "volume",
        "list",
        "find",
        "unpack",
        "headers",
        "threadpool",
        "rs16",
        "cmddata",
        "ui",
        "filestr",
        "scantree",
        "dll",
        "qopen",
        "largepage",  // New in unrar 7.x for large page memory allocation
        #[cfg(windows)]
        "motw",       // New in unrar 7.x for Mark of the Web support (Windows only)
    ].iter().map(|&s| format!("vendor/unrar/{s}.cpp")).collect();
    let mut build = cc::Build::new();
    build
        .cpp(true) // Switch to C++ library compilation.
        .opt_level(2)
        .std("c++14")
        // by default cc crate tries to link against dynamic stdlib, which causes problems on windows-gnu target
        .cpp_link_stdlib(None)
        .warnings(false)
        .extra_warnings(false)
        .flag_if_supported("-stdlib=libc++")
        .flag_if_supported("-fPIC")
        .flag_if_supported("-Wno-switch")
        .flag_if_supported("-Wno-parentheses")
        .flag_if_supported("-Wno-macro-redefined")
        .flag_if_supported("-Wno-dangling-else")
        .flag_if_supported("-Wno-logical-op-parentheses")
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-unused-variable")
        .flag_if_supported("-Wno-unused-function")
        .flag_if_supported("-Wno-missing-braces")
        .flag_if_supported("-Wno-unknown-pragmas")
        .flag_if_supported("-Wno-deprecated-declarations")
        .define("_FILE_OFFSET_BITS", Some("64"))
        .define("_LARGEFILE_SOURCE", None)
        .define("RAR_SMP", None)
        .define("RARDLL", None);

    // UNRAR_NG_FORCE_UTF8 commits Linux/BSD wide<->8bit filename conversions to
    // the same locale-independent WideToUtf / UtfToWide path that macOS has
    // used for years (raros.hpp:18-20 auto-defines _APPLE on __APPLE__ targets).
    //
    // Gated behind cargo feature `linux-batch-extract-utf8` (default-on). When
    // the feature is disabled, this define is NOT set and libunrar's
    // MBFUNCTIONS branch in unicode.cpp (`wcsrtombs` / `mbsrtowcs`) takes over.
    // In that mode the caller is responsible for `setlocale(LC_CTYPE, "")` —
    // either by calling it themselves before invoking this crate, or by
    // also enabling the high-level crate's `linux-batch-extract-setlocale`
    // cargo feature, which provides a Rust-side `OnceLock`-managed lazy init.
    //
    // Apple is excluded from the gate because raros.hpp already auto-defines
    // _APPLE on __APPLE__ targets — the WideToUtf path runs regardless of
    // this feature. Windows is excluded because the `_WIN_ALL` branch in
    // unicode.cpp uses `WideCharToMultiByte(CP_ACP, ...)` (OS-level system
    // codepage; CP936 zh-CN, CP932 ja-JP, CP65001 if user opted into the
    // Win10 ≥ 1803 / 11 "Beta UTF-8 ACP" toggle) and writes via
    // `CreateFile(LPCWSTR)` (wide-native NTFS). Vendor patch 0007 is a
    // no-op on both Apple and Windows builds.
    //
    // Cargo translates feature `linux-batch-extract-utf8` into the env var
    // `CARGO_FEATURE_LINUX_BATCH_EXTRACT_UTF8` (uppercase, hyphen → underscore).
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_vendor = std::env::var("CARGO_CFG_TARGET_VENDOR").unwrap_or_default();
    let feature_linux_batch_extract_utf8 =
        std::env::var("CARGO_FEATURE_LINUX_BATCH_EXTRACT_UTF8").is_ok();
    let force_utf8 = feature_linux_batch_extract_utf8
        && target_os != "windows"
        && target_vendor != "apple";
    if force_utf8 {
        build.define("UNRAR_NG_FORCE_UTF8", None);
    }

    build.files(&files).compile("libunrar.a");
}
