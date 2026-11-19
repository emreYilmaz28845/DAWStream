use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindowLongW, GetWindowRect, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, GWL_EXSTYLE, WS_EX_TOOLWINDOW,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowInfo {
    pub hwnd: isize,
    pub pid: u32,
    pub title: String,
    pub is_daw: bool,
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

const DAW_KEYWORDS: &[&str] = &[
    "fl studio",
    "ableton live",
    "cubase",
    "nuendo",
    "reaper",
    "studio one",
    "bitwig studio",
    "pro tools",
    "reason",
    "cakewalk",
    "sonar",
    "waveform",
    "mixcraft",
    "renoise",
    "ardour",
    "samplitude",
    "sequoia",
    "acid pro",
    "mpc beats",
    "maschine",
    "lmms",
];

fn is_daw_window(class_name: &str, title: &str) -> bool {
    let class_lower = class_name.to_lowercase();
    if NON_DAW_CLASSES.iter().any(|c| class_lower == *c) {
        return false;
    }

    if DAW_CLASS_PREFIXES
        .iter()
        .any(|p| class_lower.starts_with(p))
    {
        return true;
    }

    // Avoid matching raw file paths like "C:\Users\...\FL Studio\..."
    if title.contains(":\\") {
        return false;
    }

    let title_lower = title.to_lowercase();
    DAW_KEYWORDS.iter().any(|kw| title_lower.contains(kw))
}

struct EnumState {
    self_pid: u32,
    windows: Vec<WindowInfo>,
}

unsafe extern "system" fn enum_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let state = &mut *(lparam.0 as *mut EnumState);

    if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
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
    if GetWindowRect(hwnd, &mut rect).is_err() {
        return BOOL(1);
    }
    if (rect.right - rect.left) < 100 || (rect.bottom - rect.top) < 80 {
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

    let is_daw = is_daw_window(&class_name, &title);
    state.windows.push(WindowInfo {
        hwnd: hwnd.0 as isize,
        pid,
        title,
        is_daw,
    });

    BOOL(1)
}

pub fn list_windows() -> Vec<WindowInfo> {
    let mut state = EnumState {
        self_pid: std::process::id(),
        windows: Vec::new(),
    };

    unsafe {
        let _ = EnumWindows(
            Some(enum_callback),
            LPARAM(&mut state as *mut EnumState as isize),
        );
    }

    state.windows.sort_by(|a, b| {
        b.is_daw
            .cmp(&a.is_daw)
            .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
    });

    state.windows
}
