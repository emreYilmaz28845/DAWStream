use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dawstream_ipc::SharedAudioRing;
use nih_plug::prelude::*;
use nih_plug_egui::{create_egui_editor, egui, EguiState};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
};
use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::{
    FOLDERID_LocalAppData, FOLDERID_ProgramFiles, FOLDERID_RoamingAppData, SHGetKnownFolderPath,
};
use windows_sys::core::GUID;

const DRIVE_REMOTE: u32 = 4;
const MAX_CONFIG_BYTES: u64 = 64 * 1024;

pub struct DawStreamPlugin {
    params: Arc<DawStreamParams>,
    ring: Option<Arc<SharedAudioRing>>,
    instance_id: Arc<AtomicU64>,
    /// Host sample rate from `initialize()`, published by whichever instance owns the stream.
    sample_rate: u32,
}

impl Drop for DawStreamPlugin {
    fn drop(&mut self) {
        if let Some(ring) = &self.ring {
            let id = self.instance_id.load(Ordering::Acquire);
            if id != 0 {
                ring.release_owner(id);
            }
        }
    }
}

#[derive(Params)]
struct DawStreamParams {
    #[persist = "editor-state"]
    editor_state: Arc<EguiState>,

    /// Stream output gain (does NOT alter the DAW's own audio output).
    #[id = "gain"]
    pub stream_gain: FloatParam,

    /// Mutes the stream sent to DAWStream app (DAW audio remains unaffected).
    #[id = "mute"]
    pub mute_stream: BoolParam,
}

impl Default for DawStreamParams {
    fn default() -> Self {
        Self {
            editor_state: EguiState::from_size(360, 118),

            stream_gain: FloatParam::new(
                "Stream Gain",
                util::db_to_gain(0.0),
                FloatRange::Skewed {
                    min: util::db_to_gain(-60.0),
                    max: util::db_to_gain(12.0),
                    factor: FloatRange::gain_skew_factor(-60.0, 12.0),
                },
            )
            .with_smoother(SmoothingStyle::Logarithmic(20.0))
            .with_unit(" dB")
            .with_value_to_string(formatters::v2s_f32_gain_to_db(1))
            .with_string_to_value(formatters::s2v_f32_gain_to_db()),

            mute_stream: BoolParam::new("Mute Stream", false),
        }
    }
}

impl Default for DawStreamPlugin {
    fn default() -> Self {
        let ring = SharedAudioRing::open_or_create().ok().map(Arc::new);
        let id = ring.as_ref().map(|r| r.alloc_instance_id()).unwrap_or(0);
        Self {
            params: Arc::new(DawStreamParams::default()),
            ring,
            instance_id: Arc::new(AtomicU64::new(id)),
            sample_rate: 48_000,
        }
    }
}

struct EditorUiState {
    ring: Option<Arc<SharedAudioRing>>,
    instance_id: Arc<AtomicU64>,
    last_app_hb: u64,
    last_app_hb_time: Option<Instant>,
    app_connected: bool,
    last_plugin_hb: u64,
    last_plugin_hb_time: Instant,
    smooth_l: f32,
    smooth_r: f32,
    launch_error: Option<(String, Instant)>,
}

/// Returns `true` if the host DAW process is running with an elevated (Administrator) token.
/// Fails closed (`true`) if the token cannot be queried.
fn is_process_elevated() -> bool {
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return true;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut ret_len: u32 = 0;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut TOKEN_ELEVATION as *mut _,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret_len,
        );
        CloseHandle(token);
        if ok == 0 {
            true
        } else {
            elevation.TokenIsElevated != 0
        }
    }
}

/// Resolves a Windows Known Folder via `SHGetKnownFolderPath` instead of trusting
/// user-overridable environment variables (`%ProgramFiles%`, `%APPDATA%`, `%LOCALAPPDATA%`).
fn known_folder(id: &GUID) -> Option<PathBuf> {
    unsafe {
        let mut raw: *mut u16 = std::ptr::null_mut();
        let hr = SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut raw);
        if hr != 0 || raw.is_null() {
            return None;
        }
        let mut len = 0usize;
        while *raw.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(raw, len);
        let s = String::from_utf16_lossy(slice);
        CoTaskMemFree(raw as *const _);
        if s.is_empty() {
            None
        } else {
            Some(PathBuf::from(s))
        }
    }
}

/// Returns `false` if the path's drive root is a remote/network drive (`DRIVE_REMOTE`).
fn is_not_remote_drive(p: &Path) -> bool {
    let s = p.to_string_lossy();
    let bytes = s.as_bytes();
    if bytes.len() < 3 || bytes[1] != b':' || (bytes[2] != b'\\' && bytes[2] != b'/') {
        return false;
    }
    let root: [u16; 4] = [bytes[0] as u16, b':' as u16, b'\\' as u16, 0];
    let dtype = unsafe { GetDriveTypeW(root.as_ptr()) };
    dtype != DRIVE_REMOTE
}

fn is_allowed_exe_name(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .map(|n| {
            let lower = n.to_ascii_lowercase();
            lower == "dawstream.exe" || lower == "dawstream-app.exe"
        })
        .unwrap_or(false)
}

/// Validates that a candidate path is a local, absolute, non-UNC, non-remote executable
/// named `DAWStream.exe` (or `dawstream-app.exe`), re-checking the basename AFTER
/// canonicalization to defeat symlink-name bypasses.
fn validate_companion_exe(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.starts_with(r"\\") || trimmed.starts_with("//") {
        return None;
    }

    let p = Path::new(trimmed);
    if !p.is_absolute() || !is_not_remote_drive(p) || !is_allowed_exe_name(p) {
        return None;
    }

    let canonical = std::fs::canonicalize(p).ok()?;
    let canon_str = canonical.to_string_lossy();
    if canon_str.to_ascii_uppercase().starts_with(r"\\?\UNC\") {
        return None;
    }
    // Re-check the drive AFTER resolving symlinks/junctions
    let plain = canon_str.strip_prefix(r"\\?\").unwrap_or(&canon_str);
    if !is_not_remote_drive(Path::new(plain)) {
        return None;
    }

    // Re-check file name AFTER resolving symlinks/junctions (closes MEDIUM #13)
    if !is_allowed_exe_name(&canonical) || !canonical.is_file() {
        return None;
    }

    Some(canonical)
}

fn find_dawstream_exe(elevated: bool) -> Option<PathBuf> {
    // 1. Always check admin-protected Program Files via SHGetKnownFolderPath (never %ProgramFiles%)
    let pf_base = known_folder(&FOLDERID_ProgramFiles)?;
    let pf_exe = pf_base.join("DAWStream").join("DAWStream.exe");
    if let Some(valid) = pf_exe.to_str().and_then(validate_companion_exe) {
        // A link inside Program Files must still resolve to somewhere inside Program Files
        let inside_pf = std::fs::canonicalize(&pf_base)
            .map(|pf| valid.starts_with(pf))
            .unwrap_or(false);
        if inside_pf {
            return Some(valid);
        }
    }

    // Security guard: if the DAW is running elevated (Administrator),
    // NEVER launch an executable from a user-writable directory.
    if elevated {
        return None;
    }

    // 2. Per-user standard install location (via SHGetKnownFolderPath)
    if let Some(local_app) = known_folder(&FOLDERID_LocalAppData) {
        let p = local_app.join("DAWStream").join("DAWStream.exe");
        if let Some(valid) = p.to_str().and_then(validate_companion_exe) {
            return Some(valid);
        }
    }

    // 3. Registered path saved by DAWStream.exe (only when DAW is non-elevated, size-capped)
    if let Some(appdata) = known_folder(&FOLDERID_RoamingAppData) {
        if is_not_remote_drive(&appdata) {
            let cfg_path = appdata.join("DAWStream").join("config.json");
            if let Ok(meta) = std::fs::metadata(&cfg_path) {
                if meta.is_file() && meta.len() <= MAX_CONFIG_BYTES {
                    if let Ok(text) = std::fs::read_to_string(&cfg_path) {
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
                            if let Some(exe_str) = val.get("app_exe_path").and_then(|v| v.as_str()) {
                                if let Some(valid) = validate_companion_exe(exe_str) {
                                    return Some(valid);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    None
}

fn launch_companion_app() -> Result<(), String> {
    let elevated = is_process_elevated();
    if let Some(exe) = find_dawstream_exe(elevated) {
        let mut cmd = Command::new(&exe);
        // Don't inherit the DAW's (possibly untrusted) working directory
        if let Some(dir) = exe.parent() {
            cmd.current_dir(dir);
        }
        cmd.spawn()
            .map_err(|e| format!("Launch error: {e}"))?;
        return Ok(());
    }

    if elevated {
        Err("DAW is elevated: install DAWStream to Program Files (run Install.bat as Admin) or open DAWStream manually".into())
    } else {
        Err("DAWStream.exe not found — open it once first".into())
    }
}

/// Atomically ensures `slot` holds a non-zero instance ID (race-free CAS).
fn ensure_instance_id(ring: &SharedAudioRing, slot: &AtomicU64) -> u64 {
    let existing = slot.load(Ordering::Acquire);
    if existing != 0 {
        return existing;
    }
    let fresh = ring.alloc_instance_id();
    match slot.compare_exchange(0, fresh, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => fresh,
        Err(winner) => winner,
    }
}

impl Plugin for DawStreamPlugin {
    const NAME: &'static str = "DAWStream";
    const VENDOR: &'static str = "DAWStream";
    const URL: &'static str = "https://github.com/emreYilmaz28845/DAWStream";
    const EMAIL: &'static str = "";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(2),
            main_output_channels: NonZeroU32::new(2),
            aux_input_ports: &[],
            aux_output_ports: &[],
            names: PortNames::const_default(),
        },
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(1),
            main_output_channels: NonZeroU32::new(1),
            aux_input_ports: &[],
            aux_output_ports: &[],
            names: PortNames::const_default(),
        },
    ];

    const MIDI_INPUT: MidiConfig = MidiConfig::None;
    const MIDI_OUTPUT: MidiConfig = MidiConfig::None;
    const SAMPLE_ACCURATE_AUTOMATION: bool = false;

    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        // NOTE: never mutate the process environment here — this DLL runs inside a
        // multi-threaded host. (The plugin editor uses baseview's own WGL, not glutin.)
        let params = self.params.clone();
        let ring = self
            .ring
            .clone()
            .or_else(|| SharedAudioRing::open_or_create().ok().map(Arc::new));

        if let Some(ref r) = ring {
            ensure_instance_id(r, &self.instance_id);
        }

        let initial_hb = ring.as_ref().map(|r| r.app_heartbeat()).unwrap_or(0);
        let initial_plugin_hb = ring.as_ref().map(|r| r.heartbeat()).unwrap_or(0);
        let ui_state = EditorUiState {
            ring,
            instance_id: self.instance_id.clone(),
            last_app_hb: initial_hb,
            last_app_hb_time: None,
            app_connected: false,
            last_plugin_hb: initial_plugin_hb,
            last_plugin_hb_time: Instant::now(),
            smooth_l: 0.0,
            smooth_r: 0.0,
            launch_error: None,
        };

        create_egui_editor(
            self.params.editor_state.clone(),
            ui_state,
            |ctx, _| {
                let mut visuals = egui::Visuals::dark();
                visuals.panel_fill = egui::Color32::from_rgb(24, 26, 33);
                visuals.window_fill = egui::Color32::from_rgb(24, 26, 33);
                ctx.set_visuals(visuals);
            },
            move |egui_ctx, setter, state| {
                let my_id = state.instance_id.load(Ordering::Acquire);
                let (peak_l, peak_r, sr, is_owner) = if let Some(ring) = &state.ring {
                    let hb = ring.app_heartbeat();
                    if hb != state.last_app_hb {
                        state.last_app_hb = hb;
                        state.last_app_hb_time = Some(Instant::now());
                        state.app_connected = true;
                    } else if state
                        .last_app_hb_time
                        .map(|t| t.elapsed() > Duration::from_millis(800))
                        .unwrap_or(true)
                    {
                        state.app_connected = false;
                    }
                    // Peaks are only refreshed by process(); if the host stopped calling it
                    // (bypass, disabled track, stopped device) treat them as silence.
                    let plugin_hb = ring.heartbeat();
                    if plugin_hb != state.last_plugin_hb {
                        state.last_plugin_hb = plugin_hb;
                        state.last_plugin_hb_time = Instant::now();
                    }
                    let processing =
                        state.last_plugin_hb_time.elapsed() < Duration::from_millis(150);

                    let owner = ring.active_owner_id();
                    let is_owner = owner == 0 || (my_id != 0 && owner == my_id);
                    let (l, r) = if is_owner && processing {
                        ring.peaks()
                    } else {
                        (0.0, 0.0)
                    };
                    (l, r, ring.sample_rate(), is_owner)
                } else {
                    (0.0, 0.0, 48_000, true)
                };

                state.smooth_l = peak_l.max(state.smooth_l * 0.86);
                state.smooth_r = peak_r.max(state.smooth_r * 0.86);

                if let Some((_, ts)) = &state.launch_error {
                    if ts.elapsed() > Duration::from_secs(4) {
                        state.launch_error = None;
                    }
                }

                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::NONE
                            .fill(egui::Color32::from_rgb(24, 26, 33))
                            .inner_margin(egui::Margin::symmetric(12, 10)),
                    )
                    .show(egui_ctx, |ui| {
                        // Row 1: Status dot + sample rate | Open App / Take Over button
                        ui.horizontal(|ui| {
                            let (dot_color, status_text) = if !is_owner {
                                (
                                    egui::Color32::from_rgb(240, 190, 60),
                                    "Standby (Another instance active)".to_string(),
                                )
                            } else if state.app_connected {
                                (
                                    egui::Color32::from_rgb(65, 215, 130),
                                    format!("LIVE ({sr} Hz)"),
                                )
                            } else {
                                (
                                    egui::Color32::from_rgb(130, 135, 150),
                                    "App Offline".to_string(),
                                )
                            };

                            let (r, _) = ui.allocate_exact_size(
                                egui::vec2(10.0, 10.0),
                                egui::Sense::hover(),
                            );
                            ui.painter().circle_filled(r.center(), 4.5, dot_color);
                            ui.strong(status_text);

                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if !is_owner {
                                    if ui.button("Take Over").clicked() {
                                        if let Some(ring) = &state.ring {
                                            let id = ensure_instance_id(ring, &state.instance_id);
                                            ring.force_claim_owner(id);
                                        }
                                    }
                                } else {
                                    let label = if state.app_connected {
                                        "App Running"
                                    } else {
                                        "Open App"
                                    };
                                    let resp = ui
                                        .add_enabled(!state.app_connected, egui::Button::new(label));
                                    if resp.clicked() {
                                        if let Err(err) = launch_companion_app() {
                                            state.launch_error = Some((err, Instant::now()));
                                        }
                                    }
                                }
                            });
                        });

                        ui.add_space(8.0);

                        // Row 2: Compact Stereo VU Meter + dB readout
                        ui.horizontal(|ui| {
                            let meter_w = (ui.available_width() - 56.0).max(80.0);
                            draw_stereo_vu(ui, meter_w, state.smooth_l, state.smooth_r);

                            let peak_max = state.smooth_l.max(state.smooth_r);
                            let peak_db = if peak_max > 1e-5 {
                                format!("{:+.1} dB", 20.0 * peak_max.log10())
                            } else {
                                "-inf dB".to_string()
                            };
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.monospace(
                                    egui::RichText::new(peak_db)
                                        .size(11.0)
                                        .color(egui::Color32::from_rgb(170, 178, 195)),
                                );
                            });
                        });

                        ui.add_space(8.0);
                        ui.separator();
                        ui.add_space(6.0);

                        // Row 3: Mute | Gain slider | 0 dB reset
                        ui.horizontal(|ui| {
                            let is_muted = params.mute_stream.value();
                            let mute_btn = if is_muted {
                                egui::Button::new(
                                    egui::RichText::new("MUTED")
                                        .color(egui::Color32::WHITE)
                                        .strong(),
                                )
                                .fill(egui::Color32::from_rgb(200, 55, 65))
                            } else {
                                egui::Button::new("AUDIO")
                            };

                            if ui.add(mute_btn).clicked() {
                                setter.begin_set_parameter(&params.mute_stream);
                                setter.set_parameter(&params.mute_stream, !is_muted);
                                setter.end_set_parameter(&params.mute_stream);
                            }

                            let current_gain = params.stream_gain.value();
                            let mut db = util::gain_to_db(current_gain).clamp(-60.0, 12.0);
                            if ui
                                .add(
                                    egui::Slider::new(&mut db, -60.0..=12.0)
                                        .suffix(" dB")
                                        .fixed_decimals(1),
                                )
                                .changed()
                            {
                                setter.begin_set_parameter(&params.stream_gain);
                                setter.set_parameter(&params.stream_gain, util::db_to_gain(db));
                                setter.end_set_parameter(&params.stream_gain);
                            }

                            if ui.small_button("0 dB").clicked() {
                                setter.begin_set_parameter(&params.stream_gain);
                                setter.set_parameter(&params.stream_gain, 1.0);
                                setter.end_set_parameter(&params.stream_gain);
                            }
                        });

                        if let Some((err, _)) = &state.launch_error {
                            ui.add_space(4.0);
                            ui.colored_label(egui::Color32::from_rgb(240, 100, 100), err);
                        }
                    });

                egui_ctx.request_repaint_after(Duration::from_millis(33));
            },
        )
    }

    fn initialize(
        &mut self,
        _audio_io_layout: &AudioIOLayout,
        buffer_config: &BufferConfig,
        _context: &mut impl InitContext<Self>,
    ) -> bool {
        if self.ring.is_none() {
            self.ring = SharedAudioRing::open_or_create().ok().map(Arc::new);
        }
        self.sample_rate = buffer_config.sample_rate.round() as u32;
        if let Some(ring) = &self.ring {
            let id = ensure_instance_id(ring, &self.instance_id);
            if ring.is_or_claim_owner(id) {
                ring.set_sample_rate(self.sample_rate);
            }
        }
        true
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        _context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        let Some(ring) = &self.ring else {
            return ProcessStatus::Normal;
        };

        // Only the active owner instance writes to shared memory (prevents multi-instance glitches)
        let id = self.instance_id.load(Ordering::Acquire);
        if !ring.is_or_claim_owner(id) {
            return ProcessStatus::Normal;
        }
        // Always publish OUR rate: ownership may have moved here via Take Over / lease expiry
        if ring.sample_rate() != self.sample_rate {
            ring.set_sample_rate(self.sample_rate);
        }

        let num_samples = buffer.samples();
        if num_samples == 0 {
            return ProcessStatus::Normal;
        }

        let gain = self.params.stream_gain.smoothed.next_step(num_samples as u32);
        let muted = self.params.mute_stream.value();

        let slices = buffer.as_slice_immutable();
        if slices.len() >= 2 {
            ring.write_planar(slices[0], slices[1], gain, muted);
        } else if slices.len() == 1 {
            ring.write_planar(slices[0], slices[0], gain, muted);
        }

        ProcessStatus::Normal
    }
}

fn draw_stereo_vu(ui: &mut egui::Ui, width: f32, peak_l: f32, peak_r: f32) {
    let height = 20.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let painter = ui.painter();

    painter.rect_filled(rect, 3.0, egui::Color32::from_rgb(16, 17, 22));

    let draw_ch = |y_min: f32, y_max: f32, level: f32| {
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
    draw_ch(rect.top() + 3.0, mid - 1.0, peak_l);
    draw_ch(mid + 1.0, rect.bottom() - 3.0, peak_r);
}

impl Vst3Plugin for DawStreamPlugin {
    const VST3_CLASS_ID: [u8; 16] = *b"DAWStreamVst3Plg";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] =
        &[Vst3SubCategory::Fx, Vst3SubCategory::Tools];
}

impl ClapPlugin for DawStreamPlugin {
    const CLAP_ID: &'static str = "com.dawstream.plugin";
    const CLAP_DESCRIPTION: Option<&'static str> =
        Some("Streams DAW ASIO audio directly to DAWStream for Discord screen sharing");
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[
        ClapFeature::AudioEffect,
        ClapFeature::Stereo,
        ClapFeature::Utility,
    ];
}

nih_export_vst3!(DawStreamPlugin);
nih_export_clap!(DawStreamPlugin);
