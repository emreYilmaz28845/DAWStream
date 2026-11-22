use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use eframe::egui::Color32;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GdiFlush, GetDC, ReleaseDC,
    SelectObject, SetBrushOrgEx, SetStretchBltMode, StretchBlt, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HALFTONE, HBITMAP, HDC, HGDIOBJ, SRCCOPY,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetWindowDisplayAffinity, GetWindowRect, GetWindowThreadProcessId, IsIconic,
    IsWindow, WDA_EXCLUDEFROMCAPTURE, WDA_MONITOR,
};

/// `PW_RENDERFULLCONTENT` (0x00000002) captures Direct2D / DWM / hardware-accelerated windows.
const PW_RENDERFULLCONTENT: u32 = 0x0000_0002;
const PW_CLIENTONLY: u32 = 0x0000_0001;
/// Largest window side we will render (ultrawide / multi-monitor spans are fine).
const MAX_CAPTURE_DIM: i32 = 16_384;
/// Largest window area we will render (~8K x 4.6K, a 150 MB DIB) to bound memory.
const MAX_CAPTURE_PIXELS: i64 = 8_192 * 4_608;
/// Largest frame handed to the GPU (safe texture size on every egui backend).
const MAX_OUTPUT_DIM: u32 = 4_096;

/// Why the mirror is (or isn't) updating; shown in the UI instead of a silent freeze.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum CaptureState {
    Idle = 0,
    Live = 1,
    Minimized = 2,
    Protected = 3,
    TooLarge = 4,
    Failed = 5,
}

impl CaptureState {
    fn from_u32(v: u32) -> Self {
        match v {
            1 => Self::Live,
            2 => Self::Minimized,
            3 => Self::Protected,
            4 => Self::TooLarge,
            5 => Self::Failed,
            _ => Self::Idle,
        }
    }

    /// Short user-facing explanation, `None` when capture is fine or idle.
    pub fn message(self) -> Option<&'static str> {
        match self {
            Self::Minimized => Some("DAW window is minimized: restore it to resume video"),
            Self::Protected => Some("This window blocks screen capture"),
            Self::TooLarge => Some("Window is too large to capture"),
            Self::Failed => Some("Window capture failed (retrying)"),
            Self::Idle | Self::Live => None,
        }
    }
}

pub struct CapturedFrame {
    pub width: usize,
    pub height: usize,
    /// Opaque pixels in egui's layout, so the UI thread can hand them to the texture
    /// without another full-frame conversion (alpha is 255, so premultiplied == straight).
    pub pixels: Vec<Color32>,
    pub seq: u64,
}

pub struct WindowCapturer {
    target_hwnd: Arc<AtomicIsize>,
    target_pid: Arc<AtomicU32>,
    running: Arc<AtomicBool>,
    latest_frame: Arc<Mutex<Option<CapturedFrame>>>,
    fps_x10: Arc<AtomicU64>,
    /// Requested max output size in physical pixels, packed `(w << 32) | h` (0 = no limit).
    max_output: Arc<AtomicU64>,
    state: Arc<AtomicU32>,
    thread: Option<JoinHandle<()>>,
}

impl WindowCapturer {
    pub fn start(target_fps: u32) -> Self {
        let target_hwnd = Arc::new(AtomicIsize::new(0));
        let target_pid = Arc::new(AtomicU32::new(0));
        let running = Arc::new(AtomicBool::new(true));
        let latest_frame = Arc::new(Mutex::new(None));
        let fps_x10 = Arc::new(AtomicU64::new(0));
        let max_output = Arc::new(AtomicU64::new(0));
        let state = Arc::new(AtomicU32::new(CaptureState::Idle as u32));

        let shared = Shared {
            target_hwnd: target_hwnd.clone(),
            target_pid: target_pid.clone(),
            running: running.clone(),
            latest_frame: latest_frame.clone(),
            fps_x10: fps_x10.clone(),
            max_output: max_output.clone(),
            state: state.clone(),
        };

        let frame_interval = Duration::from_secs_f64(1.0 / target_fps.max(1) as f64);

        let thread = thread::Builder::new()
            .name("dawstream-capture".into())
            .spawn(move || capture_loop(shared, frame_interval))
            .expect("failed to spawn capture thread");

        Self {
            target_hwnd,
            target_pid,
            running,
            latest_frame,
            fps_x10,
            max_output,
            state,
            thread: Some(thread),
        }
    }

    /// Sets the target `(hwnd, pid)`. Storing the PID prevents capturing an unrelated
    /// window if Windows recycles the HWND after the DAW closes.
    pub fn set_target(&self, target: Option<(isize, u32)>) {
        match target {
            Some((hwnd, pid)) => {
                self.target_pid.store(pid, Ordering::Release);
                self.target_hwnd.store(hwnd, Ordering::Release);
            }
            None => {
                self.target_hwnd.store(0, Ordering::Release);
                self.target_pid.store(0, Ordering::Release);
                if let Ok(mut guard) = self.latest_frame.lock() {
                    *guard = None;
                }
            }
        }
    }

    /// Largest frame size (physical pixels) worth capturing: the on-screen mirror size.
    /// Frames are downscaled (HALFTONE) to fit, which avoids aliasing from GPU
    /// minification and cuts per-frame conversion/upload cost.
    pub fn set_max_output(&self, width: u32, height: u32) {
        let packed = ((width as u64) << 32) | height as u64;
        self.max_output.store(packed, Ordering::Relaxed);
    }

    pub fn take_if_newer(&self, last_seq: u64) -> Option<CapturedFrame> {
        let mut guard = self.latest_frame.lock().ok()?;
        if let Some(frame) = guard.as_ref() {
            if frame.seq > last_seq {
                return guard.take();
            }
        }
        None
    }

    pub fn has_active_target(&self) -> bool {
        self.target_hwnd.load(Ordering::Acquire) != 0
    }

    pub fn fps(&self) -> f32 {
        self.fps_x10.load(Ordering::Relaxed) as f32 / 10.0
    }

    pub fn state(&self) -> CaptureState {
        CaptureState::from_u32(self.state.load(Ordering::Relaxed))
    }
}

impl Drop for WindowCapturer {
    fn drop(&mut self) {
        // Signal the capture thread to stop and detach instead of blocking forever
        // if a hung target window is currently stalling inside synchronous PrintWindow().
        self.running.store(false, Ordering::Release);
        let _ = self.thread.take();
    }
}

struct Shared {
    target_hwnd: Arc<AtomicIsize>,
    target_pid: Arc<AtomicU32>,
    running: Arc<AtomicBool>,
    latest_frame: Arc<Mutex<Option<CapturedFrame>>>,
    fps_x10: Arc<AtomicU64>,
    max_output: Arc<AtomicU64>,
    state: Arc<AtomicU32>,
}

impl Shared {
    fn set_state(&self, s: CaptureState) {
        self.state.store(s as u32, Ordering::Relaxed);
    }

    fn clear_target(&self) {
        self.target_hwnd.store(0, Ordering::Release);
        self.target_pid.store(0, Ordering::Release);
        self.fps_x10.store(0, Ordering::Relaxed);
        self.set_state(CaptureState::Idle);
        if let Ok(mut guard) = self.latest_frame.lock() {
            *guard = None;
        }
    }
}

/// A memory DC with a selected top-down 32-bit DIB section, recreated on size change.
struct Dib {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut c_void,
    width: i32,
    height: i32,
}

impl Dib {
    fn empty() -> Self {
        Self {
            dc: HDC::default(),
            bitmap: HBITMAP::default(),
            old: HGDIOBJ::default(),
            bits: std::ptr::null_mut(),
            width: 0,
            height: 0,
        }
    }

    fn is_valid(&self) -> bool {
        !self.dc.0.is_null() && !self.bits.is_null()
    }

    fn release(&mut self) {
        unsafe {
            if !self.old.0.is_null() && !self.dc.0.is_null() {
                SelectObject(self.dc, self.old);
            }
            if !self.bitmap.0.is_null() {
                let _ = DeleteObject(self.bitmap);
            }
            if !self.dc.0.is_null() {
                let _ = DeleteDC(self.dc);
            }
        }
        *self = Self::empty();
    }

    /// Ensures a `width x height` DIB; returns `false` if it couldn't be created.
    fn ensure(&mut self, width: i32, height: i32) -> bool {
        if self.is_valid() && self.width == width && self.height == height {
            return true;
        }
        self.release();
        unsafe {
            let screen_dc = GetDC(HWND::default());
            if screen_dc.0.is_null() {
                return false;
            }
            let dc = CreateCompatibleDC(screen_dc);
            ReleaseDC(HWND::default(), screen_dc);
            if dc.0.is_null() {
                return false;
            }
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height, // top-down DIB
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut ptr: *mut c_void = std::ptr::null_mut();
            match CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut ptr, None, 0) {
                Ok(bmp) if !bmp.0.is_null() && !ptr.is_null() => {
                    self.old = SelectObject(dc, bmp);
                    self.dc = dc;
                    self.bitmap = bmp;
                    self.bits = ptr;
                    self.width = width;
                    self.height = height;
                    true
                }
                Ok(bmp) => {
                    if !bmp.0.is_null() {
                        let _ = DeleteObject(bmp);
                    }
                    let _ = DeleteDC(dc);
                    false
                }
                Err(_) => {
                    let _ = DeleteDC(dc);
                    false
                }
            }
        }
    }

    /// BGRA DIB -> egui pixels in one pass (no zero-fill), alpha forced opaque.
    fn to_pixels(&self) -> Vec<Color32> {
        let count = self.width as usize * self.height as usize;
        // CreateDIBSection bits are DWORD-aligned, so reading them as u32 (0xAARRGGBB) is valid.
        let src = unsafe { std::slice::from_raw_parts(self.bits as *const u32, count) };
        src.iter()
            .map(|&p| Color32::from_rgba_premultiplied((p >> 16) as u8, (p >> 8) as u8, p as u8, 255))
            .collect()
    }
}

/// Output size for a `w x h` source fitted (aspect-preserving, downscale only) into
/// `max_w x max_h` and the GPU texture limit.
fn fit_output(w: i32, h: i32, max_w: u32, max_h: u32) -> (i32, i32) {
    let (max_w, max_h) = (
        if max_w == 0 { MAX_OUTPUT_DIM } else { max_w.min(MAX_OUTPUT_DIM) },
        if max_h == 0 { MAX_OUTPUT_DIM } else { max_h.min(MAX_OUTPUT_DIM) },
    );
    let scale = (max_w as f64 / w as f64)
        .min(max_h as f64 / h as f64)
        .min(1.0);
    if scale >= 1.0 {
        return (w, h);
    }
    (
        ((w as f64 * scale).round() as i32).max(1),
        ((h as f64 * scale).round() as i32).max(1),
    )
}

fn capture_loop(shared: Shared, frame_interval: Duration) {
    let mut seq: u64 = 0;
    let mut frames_this_sec: u32 = 0;
    let mut fps_timer = Instant::now();
    let mut src = Dib::empty();
    let mut scaled = Dib::empty();

    while shared.running.load(Ordering::Acquire) {
        let loop_start = Instant::now();

        // FPS is refreshed every second even when no frames arrive (so it drops to 0).
        let elapsed = fps_timer.elapsed();
        if elapsed >= Duration::from_secs(1) {
            let fps = (frames_this_sec as f64 / elapsed.as_secs_f64() * 10.0) as u64;
            shared.fps_x10.store(fps, Ordering::Relaxed);
            frames_this_sec = 0;
            fps_timer = Instant::now();
        }

        let raw_hwnd = shared.target_hwnd.load(Ordering::Acquire);
        let expected_pid = shared.target_pid.load(Ordering::Acquire);

        if raw_hwnd == 0 {
            shared.fps_x10.store(0, Ordering::Relaxed);
            shared.set_state(CaptureState::Idle);
            thread::sleep(Duration::from_millis(50));
            continue;
        }

        let hwnd = HWND(raw_hwnd as *mut c_void);
        let pid_matches = || {
            if expected_pid == 0 {
                return true;
            }
            let mut current_pid: u32 = 0;
            unsafe { GetWindowThreadProcessId(hwnd, Some(&mut current_pid)) };
            current_pid == expected_pid
        };

        unsafe {
            // Guard against Windows HWND recycling leaking an unrelated private window
            if !IsWindow(hwnd).as_bool() || !pid_matches() {
                shared.clear_target();
                thread::sleep(Duration::from_millis(50));
                continue;
            }
            if IsIconic(hwnd).as_bool() {
                shared.set_state(CaptureState::Minimized);
                thread::sleep(Duration::from_millis(50));
                continue;
            }

            // Respect Windows display affinity (skip capture-protected windows)
            let mut affinity: u32 = 0;
            if GetWindowDisplayAffinity(hwnd, &mut affinity).is_ok()
                && (affinity == WDA_EXCLUDEFROMCAPTURE.0 || affinity == WDA_MONITOR.0)
            {
                shared.set_state(CaptureState::Protected);
                thread::sleep(Duration::from_millis(50));
                continue;
            }

            let mut rect = RECT::default();
            let got_client = GetClientRect(hwnd, &mut rect).is_ok();
            let mut width = rect.right.checked_sub(rect.left).unwrap_or(0);
            let mut height = rect.bottom.checked_sub(rect.top).unwrap_or(0);
            let mut flags = PRINT_WINDOW_FLAGS(PW_CLIENTONLY | PW_RENDERFULLCONTENT);

            if !got_client || width <= 0 || height <= 0 {
                if GetWindowRect(hwnd, &mut rect).is_err() {
                    shared.set_state(CaptureState::Failed);
                    thread::sleep(Duration::from_millis(50));
                    continue;
                }
                width = rect.right.checked_sub(rect.left).unwrap_or(0);
                height = rect.bottom.checked_sub(rect.top).unwrap_or(0);
                flags = PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT);
            }

            if width <= 0 || height <= 0 {
                shared.set_state(CaptureState::Failed);
                thread::sleep(Duration::from_millis(50));
                continue;
            }
            if width > MAX_CAPTURE_DIM
                || height > MAX_CAPTURE_DIM
                || (width as i64) * (height as i64) > MAX_CAPTURE_PIXELS
            {
                shared.set_state(CaptureState::TooLarge);
                thread::sleep(Duration::from_millis(200));
                continue;
            }

            if !src.ensure(width, height) {
                shared.set_state(CaptureState::Failed);
                thread::sleep(Duration::from_millis(100));
                continue;
            }

            // Re-verify owning PID immediately before PrintWindow to minimize HWND-recycling TOCTOU
            if !pid_matches() {
                shared.clear_target();
                continue;
            }

            if PrintWindow(hwnd, src.dc, flags).as_bool() {
                let packed = shared.max_output.load(Ordering::Relaxed);
                let (out_w, out_h) =
                    fit_output(width, height, (packed >> 32) as u32, packed as u32);

                let frame = if (out_w, out_h) == (width, height) {
                    Some((width, height, src.to_pixels()))
                } else if scaled.ensure(out_w, out_h) {
                    // HALFTONE averages source pixels: clean downscale of small DAW text.
                    SetStretchBltMode(scaled.dc, HALFTONE);
                    let _ = SetBrushOrgEx(scaled.dc, 0, 0, None);
                    let ok = StretchBlt(
                        scaled.dc, 0, 0, out_w, out_h, src.dc, 0, 0, width, height, SRCCOPY,
                    )
                    .as_bool();
                    let _ = GdiFlush();
                    ok.then(|| (out_w, out_h, scaled.to_pixels()))
                } else {
                    None
                };

                match frame {
                    Some((w, h, pixels)) => {
                        seq += 1;
                        if let Ok(mut guard) = shared.latest_frame.lock() {
                            *guard = Some(CapturedFrame {
                                width: w as usize,
                                height: h as usize,
                                pixels,
                                seq,
                            });
                        }
                        frames_this_sec += 1;
                        shared.set_state(CaptureState::Live);
                    }
                    None => shared.set_state(CaptureState::Failed),
                }
            } else {
                shared.set_state(CaptureState::Failed);
            }
        }

        let spent = loop_start.elapsed();
        if spent < frame_interval {
            thread::sleep(frame_interval - spent);
        }
    }

    src.release();
    scaled.release();
}

#[cfg(test)]
mod tests {
    use super::fit_output;

    #[test]
    fn fit_output_downscales_preserving_aspect() {
        assert_eq!(fit_output(3840, 2160, 1920, 1080), (1920, 1080));
        assert_eq!(fit_output(5120, 1440, 2000, 2000), (2000, 563));
        // Never upscale.
        assert_eq!(fit_output(800, 600, 1920, 1080), (800, 600));
        // No requested limit: still capped to the texture limit.
        assert_eq!(fit_output(7680, 2160, 0, 0), (4096, 1152));
    }
}
