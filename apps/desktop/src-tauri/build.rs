fn main() {
    let dist_dir = std::path::Path::new("../dist");
    let index_file = dist_dir.join("index.html");
    if !dist_dir.exists() || !index_file.exists() {
        panic!(
            "\n\n===============================================================================\n\
             ERROR: ReclaimArc desktop frontend build output is missing at 'apps/desktop/dist'.\n\
             Please build the frontend first by running:\n\
                 cd apps/desktop && npm ci && npm run build\n\
             ===============================================================================\n\n"
        );
    }
    tauri_build::build()
}
