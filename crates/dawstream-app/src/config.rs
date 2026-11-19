use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

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

impl AppConfig {
    fn path() -> Option<PathBuf> {
        let appdata = std::env::var_os("APPDATA")?;
        Some(PathBuf::from(appdata).join("DAWStream").join("config.json"))
    }

    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
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
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            // Atomic write: write to temp file in same directory, then rename over target
            let tmp_path = path.with_extension("json.tmp");
            if fs::write(&tmp_path, json).is_ok() {
                if fs::rename(&tmp_path, &path).is_err() {
                    let _ = fs::remove_file(&path);
                    let _ = fs::rename(&tmp_path, &path);
                }
            }
        }
    }
}
