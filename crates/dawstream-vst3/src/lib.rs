use std::num::NonZeroU32;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dawstream_ipc::SharedAudioRing;
use nih_plug::prelude::*;
use nih_plug_egui::{create_egui_editor, egui, EguiState};

pub struct DawStreamPlugin {
    params: Arc<DawStreamParams>,
    ring: Option<Arc<SharedAudioRing>>,
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
        Self {
            params: Arc::new(DawStreamParams::default()),
            ring: SharedAudioRing::open_or_create().ok().map(Arc::new),
        }
    }
}

struct EditorUiState {
    ring: Option<Arc<SharedAudioRing>>,
    last_app_hb: u64,
    last_app_hb_time: Instant,
    app_connected: bool,
    smooth_l: f32,
    smooth_r: f32,
    launch_error: Option<(String, Instant)>,
}

fn find_dawstream_exe() -> Option<PathBuf> {
    // 1. Registered path saved by DAWStream.exe on startup
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let cfg_path = PathBuf::from(appdata).join("DAWStream").join("config.json");
        if let Ok(text) = std::fs::read_to_string(cfg_path) {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(exe_str) = val.get("app_exe_path").and_then(|v| v.as_str()) {
                    let p = PathBuf::from(exe_str);
                    if p.exists() {
                        return Some(p);
                    }
                }
            }
        }
    }

    // 2. Standard install locations
    if let Some(local_app) = std::env::var_os("LOCALAPPDATA") {
        let p = PathBuf::from(local_app)
            .join("DAWStream")
            .join("DAWStream.exe");
        if p.exists() {
            return Some(p);
        }
    }

    let pf = PathBuf::from(r"C:\Program Files\DAWStream\DAWStream.exe");
    if pf.exists() {
        return Some(pf);
    }

    None
}

fn launch_companion_app() -> Result<(), String> {
    if let Some(exe) = find_dawstream_exe() {
        Command::new(&exe)
            .spawn()
            .map_err(|e| format!("Launch error: {e}"))?;
        return Ok(());
    }

    // Fallback: Desktop shortcut created by Install.bat
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        let lnk = PathBuf::from(profile).join("Desktop").join("DAWStream.lnk");
        if lnk.exists() {
            Command::new("cmd")
                .args(["/C", "start", "", &lnk.to_string_lossy()])
                .spawn()
                .map_err(|e| format!("Launch error: {e}"))?;
            return Ok(());
        }
    }

    Err("DAWStream.exe not found — open it once first".into())
}

impl Plugin for DawStreamPlugin {
    const NAME: &'static str = "DAWStream";
    const VENDOR: &'static str = "DAWStream";
    const URL: &'static str = "https://github.com/dawstream/dawstream";
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
        let params = self.params.clone();
        let ring = self
            .ring
            .clone()
            .or_else(|| SharedAudioRing::open_or_create().ok().map(Arc::new));

        let initial_hb = ring.as_ref().map(|r| r.app_heartbeat()).unwrap_or(0);
        let ui_state = EditorUiState {
            ring,
            last_app_hb: initial_hb,
            last_app_hb_time: Instant::now() - Duration::from_secs(2),
            app_connected: false,
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
                let (peak_l, peak_r, sr) = if let Some(ring) = &state.ring {
                    let hb = ring.app_heartbeat();
                    if hb != state.last_app_hb {
                        state.last_app_hb = hb;
                        state.last_app_hb_time = Instant::now();
                        state.app_connected = true;
                    } else if state.last_app_hb_time.elapsed() > Duration::from_millis(800) {
                        state.app_connected = false;
                    }
                    let (l, r) = ring.peaks();
                    (l, r, ring.sample_rate())
                } else {
                    (0.0, 0.0, 48_000)
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
                        // Row 1: Status dot + sample rate | Open App button
                        ui.horizontal(|ui| {
                            let (dot_color, status_text) = if state.app_connected {
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
        if let Some(ring) = &self.ring {
            ring.set_sample_rate(buffer_config.sample_rate.round() as u32);
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
