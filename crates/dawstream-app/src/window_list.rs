use std::collections::HashMap;

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, BOOL, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindow, GetWindowLongW, GetWindowPlacement, GetWindowRect,
    GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, GWL_EXSTYLE, GW_OWNER,
    WINDOWPLACEMENT, WS_EX_TOOLWINDOW,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowInfo {
    pub hwnd: isize,
    pub pid: u32,
    pub title: String,
    pub is_daw: bool,
    /// How likely this is the DAW's *main* window (higher = better); 0 for non-DAW windows.
    /// DAWs own many top-level windows (plugin GUIs, FX chains, mixers) from the same exe.
    pub daw_rank: u8,
    /// Restored-size area in pixels, used to prefer the big arrange window among candidates.
    pub area: i64,
}

/// The best auto-select candidate: the highest-ranked, then largest, DAW window.
pub fn best_daw_window(windows: &[WindowInfo]) -> Option<&WindowInfo> {
    windows
        .iter()
        .filter(|w| w.is_daw)
        .max_by_key(|w| (w.daw_rank, w.area))
}

const NON_DAW_CLASSES: &[&str] = &[
    "cabinetwclass",                  // Windows File Explorer
    "explorewclass",                  // Legacy Explorer
    "chrome_widgetwin_1",             // Chrome / Edge / Electron / Discord / VSCode
    "mozillawindowclass",             // Firefox
    "cascadia_hosting_window_class",  // Windows Terminal
    "consolewindowclass",             // cmd / powershell
    "applicationframewindow",         // UWP apps
];

const DAW_CLASS_PREFIXES: &[&str] = &[
    "tfruityloopsmainform",   // FL Studio
    "ableton live window",    // Ableton Live
    "reaperwnd",              // REAPER
    "steinbergwindowclass",   // Cubase / Nuendo
    "digiappwndclass",        // Pro Tools
    "bitwig",                 // Bitwig Studio
];

/// Known DAW executables, matched against the owning process's image name (lowercase,
/// without `.exe`). Window titles are NOT used: words like "reason" or "waveform" appear
/// in ordinary documents/chats, and auto-selecting one would stream it to Discord.
const DAW_EXE_EXACT: &[&str] = &[
    "fl", "fl64", "fl (scaled)", "fl64 (scaled)", "reaper", "studio one", "bitwig studio",
    "bitwigstudio", "protools", "reason", "cakewalk", "sonar", "renoise", "lmms", "mpc beats",
    "acid", "acidpro",
];

/// Versioned executables (e.g. `Ableton Live 12 Suite.exe`, `Cubase13.exe`, `Waveform13.exe`).
const DAW_EXE_PREFIXES: &[&str] = &[
    "ableton live", "cubase", "nuendo", "waveform", "mixcraft", "ardour", "samplitude",
    "sequoia", "maschine", "acid pro",
];

fn is_daw_exe(stem_lower: &str) -> bool {
    DAW_EXE_EXACT.contains(&stem_lower)
        || DAW_EXE_PREFIXES.iter().any(|p| stem_lower.starts_with(p))
}

/// Lowercase executable stem (`fl64`, `ableton live 12 suite`, ...) of a process.
fn process_exe_stem(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len)
            .is_ok();
        let _ = CloseHandle(handle);
        if !ok || len == 0 {
            return None;
        }
        let full = String::from_utf16_lossy(&buf[..len as usize]);
        std::path::Path::new(&full)
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_lowercase())
    }
}

/// Rank of a window as a DAW main-window candidate (0 = not a DAW window):
/// 3 = known DAW main-window class, 2 = unowned window of a DAW exe,
/// 1 = owned window of a DAW exe (floating plugin GUI, FX chain, dialog).
fn daw_rank(class_name: &str, exe_stem: Option<&str>, owned: bool) -> u8 {
    let class_lower = class_name.to_lowercase();
    if NON_DAW_CLASSES.iter().any(|c| class_lower == *c) {
        return 0;
    }

    if DAW_CLASS_PREFIXES
        .iter()
        .any(|p| class_lower.starts_with(p))
    {
        return if owned { 1 } else { 3 };
    }

    match exe_stem.map(is_daw_exe) {
        Some(true) if owned => 1,
        Some(true) => 2,
        _ => 0,
    }
}

struct EnumState {
    self_pid: u32,
    windows: Vec<WindowInfo>,
    exe_cache: HashMap<u32, Option<String>>,
}

unsafe extern "system" fn enum_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let state = &mut *(lparam.0 as *mut EnumState);

    // Minimized windows are listed too (a minimized DAW must still be auto-selectable;
    // the capturer simply waits until it is restored).
    if !IsWindowVisible(hwnd).as_bool() {
        return BOOL(1);
    }

    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == state.self_pid {
        return BOOL(1);
    }

    let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
    if (ex_style & WS_EX_TOOLWINDOW.0) != 0 {
        return BOOL(1);
    }

    let mut cloaked: u32 = 0;
    let _ = DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED,
        &mut cloaked as *mut u32 as *mut _,
        std::mem::size_of::<u32>() as u32,
    );
    if cloaked != 0 {
        return BOOL(1);
    }

    let mut rect = RECT::default();
    if IsIconic(hwnd).as_bool() {
        // A minimized window's live rect is a tiny caption at (-32000, -32000); use its
        // restored size instead.
        let mut wp = WINDOWPLACEMENT {
            length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
            ..Default::default()
        };
        if GetWindowPlacement(hwnd, &mut wp).is_err() {
            return BOOL(1);
        }
        rect = wp.rcNormalPosition;
    } else if GetWindowRect(hwnd, &mut rect).is_err() {
        return BOOL(1);
    }
    if rect.right.saturating_sub(rect.left) < 100 || rect.bottom.saturating_sub(rect.top) < 80 {
        return BOOL(1);
    }

    // Fixed stack buffer avoids unbounded heap allocation / integer overflow from GetWindowTextLengthW
    let mut buf = [0u16; 1025];
    let copied = GetWindowTextW(hwnd, &mut buf);
    if copied <= 0 {
        return BOOL(1);
    }

    let title = String::from_utf16_lossy(&buf[..copied as usize]);
    if title.trim().is_empty() {
        return BOOL(1);
    }

    let mut class_buf = [0u16; 256];
    let class_len = GetClassNameW(hwnd, &mut class_buf);
    let class_name = if class_len > 0 {
        String::from_utf16_lossy(&class_buf[..class_len as usize])
    } else {
        String::new()
    };

    let exe_stem = state
        .exe_cache
        .entry(pid)
        .or_insert_with(|| process_exe_stem(pid));
    let owned = GetWindow(hwnd, GW_OWNER).map(|h| !h.0.is_null()).unwrap_or(false);
    let rank = daw_rank(&class_name, exe_stem.as_deref(), owned);
    let area = (rect.right.saturating_sub(rect.left) as i64)
        * (rect.bottom.saturating_sub(rect.top) as i64);
    state.windows.push(WindowInfo {
        hwnd: hwnd.0 as isize,
        pid,
        title,
        is_daw: rank > 0,
        daw_rank: rank,
        area,
    });

    BOOL(1)
}

pub fn list_windows() -> Vec<WindowInfo> {
    let mut state = EnumState {
        self_pid: std::process::id(),
        windows: Vec::new(),
        exe_cache: HashMap::new(),
    };

    unsafe {
        let _ = EnumWindows(
            Some(enum_callback),
            LPARAM(&mut state as *mut EnumState as isize),
        );
    }

    // DAW windows first, main-window candidates (then larger windows) before plugin GUIs;
    // everything else alphabetically.
    state.windows.sort_by(|a, b| {
        b.daw_rank
            .cmp(&a.daw_rank)
            .then_with(|| if a.is_daw { b.area.cmp(&a.area) } else { std::cmp::Ordering::Equal })
            .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
    });

    state.windows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_daw_class_windows_rank_below_main_window() {
        assert_eq!(daw_rank("SteinbergWindowClass", Some("cubase13"), false), 3);
        assert_eq!(daw_rank("SteinbergWindowClass", Some("cubase13"), true), 1);
        assert_eq!(
            daw_rank("Ableton Live Window Class", Some("ableton live 12 suite"), false),
            3
        );
        assert_eq!(
            daw_rank("Ableton Live Window Class", Some("ableton live 12 suite"), true),
            1
        );
    }

    #[test]
    fn scaled_fl_studio_exe_recognized() {
        assert!(is_daw_exe("fl64 (scaled)"));
        assert!(is_daw_exe("fl (scaled)"));
        assert_eq!(daw_rank("SomeDetachedWindow", Some("fl64 (scaled)"), false), 2);
    }
}
