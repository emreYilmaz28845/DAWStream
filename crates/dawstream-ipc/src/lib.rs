//! Lock-free Single-Producer Single-Consumer (SPSC) audio ring buffer
//! backed by Windows Named Shared Memory (`Local\DAWStream_AudioRing_v1`).
//!
//! Real-time safe on the producer (VST3 audio thread) side:
//! zero allocations, zero locks, pure atomic operations.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};

pub const SHARED_MEM_NAME: &str = "Local\\DAWStream_AudioRing_v1";
pub const MAGIC: u32 = 0x4441_5753; // "DAWS"
pub const VERSION: u32 = 1;

/// Power-of-two number of stereo frames in the ring buffer.
/// 65,536 frames @ 48 kHz ~= 1.36 seconds of headroom.
pub const RING_FRAMES: usize = 65_536;
pub const RING_MASK: usize = RING_FRAMES - 1;
pub const CHANNELS: usize = 2;

/// Memory layout shared between the VST3 plugin (producer) and companion app (consumer).
#[repr(C, align(64))]
pub struct SharedAudioBlock {
    pub magic: AtomicU32,
    pub version: AtomicU32,
    pub sample_rate: AtomicU32,
    pub channels: AtomicU32,

    /// Monotonically increasing count of stereo frames written by producer.
    pub write_pos: AtomicU64,
    /// Monotonically increasing count of stereo frames read by consumer.
    pub read_pos: AtomicU64,

    /// Incremented by the plugin on every `process()` call to detect liveness.
    pub heartbeat: AtomicU64,

    /// Peak levels encoded as `f32::to_bits()` for UI metering.
    pub peak_l_bits: AtomicU32,
    pub peak_r_bits: AtomicU32,

    /// 0 = active, 1 = muted by plugin.
    pub plugin_muted: AtomicU32,
    pub _pad0: u32,

    /// Incremented by the companion app while actively reading audio.
    pub app_heartbeat: AtomicU64,
    pub _reserved: [u32; 4],

    /// Interleaved stereo f32 samples: `[L0, R0, L1, R1, ...]`.
    pub samples: [f32; RING_FRAMES * CHANNELS],
}

/// Safe RAII wrapper around the Windows shared memory mapping.
pub struct SharedAudioRing {
    handle: HANDLE,
    view: *mut SharedAudioBlock,
}

unsafe impl Send for SharedAudioRing {}
unsafe impl Sync for SharedAudioRing {}

impl SharedAudioRing {
    /// Opens or creates the shared memory block.
    /// Both producer and consumer can call this in any order.
    pub fn open_or_create() -> Result<Self, String> {
        let wide_name: Vec<u16> = OsStr::new(SHARED_MEM_NAME)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let size = std::mem::size_of::<SharedAudioBlock>();
        let size_high = ((size as u64) >> 32) as u32;
        let size_low = (size as u64 & 0xFFFF_FFFF) as u32;

        unsafe {
            let handle = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                ptr::null(),
                PAGE_READWRITE,
                size_high,
                size_low,
                wide_name.as_ptr(),
            );

            if handle.is_null() {
                return Err(format!(
                    "CreateFileMappingW failed with OS error: {}",
                    std::io::Error::last_os_error()
                ));
            }

            let view = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, size);
            if view.Value.is_null() {
                let err = std::io::Error::last_os_error();
                CloseHandle(handle);
                return Err(format!("MapViewOfFile failed with OS error: {err}"));
            }

            let block = view.Value as *mut SharedAudioBlock;
            let block_ref = &*block;

            // Initialize header once if freshly created (magic == 0)
            if block_ref
                .magic
                .compare_exchange(0, MAGIC, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                block_ref.version.store(VERSION, Ordering::Release);
                block_ref.sample_rate.store(48_000, Ordering::Release);
                block_ref.channels.store(CHANNELS as u32, Ordering::Release);
                block_ref.write_pos.store(0, Ordering::Release);
                block_ref.read_pos.store(0, Ordering::Release);
                block_ref.heartbeat.store(0, Ordering::Release);
                block_ref.peak_l_bits.store(0, Ordering::Release);
                block_ref.peak_r_bits.store(0, Ordering::Release);
                block_ref.plugin_muted.store(0, Ordering::Release);
                block_ref.app_heartbeat.store(0, Ordering::Release);
            }

            Ok(Self { handle, view: block })
        }
    }

    #[inline]
    fn block(&self) -> &SharedAudioBlock {
        unsafe { &*self.view }
    }

    #[inline]
    fn block_mut_samples_ptr(&self) -> *mut f32 {
        unsafe { ptr::addr_of_mut!((*self.view).samples) as *mut f32 }
    }

    /// Producer: updates sample rate metadata.
    #[inline]
    pub fn set_sample_rate(&self, sample_rate: u32) {
        self.block().sample_rate.store(sample_rate, Ordering::Release);
    }

    /// Consumer: reads current sample rate from plugin.
    #[inline]
    pub fn sample_rate(&self) -> u32 {
        let sr = self.block().sample_rate.load(Ordering::Acquire);
        if sr == 0 { 48_000 } else { sr }
    }

    /// Producer: writes stereo planar slices into the interleaved ring buffer.
    /// Real-time safe (no locks, no allocations).
    pub fn write_planar(&self, left: &[f32], right: &[f32], gain: f32, muted: bool) {
        let frames = left.len().min(right.len());
        if frames == 0 {
            return;
        }

        let b = self.block();
        b.plugin_muted
            .store(if muted { 1 } else { 0 }, Ordering::Relaxed);

        let mut wpos = b.write_pos.load(Ordering::Relaxed);
        let samples_ptr = self.block_mut_samples_ptr();

        let mut peak_l = 0.0f32;
        let mut peak_r = 0.0f32;

        if muted {
            for _ in 0..frames {
                let idx = (wpos as usize & RING_MASK) * 2;
                unsafe {
                    samples_ptr.add(idx).write(0.0);
                    samples_ptr.add(idx + 1).write(0.0);
                }
                wpos = wpos.wrapping_add(1);
            }
        } else {
            for i in 0..frames {
                let l = (left[i] * gain).clamp(-2.0, 2.0);
                let r = (right[i] * gain).clamp(-2.0, 2.0);
                peak_l = peak_l.max(l.abs());
                peak_r = peak_r.max(r.abs());

                let idx = (wpos as usize & RING_MASK) * 2;
                unsafe {
                    samples_ptr.add(idx).write(l);
                    samples_ptr.add(idx + 1).write(r);
                }
                wpos = wpos.wrapping_add(1);
            }
        }

        b.write_pos.store(wpos, Ordering::Release);
        b.peak_l_bits.store(peak_l.to_bits(), Ordering::Relaxed);
        b.peak_r_bits.store(peak_r.to_bits(), Ordering::Relaxed);
        b.heartbeat.fetch_add(1, Ordering::Release);
    }

    /// Consumer: reads interleaved stereo frames into `out` (`out.len() == frames * 2`).
    /// Automatically keeps latency bounded around `target_latency_frames`.
    /// Returns the number of frames read from the ring (remainder is zero-filled).
    pub fn read_interleaved(&self, out: &mut [f32], target_latency_frames: u64) -> usize {
        let requested_frames = out.len() / 2;
        if requested_frames == 0 {
            return 0;
        }

        let b = self.block();
        let wpos = b.write_pos.load(Ordering::Acquire);
        let mut rpos = b.read_pos.load(Ordering::Relaxed);

        // If ring overflowed or latency drifted too high, jump forward to target latency.
        let max_drift = (target_latency_frames * 3).max(requested_frames as u64 * 4);
        let available = wpos.wrapping_sub(rpos);

        if available > (RING_FRAMES as u64) || available > max_drift {
            rpos = wpos.saturating_sub(target_latency_frames);
        }

        let available = wpos.wrapping_sub(rpos);
        let to_read = (available as usize).min(requested_frames);

        let samples_ptr = self.block_mut_samples_ptr() as *const f32;

        for i in 0..to_read {
            let idx = (rpos as usize & RING_MASK) * 2;
            unsafe {
                out[i * 2] = samples_ptr.add(idx).read();
                out[i * 2 + 1] = samples_ptr.add(idx + 1).read();
            }
            rpos = rpos.wrapping_add(1);
        }

        // Zero-fill on underrun
        if to_read < requested_frames {
            out[to_read * 2..requested_frames * 2].fill(0.0);
        }

        b.read_pos.store(rpos, Ordering::Release);
        to_read
    }

    /// Resets consumer read cursor to just behind producer write cursor.
    pub fn resync_consumer(&self, target_latency_frames: u64) {
        let b = self.block();
        let wpos = b.write_pos.load(Ordering::Acquire);
        b.read_pos
            .store(wpos.saturating_sub(target_latency_frames), Ordering::Release);
    }

    #[inline]
    pub fn heartbeat(&self) -> u64 {
        self.block().heartbeat.load(Ordering::Acquire)
    }

    #[inline]
    pub fn peaks(&self) -> (f32, f32) {
        let b = self.block();
        let l = f32::from_bits(b.peak_l_bits.load(Ordering::Relaxed));
        let r = f32::from_bits(b.peak_r_bits.load(Ordering::Relaxed));
        (l, r)
    }

    #[inline]
    pub fn plugin_muted(&self) -> bool {
        self.block().plugin_muted.load(Ordering::Relaxed) != 0
    }

    /// Consumer: signals that the companion app is alive and reading.
    #[inline]
    pub fn bump_app_heartbeat(&self) {
        self.block().app_heartbeat.fetch_add(1, Ordering::Release);
    }

    /// Producer: reads the companion app heartbeat counter.
    #[inline]
    pub fn app_heartbeat(&self) -> u64 {
        self.block().app_heartbeat.load(Ordering::Acquire)
    }
}

impl Drop for SharedAudioRing {
    fn drop(&mut self) {
        unsafe {
            if !self.view.is_null() {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: self.view as *mut _,
                });
            }
            if !self.handle.is_null() {
                CloseHandle(self.handle);
            }
        }
    }
}
