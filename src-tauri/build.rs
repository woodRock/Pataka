use std::path::PathBuf;

fn main() {
    // The `pataka` command ships inside the app (Tauri sidecar), so Settings
    // can install it for Claude Code. Release builds put the real binary at
    // binaries/pataka-cli-<target>; for development builds without it, an empty
    // placeholder keeps the build working (the app checks it before use).
    let target = std::env::var("TARGET").unwrap_or_default();
    let ext = if target.contains("windows") { ".exe" } else { "" };
    let sidecar = PathBuf::from("binaries").join(format!("pataka-cli-{target}{ext}"));
    if !sidecar.exists() {
        let _ = std::fs::create_dir_all("binaries");
        let _ = std::fs::write(&sidecar, b"");
        println!(
            "cargo:warning=no pataka CLI at {}; using an empty placeholder",
            sidecar.display()
        );
    }
    println!("cargo:rerun-if-changed={}", sidecar.display());
    tauri_build::build()
}
