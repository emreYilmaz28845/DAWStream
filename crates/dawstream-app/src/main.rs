#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod capture;
mod config;
mod window_list;

use std::time::Duration;

use audio::{list_output_devices, AudioEngine, AudioOutputDevice, DeviceTier};
use capture::WindowCapturer;
use config::AppConfig;
use eframe::egui;
use window_list::{list_windows, WindowInfo};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, SetLastError, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, HANDLE,
};
use windows::Win32::System::LibraryLoader::{
    SetDefaultDllDirectories, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows::Win32::System::Threading::{
    CreateMutexW, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetWindowThreadProcessId, SetForegroundWindow, ShowWindow, SW_RESTORE,
};

const APP_WINDOW_TITLE: &str = "DAWStream — Discord DAW Streaming";

struct SingleInstanceGuard(HANDLE);

unsafe fn hwnd_belongs_to_dawstream(hwnd: windows::Win32::Foundation::HWND) -> bool {
    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == 0 {
        return false;
    }
    let Ok(proc_handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
        return false;
    };
    let mut buf = [0u16; 512];
    let mut len = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(
        proc_handle,
        PROCESS_NAME_WIN32,
        PWSTR(buf.as_mut_ptr()),
        &mut len,
    )
    .is_ok();
    let _ = CloseHandle(proc_handle);
    if !ok || len == 0 {
        return false;
    }
    let full = String::from_utf16_lossy(&buf[..len as usize]);
    std::path::Path::new(&full)
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| {
            let lower = n.to_ascii_lowercase();
            lower == "dawstream.exe" || lower == "dawstream-app.exe"
        })
        .unwrap_or(false)
}

impl SingleInstanceGuard {
    /// Returns `None` if another instance of DAWStream.exe is already running
    /// (bringing it to the foreground only after verifying its process image name),
    /// or fails closed if the mutex cannot be created.
    fn acquire() -> Option<Self> {
        let name: Vec<u16> = "Local\\DAWStream_AppSingleInstance_v1\0"
            .encode_utf16()
            .collect();
        unsafe {
            SetLastError(ERROR_SUCCESS);
            let Ok(handle) = CreateMutexW(None, false, PCWSTR(name.as_ptr())) else {
                // Fail closed rather than allowing concurrent readers on the SPSC ring
                return None;
            };
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let title: Vec<u16> = format!("{APP_WINDOW_TITLE}\0").encode_utf16().collect();
                if let Ok(hwnd) = FindWindowW(None, PCWSTR(title.as_ptr())) {
                    if hwnd_belongs_to_dawstream(hwnd) {
                        let _ = ShowWindow(hwnd, SW_RESTORE);
                        let _ = SetForegroundWindow(hwnd);
                    }
                }
                let _ = CloseHandle(handle);
                return None;
            }
            Some(Self(handle))
        }
    }
}

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() && !self.0 .0.is_null() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

fn main() -> eframe::Result<()> {
    // Prevent glutin WGL backend from loading an attacker-supplied DLL via GLUTIN_WGL_OPENGL_DLL
    unsafe {
        std::env::remove_var("GLUTIN_WGL_OPENGL_DLL");
        let _ = SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32);
    }

    let Some(_instance_guard) = SingleInstanceGuard::acquire() else {
        return Ok(());
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_WINDOW_TITLE)
            .with_inner_size([1280.0, 760.0])
            .with_min_inner_size([640.0, 400.0]),
        ..Default::default()
    };

    eframe::run_native(
        "DAWStream",
        options,
        Box::new(|cc| Ok(Box::new(DawStreamApp::new(cc)))),
    )
}

struct DawStreamApp {
    windows: Vec<WindowInfo>,
    selected_target: Option<(isize, u32)>,

    devices: Vec<AudioOutputDevice>,
    selected_device_id: Option<String>,

    audio_engine: AudioEngine,
    capturer: WindowCapturer,

    mirror_texture: Option<egui::TextureHandle>,
    last_frame_seq: u64,

    volume: f32,
    muted: bool,
    hide_toolbar: bool,

    smooth_peak_l: f32,
    smooth_peak_r: f32,
}

impl DawStreamApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = egui::Color32::from_rgb(16, 17, 21);
        visuals.window_fill = egui::Color32::from_rgb(22, 24, 30);
        cc.egui_ctx.set_visuals(visuals);

        let saved = AppConfig::load();

        let windows = list_windows();
        let selected_target = windows.iter().find(|w| w.is_daw).map(|w| (w.hwnd, w.pid));

        let devices = list_output_devices();
        let selected_device_id = saved
            .device_id
            .filter(|id| devices.iter().any(|d| &d.id == id))
            .or_else(|| devices.first().map(|d| d.id.clone()));

        let volume = saved.volume.clamp(0.0, 1.5);
        let muted = saved.muted;

        let audio_engine = AudioEngine::start(selected_device_id.clone());
        audio_engine.set_volume(volume);
        audio_engine.set_muted(muted);

        let capturer = WindowCapturer::start(60);
        capturer.set_target(selected_target);

        let app = Self {
            windows,
            selected_target,
            devices,
            selected_device_id,
            audio_engine,
            capturer,
            mirror_texture: None,
            last_frame_seq: 0,
            volume,
            muted,
            hide_toolbar: false,
            smooth_peak_l: 0.0,
            smooth_peak_r: 0.0,
        };
        // Persist current exe path so the VST3/CLAP plugin can launch DAWStream.exe with one click
        app.save_config();
        app
    }

    fn save_config(&self) {
        let exe_path = std::env::current_exe()
            .ok()
            .and_then(|p| p.to_str().map(|s| s.to_string()));
        AppConfig {
            device_id: self.selected_device_id.clone(),
            volume: self.volume,
            muted: self.muted,
            app_exe_path: exe_path,
        }
        .save();
    }

    fn refresh_windows(&mut self) {
        self.windows = list_windows();
        if let Some((hwnd, pid)) = self.selected_target {
            // Require BOTH hwnd and pid to match so a recycled HWND never re-arms on a new process
            if self.windows.iter().any(|w| w.hwnd == hwnd && w.pid == pid) {
                self.capturer.set_target(Some((hwnd, pid)));
            } else {
                let daw = self.windows.iter().find(|w| w.is_daw).map(|w| (w.hwnd, w.pid));
                self.selected_target = daw;
                self.capturer.set_target(daw);
                self.mirror_texture = None;
            }
        } else if let Some(daw) = self.windows.iter().find(|w| w.is_daw) {
            self.selected_target = Some((daw.hwnd, daw.pid));
            self.capturer.set_target(self.selected_target);
        }
    }

    fn refresh_devices(&mut self) {
        self.devices = list_output_devices();
        if let Some(ref id) = self.selected_device_id {
            if !self.devices.iter().any(|d| &d.id == id) {
                self.selected_device_id = self.devices.first().map(|d| d.id.clone());
                self.audio_engine.set_device(self.selected_device_id.clone());
                self.save_config();
            }
        }
    }

    fn draw_vu_meter(ui: &mut egui::Ui, peak_l: f32, peak_r: f32) {
        let desired = egui::vec2(110.0, 20.0);
        let (rect, _resp) = ui.allocate_exact_size(desired, egui::Sense::hover());
        let painter = ui.painter();

        painter.rect_filled(rect, 3.0, egui::Color32::from_rgb(28, 30, 38));

        let draw_bar = |y_min: f32, y_max: f32, level: f32| {
            let db = if level > 1e-5 {
                20.0 * level.log10()
            } else {
                -60.0
            };
            let norm = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
            let w = (rect.width() - 4.0) * norm;
            if w > 0.5 {
                let color = if db > -3.0 {
                    egui::Color32::from_rgb(235, 75, 75)
                } else if db > -12.0 {
                    egui::Color32::from_rgb(240, 195, 65)
                } else {
                    egui::Color32::from_rgb(65, 215, 130)
                };
                let bar = egui::Rect::from_min_max(
                    egui::pos2(rect.left() + 2.0, y_min),
                    egui::pos2(rect.left() + 2.0 + w, y_max),
                );
                painter.rect_filled(bar, 2.0, color);
            }
        };

        let mid = rect.center().y;
        draw_bar(rect.top() + 3.0, mid - 1.0, peak_l);
        draw_bar(mid + 1.0, rect.bottom() - 3.0, peak_r);
    }
}

impl eframe::App for DawStreamApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if ctx.input(|i| i.key_pressed(egui::Key::H)) {
            self.hide_toolbar = !self.hide_toolbar;
        }

        if !self.capturer.has_active_target() {
            if self.mirror_texture.is_some() {
                self.mirror_texture = None;
            }
            self.selected_target = None;
        } else if let Some(frame) = self.capturer.take_if_newer(self.last_frame_seq) {
            self.last_frame_seq = frame.seq;
            let color_image =
                egui::ColorImage::from_rgba_unmultiplied([frame.width, frame.height], &frame.rgba);
            match &mut self.mirror_texture {
                Some(tex) => tex.set(color_image, egui::TextureOptions::LINEAR),
                None => {
                    self.mirror_texture = Some(ctx.load_texture(
                        "daw_mirror",
                        color_image,
                        egui::TextureOptions::LINEAR,
                    ));
                }
            }
        }

        let audio_status = self.audio_engine.status();
        self.smooth_peak_l = audio_status.peak_l.max(self.smooth_peak_l * 0.86);
        self.smooth_peak_r = audio_status.peak_r.max(self.smooth_peak_r * 0.86);

        let selected_device_tier = self
            .devices
            .iter()
            .find(|d| Some(&d.id) == self.selected_device_id.as_ref())
            .map(|d| d.tier)
            .unwrap_or(DeviceTier::Secondary);

        if !self.hide_toolbar {
            egui::TopBottomPanel::top("toolbar")
                .frame(
                    egui::Frame::none()
                        .fill(egui::Color32::from_rgb(24, 26, 33))
                        .inner_margin(egui::Margin::symmetric(10.0, 6.0)),
                )
                .show(ctx, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;

                        // Connection status indicator
                        let (dot_color, status_text) = if audio_status.plugin_connected {
                            if audio_status.plugin_muted {
                                (
                                    egui::Color32::from_rgb(240, 190, 60),
                                    format!("PLUGIN MUTED ({} Hz)", audio_status.sample_rate),
                                )
                            } else {
                                (
                                    egui::Color32::from_rgb(65, 215, 130),
                                    format!("LIVE ({} Hz)", audio_status.sample_rate),
                                )
                            }
                        } else {
                            (
                                egui::Color32::from_rgb(130, 135, 150),
                                "Waiting for Plugin".to_string(),
                            )
                        };

                        ui.horizontal(|ui| {
                            let (r, _) =
                                ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                            ui.painter().circle_filled(r.center(), 4.5, dot_color);
                            ui.strong(status_text);
                        });

                        ui.separator();

                        // Window selector
                        ui.label("Window:");
                        let current_win_label = self
                            .windows
                            .iter()
                            .find(|w| Some((w.hwnd, w.pid)) == self.selected_target)
                            .map(|w| truncate_str(&w.title, 28))
                            .unwrap_or_else(|| "Select window...".into());

                        egui::ComboBox::from_id_salt("window_combo")
                            .selected_text(current_win_label)
                            .width(200.0)
                            .show_ui(ui, |ui| {
                                if ui
                                    .selectable_label(self.selected_target.is_none(), "(No Video)")
                                    .clicked()
                                {
                                    self.selected_target = None;
                                    self.capturer.set_target(None);
                                    self.mirror_texture = None;
                                }
                                for win in &self.windows {
                                    let label = if win.is_daw {
                                        format!("[DAW] {}", truncate_str(&win.title, 40))
                                    } else {
                                        truncate_str(&win.title, 44)
                                    };
                                    if ui
                                        .selectable_label(
                                            self.selected_target == Some((win.hwnd, win.pid)),
                                            label,
                                        )
                                        .clicked()
                                    {
                                        self.selected_target = Some((win.hwnd, win.pid));
                                        self.capturer.set_target(self.selected_target);
                                        self.mirror_texture = None;
                                    }
                                }
                            });

                        if ui.button("Refresh").on_hover_text("Refresh open windows").clicked() {
                            self.refresh_windows();
                        }

                        ui.separator();

                        // Audio sink selector
                        ui.label("Audio Output:");
                        let current_dev_label = self
                            .devices
                            .iter()
                            .find(|d| Some(&d.id) == self.selected_device_id.as_ref())
                            .map(|d| truncate_str(&d.name, 26))
                            .unwrap_or_else(|| "Select output...".into());

                        egui::ComboBox::from_id_salt("device_combo")
                            .selected_text(current_dev_label)
                            .width(210.0)
                            .show_ui(ui, |ui| {
                                for dev in &self.devices {
                                    let label = match dev.tier {
                                        DeviceTier::SilentDigital => {
                                            format!("[Recommended] {}", dev.name)
                                        }
                                        DeviceTier::Secondary => dev.name.clone(),
                                        DeviceTier::ProInterface => {
                                            format!("[!] {} (Audio Interface)", dev.name)
                                        }
                                    };
                                    if ui
                                        .selectable_label(
                                            self.selected_device_id.as_ref() == Some(&dev.id),
                                            label,
                                        )
                                        .clicked()
                                    {
                                        self.selected_device_id = Some(dev.id.clone());
                                        self.audio_engine.set_device(Some(dev.id.clone()));
                                        self.save_config();
                                    }
                                }
                            });

                        if ui
                            .button("Refresh")
                            .on_hover_text("Refresh audio output devices")
                            .clicked()
                        {
                            self.refresh_devices();
                        }

                        if selected_device_tier == DeviceTier::ProInterface {
                            ui.colored_label(
                                egui::Color32::from_rgb(245, 190, 65),
                                "[!] Double audio risk",
                            )
                            .on_hover_text(
                                "Your main audio interface is selected. Choose a [Recommended] silent/unused output to avoid hearing double audio.",
                            );
                        }

                        ui.separator();

                        // VU Meter + Mute + Volume
                        Self::draw_vu_meter(ui, self.smooth_peak_l, self.smooth_peak_r);

                        let mute_btn = if self.muted {
                            egui::Button::new(
                                egui::RichText::new("MUTED").color(egui::Color32::WHITE).strong(),
                            )
                            .fill(egui::Color32::from_rgb(200, 55, 65))
                        } else {
                            egui::Button::new("AUDIO")
                        };

                        if ui
                            .add(mute_btn)
                            .on_hover_text("Mute/Unmute stream audio")
                            .clicked()
                        {
                            self.muted = !self.muted;
                            self.audio_engine.set_muted(self.muted);
                            self.save_config();
                        }

                        let mut pct = (self.volume * 100.0).round();
                        let slider_resp = ui.add(
                            egui::Slider::new(&mut pct, 0.0..=150.0)
                                .suffix("%")
                                .show_value(true)
                                .max_decimals(0),
                        );
                        if slider_resp.changed() {
                            self.volume = pct / 100.0;
                            self.audio_engine.set_volume(self.volume);
                        }
                        if slider_resp.drag_stopped() || (slider_resp.changed() && !slider_resp.dragged()) {
                            self.save_config();
                        }

                        // Right-aligned FPS & Hide bar button
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .button("Hide Bar (H)")
                                .on_hover_text("Hide top bar for a clean Discord stream (press H to restore)")
                                .clicked()
                            {
                                self.hide_toolbar = true;
                            }
                            let fps = self.capturer.fps();
                            if fps > 0.1 {
                                ui.weak(format!("{fps:.0} FPS"));
                            }
                        });
                    });

                    if let Some(err) = &audio_status.last_error {
                        ui.colored_label(
                            egui::Color32::from_rgb(240, 90, 90),
                            format!("Audio Error: {err}"),
                        );
                    }
                });
        }

        // Main mirror viewport
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(egui::Color32::from_rgb(12, 13, 16)))
            .show(ctx, |ui| {
                if let Some(tex) = &self.mirror_texture {
                    let avail = ui.available_size();
                    let tex_size = tex.size_vec2();
                    let scale = (avail.x / tex_size.x).min(avail.y / tex_size.y);
                    let display_size = tex_size * scale;

                    ui.centered_and_justified(|ui| {
                        ui.image((tex.id(), display_size));
                    });
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.vertical_centered(|ui| {
                            ui.add_space(40.0);
                            ui.heading("DAWStream Ready");
                            ui.add_space(12.0);
                            ui.label("1. Open your DAW (FL Studio, Ableton Live, Cubase, REAPER, Studio One, Bitwig, etc.) and add DAWStream (VST3/CLAP) to your Master channel.");
                            ui.label("2. Select your DAW window above and choose a [Recommended] silent audio output.");
                            ui.label("3. In Discord: Share Screen -> Applications -> select \"DAWStream\".");
                            ui.add_space(12.0);
                            ui.weak("Tip: Press 'H' anytime to hide/show the top bar for a clean stream.");
                        });
                    });
                }
            });

        // Floating "Show Bar" pill when toolbar is hidden and mouse hovers near top edge
        if self.hide_toolbar {
            let near_top = ctx
                .input(|i| i.pointer.hover_pos())
                .map(|p| p.y < 36.0)
                .unwrap_or(false);
            if near_top {
                egui::Area::new(egui::Id::new("show_bar_overlay"))
                    .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 6.0))
                    .show(ctx, |ui| {
                        if ui.button("Show Bar (H)").clicked() {
                            self.hide_toolbar = false;
                        }
                    });
            }
        }

        if self.selected_target.is_some() {
            ctx.request_repaint_after(Duration::from_millis(16));
        } else {
            ctx.request_repaint_after(Duration::from_millis(66));
        }
    }
}

fn truncate_str(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}
