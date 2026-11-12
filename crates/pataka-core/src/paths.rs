//! Where Pātaka keeps its data. The app and the `pataka` command use the same
//! folder, so they share one vault, one kaupapa and one audit log.

use std::path::PathBuf;

/// The app's identifier, which names the data folder.
pub const APP_ID: &str = "nz.pataka.desktop";

/// The data folder: `~/.local/share/nz.pataka.desktop` on Linux,
/// `~/Library/Application Support/nz.pataka.desktop` on macOS and
/// `%APPDATA%\nz.pataka.desktop` on Windows (the same place Tauri uses).
/// `PATAKA_DATA_DIR` overrides it, for tests and portable setups.
pub fn data_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("PATAKA_DATA_DIR") {
        return Some(PathBuf::from(d));
    }
    dirs::data_dir().map(|d| d.join(APP_ID))
}
