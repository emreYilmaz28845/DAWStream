use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use dawstream_ipc::{SharedAudioRing, MAX_SAMPLE_RATE, MIN_SAMPLE_RATE};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, S_FALSE, S_OK, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eRender, IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    DEVICE_STATE_ACTIVE, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0,
};
use windows::Win32::Media::KernelStreaming::{
    SPEAKER_FRONT_LEFT, SPEAKER_FRONT_RIGHT, WAVE_FORMAT_EXTENSIBLE,
};
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DeviceTier {
    /// Digital/Optical/S/PDIF/Virtual sinks — completely silent locally, ideal for Discord.
    SilentDigital = 0,
    /// Secondary outputs (HDMI/DisplayPort monitors, onboard speakers).
    Secondary = 1,
    /// Dedicated ASIO / pro audio interfaces (may cause double monitoring).
    ProInterface = 2,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioOutputDevice {
    pub id: String,
    pub name: String,
    pub tier: DeviceTier,
}

const SILENT_DIGITAL_KEYWORDS: &[&str] = &[
    "digital output",
    "s/pdif",
    "spdif",
    "optical",
    "vb-audio",
    "cable input",
    "virtual audio",
    "steam streaming",
];

const PRO_INTERFACE_KEYWORDS: &[&str] = &[
    "topping",
    "e4x4",
    "e2x2",
    "focusrite",
    "scarlett",
    "audient",
    "steinberg",
    "ur22",
    "ur44",
    "motu",
    "uad",
    "apollo",
    "rme",
    "presonus",
    "behringer",
    "umc",
    "ssl",
    "volt",
    "komplete",
    "asio",
];

fn classify_device(name: &str) -> DeviceTier {
    let lower = name.to_lowercase();
    if PRO_INTERFACE_KEYWORDS.iter().any(|kw| lower.contains(kw)) {
        DeviceTier::ProInterface
    } else if SILENT_DIGITAL_KEYWORDS.iter().any(|kw| lower.contains(kw)) {
        DeviceTier::SilentDigital
    } else {
        DeviceTier::Secondary
    }
}

unsafe fn pwstr_to_string_and_free(pwstr: PWSTR) -> String {
    if pwstr.is_null() {
        return String::new();
    }
    let s = pwstr.to_string().unwrap_or_default();
    CoTaskMemFree(Some(pwstr.0 as *const _));
    s
}

/// Balanced RAII guard for COM initialization: only calls `CoUninitialize`
/// if `CoInitializeEx` succeeded with `S_OK` or `S_FALSE`.
struct ComGuard(bool);

impl ComGuard {
    fn init_mta() -> Self {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Self(hr == S_OK || hr == S_FALSE)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

/// Lists all active Windows WASAPI output (render) endpoints.
pub fn list_output_devices() -> Vec<AudioOutputDevice> {
    let mut devices = Vec::new();
    let _com = ComGuard::init_mta();

    unsafe {
        let Ok(enumerator): windows::core::Result<IMMDeviceEnumerator> =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
        else {
            return devices;
        };

        let Ok(collection) = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) else {
            return devices;
        };

        let count = collection.GetCount().unwrap_or(0);
        for i in 0..count {
            let Ok(device) = collection.Item(i) else {
                continue;
            };
            let Ok(id_pwstr) = device.GetId() else {
                continue;
            };
            let id = pwstr_to_string_and_free(id_pwstr);

            let name = device
                .OpenPropertyStore(STGM_READ)
                .ok()
                .and_then(|store| store.GetValue(&PKEY_Device_FriendlyName).ok())
                .and_then(|pv| PropVariantToStringAlloc(&pv).ok())
                .map(|pw| pwstr_to_string_and_free(pw))
                .unwrap_or_else(|| format!("Audio Output #{i}"));

            let tier = classify_device(&name);
            devices.push(AudioOutputDevice { id, name, tier });
        }
    }

    // Order: SilentDigital -> Secondary -> ProInterface
    devices.sort_by(|a, b| {
        a.tier
            .cmp(&b.tier)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    devices
}

#[derive(Clone, Debug)]
pub struct AudioStatus {
    pub plugin_connected: bool,
    pub plugin_muted: bool,
    pub sample_rate: u32,
    pub peak_l: f32,
    pub peak_r: f32,
    pub active_device_name: String,
    pub last_error: Option<String>,
}

impl Default for AudioStatus {
    fn default() -> Self {
        Self {
            plugin_connected: false,
            plugin_muted: false,
            sample_rate: 48_000,
            peak_l: 0.0,
            peak_r: 0.0,
            active_device_name: String::new(),
            last_error: None,
        }
    }
}

pub struct AudioEngine {
    running: Arc<AtomicBool>,
    volume_bits: Arc<AtomicU32>,
    app_muted: Arc<AtomicBool>,
    desired_device_id: Arc<Mutex<Option<String>>>,
    device_changed: Arc<AtomicBool>,
    status: Arc<Mutex<AudioStatus>>,
    thread: Option<JoinHandle<()>>,
}

impl AudioEngine {
    pub fn start(initial_device_id: Option<String>) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let volume_bits = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let app_muted = Arc::new(AtomicBool::new(false));
        let desired_device_id = Arc::new(Mutex::new(initial_device_id));
        let device_changed = Arc::new(AtomicBool::new(true));
        let status = Arc::new(Mutex::new(AudioStatus::default()));

        let t_run = running.clone();
        let t_vol = volume_bits.clone();
        let t_mute = app_muted.clone();
        let t_dev = desired_device_id.clone();
        let t_chg = device_changed.clone();
        let t_stat = status.clone();

        let thread = thread::Builder::new()
            .name("dawstream-wasapi".into())
            .spawn(move || {
                audio_thread_main(t_run, t_vol, t_mute, t_dev, t_chg, t_stat);
            })
            .expect("failed to spawn audio thread");

        Self {
            running,
            volume_bits,
            app_muted,
            desired_device_id,
            device_changed,
            status,
            thread: Some(thread),
        }
    }

    pub fn set_device(&self, device_id: Option<String>) {
        if let Ok(mut guard) = self.desired_device_id.lock() {
            *guard = device_id;
        }
        self.device_changed.store(true, Ordering::Release);
    }

    pub fn set_volume(&self, vol: f32) {
        self.volume_bits
            .store(vol.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
    }

    pub fn set_muted(&self, muted: bool) {
        self.app_muted.store(muted, Ordering::Relaxed);
    }

    pub fn status(&self) -> AudioStatus {
        self.status
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default()
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

fn audio_thread_main(
    running: Arc<AtomicBool>,
    volume_bits: Arc<AtomicU32>,
    app_muted: Arc<AtomicBool>,
    desired_device_id: Arc<Mutex<Option<String>>>,
    device_changed: Arc<AtomicBool>,
    status: Arc<Mutex<AudioStatus>>,
) {
    let _com = ComGuard::init_mta();

    let ring = loop {
        if !running.load(Ordering::Acquire) {
            return;
        }
        match SharedAudioRing::open_or_create() {
            Ok(r) => break r,
            Err(e) => {
                if let Ok(mut s) = status.lock() {
                    s.last_error = Some(format!("Shared memory error (retrying): {e}"));
                }
                for _ in 0..10 {
                    if !running.load(Ordering::Acquire) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
    };

    while running.load(Ordering::Acquire) {
        device_changed.store(false, Ordering::Release);
        let dev_id = desired_device_id.lock().ok().and_then(|g| g.clone());

        let Some(dev_id) = dev_id else {
            if let Ok(mut s) = status.lock() {
                s.active_device_name = "No output selected".into();
                s.last_error = None;
            }
            thread::sleep(Duration::from_millis(100));
            continue;
        };

        let sample_rate = ring.sample_rate().clamp(MIN_SAMPLE_RATE, MAX_SAMPLE_RATE);
        if let Err(e) = run_wasapi_session(
            &ring,
            &dev_id,
            sample_rate,
            &running,
            &volume_bits,
            &app_muted,
            &device_changed,
            &status,
        ) {
            if let Ok(mut s) = status.lock() {
                s.last_error = Some(e);
            }
            // Brief backoff before retrying
            for _ in 0..10 {
                if !running.load(Ordering::Acquire) || device_changed.load(Ordering::Acquire) {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_wasapi_session(
    ring: &SharedAudioRing,
    device_id: &str,
    sample_rate: u32,
    running: &AtomicBool,
    volume_bits: &AtomicU32,
    app_muted: &AtomicBool,
    device_changed: &AtomicBool,
    status: &Mutex<AudioStatus>,
) -> Result<(), String> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| format!("MMDeviceEnumerator: {e}"))?;

        let wide_id: Vec<u16> = device_id.encode_utf16().chain(std::iter::once(0)).collect();
        let device: IMMDevice = enumerator
            .GetDevice(PCWSTR(wide_id.as_ptr()))
            .map_err(|e| format!("GetDevice: {e}"))?;

        let dev_name = device
            .OpenPropertyStore(STGM_READ)
            .ok()
            .and_then(|store| store.GetValue(&PKEY_Device_FriendlyName).ok())
            .and_then(|pv| PropVariantToStringAlloc(&pv).ok())
            .map(|pw| pwstr_to_string_and_free(pw))
            .unwrap_or_else(|| "WASAPI Device".into());

        let audio_client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| format!("Activate IAudioClient: {e}"))?;

        let channels: u16 = 2;
        let bits_per_sample: u16 = 32;
        let block_align = channels * (bits_per_sample / 8);
        let avg_bytes_per_sec = sample_rate * (block_align as u32);

        let wf_ext = WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
                nChannels: channels,
                nSamplesPerSec: sample_rate,
                nAvgBytesPerSec: avg_bytes_per_sec,
                nBlockAlign: block_align,
                wBitsPerSample: bits_per_sample,
                cbSize: 22,
            },
            Samples: WAVEFORMATEXTENSIBLE_0 {
                wValidBitsPerSample: 32,
            },
            dwChannelMask: SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT,
            SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        };

        // 20 ms buffer duration (in 100-nanosecond units)
        let buffer_duration: i64 = 200_000;
        let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;

        audio_client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags,
                buffer_duration,
                0,
                &wf_ext.Format,
                None,
            )
            .map_err(|e| format!("IAudioClient::Initialize({sample_rate}Hz): {e}"))?;

        let event = CreateEventW(None, false, false, None)
            .map_err(|e| format!("CreateEventW: {e}"))?;

        audio_client
            .SetEventHandle(event)
            .map_err(|e| {
                let _ = CloseHandle(event);
                format!("SetEventHandle: {e}")
            })?;

        let buffer_frames = audio_client.GetBufferSize().map_err(|e| {
            let _ = CloseHandle(event);
            format!("GetBufferSize: {e}")
        })?;

        let render_client: IAudioRenderClient = audio_client.GetService().map_err(|e| {
            let _ = CloseHandle(event);
            format!("GetService<IAudioRenderClient>: {e}")
        })?;

        // Target ~20 ms latency inside the IPC ring buffer
        let target_latency_frames = (sample_rate as u64 * 20) / 1000;
        ring.resync_consumer(target_latency_frames);

        // Pre-fill silence before starting
        if let Ok(buf_ptr) = render_client.GetBuffer(buffer_frames) {
            if !buf_ptr.is_null() {
                std::ptr::write_bytes(buf_ptr, 0, (buffer_frames as usize) * (block_align as usize));
            }
            let _ = render_client.ReleaseBuffer(buffer_frames, 0);
        }

        audio_client.Start().map_err(|e| {
            let _ = CloseHandle(event);
            format!("IAudioClient::Start: {e}")
        })?;

        if let Ok(mut s) = status.lock() {
            s.active_device_name = dev_name;
            s.sample_rate = sample_rate;
            s.last_error = None;
        }

        let mut last_hb = ring.heartbeat();
        let mut last_hb_time = Instant::now();
        let mut plugin_connected = false;
        let mut temp_buf: Vec<f32> = Vec::new();
        let mut status_tick = Instant::now();

        while running.load(Ordering::Acquire) && !device_changed.load(Ordering::Acquire) {
            ring.bump_app_heartbeat();

            // Restart WASAPI only if the clamped sample rate actually changed
            let ring_sr = ring.sample_rate().clamp(MIN_SAMPLE_RATE, MAX_SAMPLE_RATE);
            if ring_sr != sample_rate {
                break;
            }

            let wait = WaitForSingleObject(event, 100);
            if wait != WAIT_OBJECT_0 {
                // Timeout: still update connection state
                let hb = ring.heartbeat();
                if hb != last_hb {
                    last_hb = hb;
                    last_hb_time = Instant::now();
                    plugin_connected = true;
                } else if last_hb_time.elapsed() > Duration::from_millis(500) {
                    plugin_connected = false;
                }
                continue;
            }

            let padding = match audio_client.GetCurrentPadding() {
                Ok(p) => p,
                Err(_) => break,
            };
            let frames_available = buffer_frames.saturating_sub(padding);
            if frames_available == 0 {
                continue;
            }

            let hb = ring.heartbeat();
            if hb != last_hb {
                last_hb = hb;
                last_hb_time = Instant::now();
                plugin_connected = true;
            } else if last_hb_time.elapsed() > Duration::from_millis(500) {
                plugin_connected = false;
            }

            let sample_count = (frames_available as usize) * 2;
            if temp_buf.len() < sample_count {
                temp_buf.resize(sample_count, 0.0);
            }
            let slice = &mut temp_buf[..sample_count];

            let muted = app_muted.load(Ordering::Relaxed);
            let vol = f32::from_bits(volume_bits.load(Ordering::Relaxed));

            if plugin_connected && !muted {
                ring.read_interleaved(slice, target_latency_frames);
                if (vol - 1.0).abs() > 1e-4 {
                    for s in slice.iter_mut() {
                        *s = (*s * vol).clamp(-1.0, 1.0);
                    }
                }
            } else {
                // Keep reading/advancing ring if connected & muted, otherwise output silence
                if plugin_connected {
                    ring.read_interleaved(slice, target_latency_frames);
                }
                slice.fill(0.0);
            }

            match render_client.GetBuffer(frames_available) {
                Ok(ptr) if !ptr.is_null() => {
                    // Byte-wise copy avoids alignment UB on *mut u8 -> *mut f32 casts
                    std::ptr::copy_nonoverlapping(
                        slice.as_ptr() as *const u8,
                        ptr,
                        sample_count * std::mem::size_of::<f32>(),
                    );
                    if render_client.ReleaseBuffer(frames_available, 0).is_err() {
                        break;
                    }
                }
                _ => break,
            }

            // Update UI status every ~33ms
            if status_tick.elapsed() >= Duration::from_millis(33) {
                status_tick = Instant::now();
                let (pl, pr) = if plugin_connected && !muted {
                    let (l, r) = ring.peaks();
                    ((l * vol).min(1.0), (r * vol).min(1.0))
                } else {
                    (0.0, 0.0)
                };
                if let Ok(mut s) = status.lock() {
                    s.plugin_connected = plugin_connected;
                    s.plugin_muted = ring.plugin_muted();
                    s.sample_rate = sample_rate;
                    s.peak_l = pl;
                    s.peak_r = pr;
                }
            }
        }

        let _ = audio_client.Stop();
        let _ = CloseHandle(event);
    }

    Ok(())
}
