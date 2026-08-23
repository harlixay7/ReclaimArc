fn main() {
    // The vendored UnRAR source (compiled by unrar_sys) references Windows
    // system libraries that the unrar_sys build script does not link on MSVC.
    for lib in ["advapi32", "user32", "shell32", "ole32", "gdi32"] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
}