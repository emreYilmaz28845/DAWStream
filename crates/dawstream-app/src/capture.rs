use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use eframe::egui::Color32;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetWindowDisplayAffinity, GetWindowRect, GetWindowThreadProcessId, IsIconic,
    IsWindow, WDA_EXCLUDEFROMCAPTURE, WDA_MONITOR,
};

/// `PW_RENDERFULLCONTENT` (0x00000002) captures Direct2D / DWM / hardware-accelerated windows.
const PW_RENDERFULLCONTENT: u32 = 0x0000_0002;
const PW_CLIENTONLY: u32 = 0x0000_0001;
/// Cap capture dimensions at 4K (4096x4096) to prevent OOM or GPU texture limit aborts.
const MAX_CAPTURE_DIM: i32 = 4096;

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
    thread: Option<JoinHandle<()>>,
}

impl WindowCapturer {
    pub fn start(target_fps: u32) -> Self {
        let target_hwnd = Arc::new(AtomicIsize::new(0));
        let target_pid = Arc::new(AtomicU32::new(0));
        let running = Arc::new(AtomicBool::new(true));
        let latest_frame = Arc::new(Mutex::new(None));
        let fps_x10 = Arc::new(AtomicU64::new(0));

        let t_hwnd = target_hwnd.clone();
        let t_pid = target_pid.clone();
        let t_run = running.clone();
        let t_frame = latest_frame.clone();
        let t_fps = fps_x10.clone();

        let frame_interval = Duration::from_secs_f64(1.0 / target_fps.max(1) as f64);

        let thread = thread::Builder::new()
            .name("dawstream-capture".into())
            .spawn(move || {
                capture_loop(t_hwnd, t_pid, t_run, t_frame, t_fps, frame_interval);
            })
            .expect("failed to spawn capture thread");

        Self {
            target_hwnd,
            target_pid,
            running,
            latest_frame,
            fps_x10,
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
}

impl Drop for WindowCapturer {
    fn drop(&mut self) {
        // Signal the capture thread to stop and detach instead of blocking forever
        // if a hung target window is currently stalling inside synchronous PrintWindow().
        self.running.store(false, Ordering::Release);
        let _ = self.thread.take();
    }
}

fn capture_loop(
    target_hwnd: Arc<AtomicIsize>,
    target_pid: Arc<AtomicU32>,
    running: Arc<AtomicBool>,
    latest_frame: Arc<Mutex<Option<CapturedFrame>>>,
    fps_x10: Arc<AtomicU64>,
    frame_interval: Duration,
) {
    let mut seq: u64 = 0;
    let mut frames_this_sec: u32 = 0;
    let mut fps_timer = Instant::now();

    let mut cached_w: i32 = 0;
    let mut cached_h: i32 = 0;
    let mut mem_dc = windows::Win32::Graphics::Gdi::HDC::default();
    let mut bitmap = windows::Win32::Graphics::Gdi::HBITMAP::default();
    let mut old_obj = windows::Win32::Graphics::Gdi::HGDIOBJ::default();
    let mut bits_ptr: *mut c_void = std::ptr::null_mut();

    while running.load(Ordering::Acquire) {
        let loop_start = Instant::now();
        let raw_hwnd = target_hwnd.load(Ordering::Acquire);
        let expected_pid = target_pid.load(Ordering::Acquire);

        if raw_hwnd == 0 {
            fps_x10.store(0, Ordering::Relaxed);
            thread::sleep(Duration::from_millis(50));
            continue;
        }

        let hwnd = HWND(raw_hwnd as *mut c_void);

        unsafe {
            if !IsWindow(hwnd).as_bool() {
                target_hwnd.store(0, Ordering::Release);
                target_pid.store(0, Ordering::Release);
                fps_x10.store(0, Ordering::Relaxed);
                if let Ok(mut guard) = latest_frame.lock() {
                    *guard = None;
                }
                thread::sleep(Duration::from_millis(50));
                continue;
            }
            if IsIconic(hwnd).as_bool() {
                fps_x10.store(0, Ordering::Relaxed);
                thread::sleep(Duration::from_millis(50));
                continue;
            }

            // Respect Windows display affinity (skip capture-protected windows)
            let mut affinity: u32 = 0;
            if GetWindowDisplayAffinity(hwnd, &mut affinity).is_ok()
                && (affinity == WDA_EXCLUDEFROMCAPTURE.0 || affinity == WDA_MONITOR.0)
            {
                thread::sleep(Duration::from_millis(50));
                continue;
            }

            // Guard against Windows HWND recycling leaking an unrelated private window
            if expected_pid != 0 {
                let mut current_pid: u32 = 0;
                GetWindowThreadProcessId(hwnd, Some(&mut current_pid));
                if current_pid != expected_pid {
                    target_hwnd.store(0, Ordering::Release);
                    target_pid.store(0, Ordering::Release);
                    fps_x10.store(0, Ordering::Relaxed);
                    if let Ok(mut guard) = latest_frame.lock() {
                        *guard = None;
                    }
                    thread::sleep(Duration::from_millis(50));
                    continue;
                }
            }

            let mut rect = RECT::default();
            let got_client = GetClientRect(hwnd, &mut rect).is_ok();
            let mut width = rect.right.checked_sub(rect.left).unwrap_or(0);
            let mut height = rect.bottom.checked_sub(rect.top).unwrap_or(0);
            let mut flags = PRINT_WINDOW_FLAGS(PW_CLIENTONLY | PW_RENDERFULLCONTENT);

            if !got_client || width <= 0 || height <= 0 {
                if GetWindowRect(hwnd, &mut rect).is_err() {
                    thread::sleep(Duration::from_millis(50));
                    continue;
                }
                width = rect.right.checked_sub(rect.left).unwrap_or(0);
                height = rect.bottom.checked_sub(rect.top).unwrap_or(0);
                flags = PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT);
            }

            if width <= 0 || height <= 0 || width > MAX_CAPTURE_DIM || height > MAX_CAPTURE_DIM {
                thread::sleep(Duration::from_millis(50));
                continue;
            }

            if width != cached_w || height != cached_h || mem_dc.0.is_null() {
                if !old_obj.0.is_null() && !mem_dc.0.is_null() {
                    SelectObject(mem_dc, old_obj);
                    old_obj = Default::default();
                }
                if !bitmap.0.is_null() {
                    let _ = DeleteObject(bitmap);
                    bitmap = Default::default();
                    bits_ptr = std::ptr::null_mut();
                }
                if !mem_dc.0.is_null() {
                    let _ = DeleteDC(mem_dc);
                    mem_dc = Default::default();
                }

                let screen_dc = GetDC(HWND::default());
                if screen_dc.0.is_null() {
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
                mem_dc = CreateCompatibleDC(screen_dc);
                ReleaseDC(HWND::default(), screen_dc);
                if mem_dc.0.is_null() {
                    thread::sleep(Duration::from_millis(100));
                    continue;
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
                match CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut ptr, None, 0) {
                    Ok(bmp) if !bmp.0.is_null() && !ptr.is_null() => {
                        bitmap = bmp;
                        bits_ptr = ptr;
                        old_obj = SelectObject(mem_dc, bitmap);
                        cached_w = width;
                        cached_h = height;
                    }
                    Ok(bmp) => {
                        if !bmp.0.is_null() {
                            let _ = DeleteObject(bmp);
                        }
                        let _ = DeleteDC(mem_dc);
                        mem_dc = Default::default();
                        thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                    Err(_) => {
                        let _ = DeleteDC(mem_dc);
                        mem_dc = Default::default();
                        thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                }
            }

            // Re-verify owning PID immediately before PrintWindow to minimize HWND-recycling TOCTOU
            if expected_pid != 0 {
                let mut current_pid: u32 = 0;
                GetWindowThreadProcessId(hwnd, Some(&mut current_pid));
                if current_pid != expected_pid {
                    target_hwnd.store(0, Ordering::Release);
                    target_pid.store(0, Ordering::Release);
                    fps_x10.store(0, Ordering::Relaxed);
                    if let Ok(mut guard) = latest_frame.lock() {
                        *guard = None;
                    }
                    continue;
                }
            }

            if PrintWindow(hwnd, mem_dc, flags).as_bool() {
                let Some(pixel_count) = (width as usize).checked_mul(height as usize) else {
                    continue;
                };
                if pixel_count.checked_mul(4).is_none() {
                    continue;
                }
                // CreateDIBSection bits are DWORD-aligned, so reading them as u32 (0xAARRGGBB) is valid.
                let src = std::slice::from_raw_parts(bits_ptr as *const u32, pixel_count);

                // One pass, no zero-fill: BGRA -> egui RGBA, alpha forced opaque.
                let pixels: Vec<Color32> = src
                    .iter()
                    .map(|&p| Color32::from_rgba_premultiplied((p >> 16) as u8, (p >> 8) as u8, p as u8, 255))
                    .collect();

                seq += 1;
                if let Ok(mut guard) = latest_frame.lock() {
                    *guard = Some(CapturedFrame {
                        width: width as usize,
                        height: height as usize,
                        pixels,
                        seq,
                    });
                }

                frames_this_sec += 1;
                let elapsed = fps_timer.elapsed();
                if elapsed >= Duration::from_secs(1) {
                    let fps = (frames_this_sec as f64 / elapsed.as_secs_f64() * 10.0) as u64;
                    fps_x10.store(fps, Ordering::Relaxed);
                    frames_this_sec = 0;
                    fps_timer = Instant::now();
                }
            }
        }

        let spent = loop_start.elapsed();
        if spent < frame_interval {
            thread::sleep(frame_interval - spent);
        }
    }

    unsafe {
        if !old_obj.0.is_null() && !mem_dc.0.is_null() {
            SelectObject(mem_dc, old_obj);
        }
        if !bitmap.0.is_null() {
            let _ = DeleteObject(bitmap);
        }
        if !mem_dc.0.is_null() {
            let _ = DeleteDC(mem_dc);
        }
    }
}
