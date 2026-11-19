use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, GetWindowRect, IsIconic, IsWindow};

/// `PW_RENDERFULLCONTENT` (0x00000002) captures Direct2D / DWM / hardware-accelerated windows.
const PW_RENDERFULLCONTENT: u32 = 0x0000_0002;
const PW_CLIENTONLY: u32 = 0x0000_0001;
const MAX_CAPTURE_DIM: i32 = 8192;

pub struct CapturedFrame {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
    pub seq: u64,
}

pub struct WindowCapturer {
    target_hwnd: Arc<AtomicIsize>,
    running: Arc<AtomicBool>,
    latest_frame: Arc<Mutex<Option<CapturedFrame>>>,
    fps_x10: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

impl WindowCapturer {
    pub fn start(target_fps: u32) -> Self {
        let target_hwnd = Arc::new(AtomicIsize::new(0));
        let running = Arc::new(AtomicBool::new(true));
        let latest_frame = Arc::new(Mutex::new(None));
        let fps_x10 = Arc::new(AtomicU64::new(0));

        let t_hwnd = target_hwnd.clone();
        let t_run = running.clone();
        let t_frame = latest_frame.clone();
        let t_fps = fps_x10.clone();

        let frame_interval = Duration::from_secs_f64(1.0 / target_fps.max(1) as f64);

        let thread = thread::Builder::new()
            .name("dawstream-capture".into())
            .spawn(move || {
                capture_loop(t_hwnd, t_run, t_frame, t_fps, frame_interval);
            })
            .expect("failed to spawn capture thread");

        Self {
            target_hwnd,
            running,
            latest_frame,
            fps_x10,
            thread: Some(thread),
        }
    }

    pub fn set_target(&self, hwnd: Option<isize>) {
        self.target_hwnd.store(hwnd.unwrap_or(0), Ordering::Release);
        if hwnd.is_none() {
            if let Ok(mut guard) = self.latest_frame.lock() {
                *guard = None;
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

    pub fn fps(&self) -> f32 {
        self.fps_x10.load(Ordering::Relaxed) as f32 / 10.0
    }
}

impl Drop for WindowCapturer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

fn capture_loop(
    target_hwnd: Arc<AtomicIsize>,
    running: Arc<AtomicBool>,
    latest_frame: Arc<Mutex<Option<CapturedFrame>>>,
    fps_x10: Arc<AtomicU64>,
    frame_interval: Duration,
) {
    let mut seq: u64 = 0;
    let mut frames_this_sec: u32 = 0;
    let mut fps_timer = Instant::now();

    // Cached GDI resources to avoid re-creating DIB on every frame
    let mut cached_w: i32 = 0;
    let mut cached_h: i32 = 0;
    let mut mem_dc = windows::Win32::Graphics::Gdi::HDC::default();
    let mut bitmap = windows::Win32::Graphics::Gdi::HBITMAP::default();
    let mut old_obj = windows::Win32::Graphics::Gdi::HGDIOBJ::default();
    let mut bits_ptr: *mut c_void = std::ptr::null_mut();

    while running.load(Ordering::Acquire) {
        let loop_start = Instant::now();
        let raw_hwnd = target_hwnd.load(Ordering::Acquire);

        if raw_hwnd == 0 {
            fps_x10.store(0, Ordering::Relaxed);
            thread::sleep(Duration::from_millis(50));
            continue;
        }

        let hwnd = HWND(raw_hwnd as *mut c_void);

        unsafe {
            if !IsWindow(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
                thread::sleep(Duration::from_millis(50));
                continue;
            }

            let mut rect = RECT::default();
            let got_client = GetClientRect(hwnd, &mut rect).is_ok();
            let mut width = rect.right - rect.left;
            let mut height = rect.bottom - rect.top;
            let mut flags = PRINT_WINDOW_FLAGS(PW_CLIENTONLY | PW_RENDERFULLCONTENT);

            if !got_client || width <= 0 || height <= 0 {
                if GetWindowRect(hwnd, &mut rect).is_err() {
                    thread::sleep(Duration::from_millis(50));
                    continue;
                }
                width = rect.right - rect.left;
                height = rect.bottom - rect.top;
                flags = PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT);
            }

            if width <= 0 || height <= 0 || width > MAX_CAPTURE_DIM || height > MAX_CAPTURE_DIM {
                thread::sleep(Duration::from_millis(50));
                continue;
            }

            // Re-allocate DIB only when dimensions change
            if width != cached_w || height != cached_h || mem_dc.0.is_null() {
                if !old_obj.0.is_null() && !mem_dc.0.is_null() {
                    SelectObject(mem_dc, old_obj);
                    old_obj = Default::default();
                }
                if !bitmap.0.is_null() {
                    let _ = DeleteObject(bitmap);
                    bitmap = Default::default();
                }
                if !mem_dc.0.is_null() {
                    let _ = DeleteDC(mem_dc);
                }

                let screen_dc = GetDC(HWND::default());
                mem_dc = CreateCompatibleDC(screen_dc);
                ReleaseDC(HWND::default(), screen_dc);

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
                    Ok(bmp) if !ptr.is_null() => {
                        bitmap = bmp;
                        bits_ptr = ptr;
                        old_obj = SelectObject(mem_dc, bitmap);
                        cached_w = width;
                        cached_h = height;
                    }
                    _ => {
                        if !mem_dc.0.is_null() {
                            let _ = DeleteDC(mem_dc);
                            mem_dc = Default::default();
                        }
                        thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                }
            }

            if PrintWindow(hwnd, mem_dc, flags).as_bool() {
                let Some(pixel_count) = (width as usize).checked_mul(height as usize) else {
                    continue;
                };
                let Some(byte_len) = pixel_count.checked_mul(4) else {
                    continue;
                };
                let src = std::slice::from_raw_parts(bits_ptr as *const u8, byte_len);

                let mut rgba = vec![0u8; byte_len];
                // Fast BGRA -> RGBA conversion (force opaque alpha = 255)
                for (dst_px, src_px) in rgba.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                    dst_px[0] = src_px[2]; // R
                    dst_px[1] = src_px[1]; // G
                    dst_px[2] = src_px[0]; // B
                    dst_px[3] = 255;       // A
                }

                seq += 1;
                if let Ok(mut guard) = latest_frame.lock() {
                    *guard = Some(CapturedFrame {
                        width: width as usize,
                        height: height as usize,
                        rgba,
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

    // Cleanup GDI handles
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
