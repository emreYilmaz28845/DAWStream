use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use windows::Win32::Storage::FileSystem::GetDriveTypeW;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{FOLDERID_RoamingAppData, SHGetKnownFolderPath};

const DRIVE_REMOTE: u32 = 4;
const MAX_CONFIG_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppConfig {
    pub device_id: Option<String>,
    pub volume: f32,
    pub muted: bool,
    #[serde(default)]
    pub app_exe_path: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            device_id: None,
            volume: 1.0,
            muted: false,
            app_exe_path: None,
        }
    }
}

fn is_local_non_unc_dir(p: &Path) -> bool {
    let s = p.to_string_lossy();
    if s.starts_with(r"\\") || s.starts_with("//") {
        return false;
    }
    let bytes = s.as_bytes();
    if bytes.len() < 3 || bytes[1] != b':' || (bytes[2] != b'\\' && bytes[2] != b'/') {
        return false;
    }
    let root: [u16; 4] = [bytes[0] as u16, b':' as u16, b'\\' as u16, 0];
    let dtype = unsafe { GetDriveTypeW(windows::core::PCWSTR(root.as_ptr())) };
    dtype != DRIVE_REMOTE
}

impl AppConfig {
    /// Resolves `%APPDATA%\DAWStream\config.json` via `SHGetKnownFolderPath`
    /// (never trusts `%APPDATA%` env var) and rejects UNC / remote network shares.
    fn path() -> Option<PathBuf> {
        let base = unsafe {
            let pwstr = SHGetKnownFolderPath(&FOLDERID_RoamingAppData, Default::default(), None).ok()?;
            let s = pwstr.to_string().ok();
            CoTaskMemFree(Some(pwstr.0 as *const _));
            PathBuf::from(s?)
        };
        if !is_local_non_unc_dir(&base) {
            return None;
        }
        Some(base.join("DAWStream").join("config.json"))
    }

    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        let Ok(meta) = fs::metadata(&path) else {
            return Self::default();
        };
        if !meta.is_file() || meta.len() > MAX_CONFIG_BYTES {
            return Self::default();
        }
        let Ok(text) = fs::read_to_string(path) else {
            return Self::default();
        };
        serde_json::from_str(&text).unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(path) = Self::path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            if fs::create_dir_all(parent).is_err() {
                return;
            }
            // Reject directory junctions / symlinks on the config folder
            if let Ok(meta) = fs::symlink_metadata(parent) {
                if meta.file_type().is_symlink() {
                    return;
                }
            }
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let pid = std::process::id();
            let tmp_path = path.with_file_name(format!("config.{pid}.tmp"));
            if fs::write(&tmp_path, json).is_ok() {
                if fs::rename(&tmp_path, &path).is_err() {
                    let _ = fs::remove_file(&path);
                    let _ = fs::rename(&tmp_path, &path);
                }
            }
        }
    }
}
