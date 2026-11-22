//! Lock-free Single-Producer Single-Consumer (SPSC) audio ring buffer
//! backed by Windows Named Shared Memory (`Local\DAWStream_AudioRing_v4`).
//!
//! Real-time safe on the producer (VST3/CLAP audio thread) side:
//! zero allocations, zero locks, pure atomic operations, NaN/Inf sanitization,
//! and crash-resilient multi-instance ownership leasing.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, ERROR_ALREADY_EXISTS, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
    SE_KERNEL_OBJECT,
};
use windows_sys::Win32::Security::{
    GetAce, GetSidSubAuthority, GetSidSubAuthorityCount, ACL, LABEL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, SYSTEM_MANDATORY_LABEL_ACE,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, VirtualQuery, FILE_MAP_ALL_ACCESS,
    MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};
use windows_sys::Win32::System::SystemInformation::GetTickCount64;

pub const SHARED_MEM_NAME: &str = "Local\\DAWStream_AudioRing_v4";
pub const MAGIC: u32 = 0x4441_5753; // "DAWS"
const MAGIC_INITING: u32 = 0x494E_4954; // "INIT"
pub const VERSION: u32 = 4;

// Mandatory-label constants (winnt.h)
const SYSTEM_MANDATORY_LABEL_ACE_TYPE: u8 = 0x11;
const SYSTEM_MANDATORY_LABEL_NO_WRITE_UP: u32 = 0x1;
const SYSTEM_MANDATORY_LABEL_NO_READ_UP: u32 = 0x2;
const SECURITY_MANDATORY_MEDIUM_RID: u32 = 0x2000;

pub const MIN_SAMPLE_RATE: u32 = 22_050;
pub const MAX_SAMPLE_RATE: u32 = 192_000;

/// If the active producer hasn't processed audio for this long (e.g. DAW crashed
/// without running Drop), another instance may automatically reclaim ownership.
pub const OWNER_LEASE_TIMEOUT_MS: u64 = 1_500;

/// Only write audio samples into shared memory when the companion app has
/// polled within this window (prevents 24/7 audio broadcast when not streaming).
pub const CONSUMER_ACTIVE_TIMEOUT_MS: u64 = 1_500;

/// Power-of-two number of stereo frames in the ring buffer.
/// 65,536 frames @ 48 kHz ~= 1.36 seconds of headroom.
pub const RING_FRAMES: usize = 65_536;
pub const RING_MASK: usize = RING_FRAMES - 1;
pub const CHANNELS: usize = 2;

/// Memory layout shared between the plugin (producer) and companion app (consumer).
/// All fields are atomic to avoid any data races / UB across processes.
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

    /// Incremented by the active plugin on every `process()` call.
    pub heartbeat: AtomicU64,

    /// Peak levels encoded as `f32::to_bits()` for UI metering.
    pub peak_l_bits: AtomicU32,
    pub peak_r_bits: AtomicU32,

    /// 0 = active, 1 = muted by plugin.
    pub plugin_muted: AtomicU32,
    pub _pad0: u32,

    /// Incremented by the companion app while actively reading audio.
    pub app_heartbeat: AtomicU64,
    /// Monotonic tick (`GetTickCount64`) when the companion app was last active.
    pub app_last_tick_ms: AtomicU64,

    /// Multi-instance guard, packed so owner + lease timestamp change in ONE atomic op:
    /// high 32 bits = owner instance id (0 = none), low 32 bits = `GetTickCount64() as u32`
    /// of the owner's last `process()` call.
    pub owner_lease: AtomicU64,
    pub next_instance_id: AtomicU64,

    /// Interleaved stereo f32 samples stored as atomic bit patterns: `[L0, R0, L1, R1, ...]`.
    pub samples: [AtomicU32; RING_FRAMES * CHANNELS],
}

/// Safe RAII wrapper around the Windows shared memory mapping.
pub struct SharedAudioRing {
    handle: HANDLE,
    view: *mut SharedAudioBlock,
    /// Consumer-local (not shared): `true` while waiting for the ring to refill
    /// to the target latency cushion after startup or an underrun.
    consumer_buffering: AtomicBool,
}

unsafe impl Send for SharedAudioRing {}
unsafe impl Sync for SharedAudioRing {}

#[inline]
fn now_tick_ms() -> u64 {
    unsafe { GetTickCount64() }
}

#[inline]
fn pack_lease(id: u32, tick: u32) -> u64 {
    ((id as u64) << 32) | tick as u64
}

#[inline]
fn lease_owner(v: u64) -> u32 {
    (v >> 32) as u32
}

#[inline]
fn lease_tick(v: u64) -> u32 {
    v as u32
}

/// Verifies that an already-existing section carries an explicit mandatory label of at
/// least Medium integrity with No-Read-Up + No-Write-Up. A Low-IL process cannot create
/// an object labelled above its own integrity level, so this rejects squatted sections.
unsafe fn existing_section_is_trusted(handle: HANDLE) -> bool {
    let mut sacl: *mut ACL = ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
    let err = GetSecurityInfo(
        handle,
        SE_KERNEL_OBJECT,
        LABEL_SECURITY_INFORMATION,
        ptr::null_mut(),
        ptr::null_mut(),
        ptr::null_mut(),
        &mut sacl,
        &mut sd,
    );
    if err != 0 {
        return false;
    }

    let mut trusted = false;
    if !sacl.is_null() {
        for i in 0..(*sacl).AceCount as u32 {
            let mut ace: *mut core::ffi::c_void = ptr::null_mut();
            if GetAce(sacl, i, &mut ace) == 0 || ace.is_null() {
                continue;
            }
            let label = &*(ace as *const SYSTEM_MANDATORY_LABEL_ACE);
            if label.Header.AceType != SYSTEM_MANDATORY_LABEL_ACE_TYPE {
                continue;
            }
            let sid = &label.SidStart as *const u32 as PSID;
            let count = *GetSidSubAuthorityCount(sid);
            if count == 0 {
                break;
            }
            let rid = *GetSidSubAuthority(sid, count as u32 - 1);
            let policy = SYSTEM_MANDATORY_LABEL_NO_READ_UP | SYSTEM_MANDATORY_LABEL_NO_WRITE_UP;
            trusted = rid >= SECURITY_MANDATORY_MEDIUM_RID && (label.Mask & policy) == policy;
            break;
        }
    }

    if !sd.is_null() {
        LocalFree(sd);
    }
    trusted
}

#[inline]
fn sanitize_sample(x: f32) -> f32 {
    if x.is_finite() {
        x.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

impl SharedAudioRing {
    /// Opens or creates the default named shared memory block.
    pub fn open_or_create() -> Result<Self, String> {
        Self::open_or_create_named(SHARED_MEM_NAME)
    }

    /// Opens or creates a shared memory block with a custom name (useful for isolated tests).
    pub fn open_or_create_named(name: &str) -> Result<Self, String> {
        let wide_name: Vec<u16> = OsStr::new(name)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let size = std::mem::size_of::<SharedAudioBlock>();
        let size_high = ((size as u64) >> 32) as u32;
        let size_low = (size as u64 & 0xFFFF_FFFF) as u32;

        unsafe {
            // Restrict section access to current user (Owner) + Administrators,
            // and enforce Medium Integrity Level with No-Read-Up + No-Write-Up (NRNW)
            // to block AppContainer / Low-IL processes from both reading and writing.
            let sddl: Vec<u16> = OsStr::new("D:P(A;;GA;;;OW)(A;;GA;;;BA)S:(ML;;NRNW;;;ME)")
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
            let sd_ok = ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                ptr::null_mut(),
            ) != 0;
            // Fail closed: never create the section with a default (label-less) descriptor.
            if !sd_ok || sd.is_null() {
                return Err(format!(
                    "Failed to build shared memory security descriptor: {}",
                    std::io::Error::last_os_error()
                ));
            }

            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd,
                bInheritHandle: 0,
            };

            let handle = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                &sa,
                PAGE_READWRITE,
                size_high,
                size_low,
                wide_name.as_ptr(),
            );
            // Must be read before any other API call clobbers it.
            let already_existed = GetLastError() == ERROR_ALREADY_EXISTS;

            LocalFree(sd);

            if handle.is_null() {
                return Err(format!(
                    "CreateFileMappingW failed with OS error: {}",
                    std::io::Error::last_os_error()
                ));
            }

            // When the section already exists our SECURITY_ATTRIBUTES were ignored,
            // so verify the existing object was not pre-created by a lower-integrity process.
            if already_existed && !existing_section_is_trusted(handle) {
                CloseHandle(handle);
                return Err(
                    "Shared memory section exists with an untrusted security label".into(),
                );
            }

            let view = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, size);
            if view.Value.is_null() {
                let err = std::io::Error::last_os_error();
                CloseHandle(handle);
                return Err(format!("MapViewOfFile failed with OS error: {err}"));
            }

            // Verify mapped region is actually large enough (defends against undersized section squatting)
            let mut mbi: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
            let queried = VirtualQuery(
                view.Value,
                &mut mbi,
                std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            );
            if queried == 0 || mbi.RegionSize < size {
                UnmapViewOfFile(view);
                CloseHandle(handle);
                return Err("Mapped shared memory region is smaller than expected".into());
            }

            let block = view.Value as *mut SharedAudioBlock;
            let block_ref = &*block;

            // Two-phase initialization: CAS 0 -> MAGIC_INITING, populate all fields, then Release MAGIC
            if block_ref
                .magic
                .compare_exchange(0, MAGIC_INITING, Ordering::AcqRel, Ordering::Acquire)
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
                block_ref.app_last_tick_ms.store(0, Ordering::Release);
                block_ref.owner_lease.store(0, Ordering::Release);
                block_ref.next_instance_id.store(1, Ordering::Release);
                block_ref.magic.store(MAGIC, Ordering::Release);
            } else {
                // Spin briefly if another process is mid-initialization
                let mut spins = 0u32;
                while block_ref.magic.load(Ordering::Acquire) == MAGIC_INITING && spins < 500 {
                    std::thread::yield_now();
                    spins += 1;
                }
                let magic = block_ref.magic.load(Ordering::Acquire);
                let ver = block_ref.version.load(Ordering::Acquire);
                if magic != MAGIC || ver != VERSION {
                    UnmapViewOfFile(view);
                    CloseHandle(handle);
                    return Err(format!(
                        "Incompatible shared memory header (magic={magic:#x}, version={ver})"
                    ));
                }
            }

            Ok(Self {
                handle,
                view: block,
                consumer_buffering: AtomicBool::new(true),
            })
        }
    }

    #[inline]
    fn block(&self) -> &SharedAudioBlock {
        unsafe { &*self.view }
    }

    /// Producer: updates sample rate metadata (clamped to supported WASAPI bounds).
    #[inline]
    pub fn set_sample_rate(&self, sample_rate: u32) {
        let clamped = sample_rate.clamp(MIN_SAMPLE_RATE, MAX_SAMPLE_RATE);
        self.block().sample_rate.store(clamped, Ordering::Release);
    }

    /// Consumer: reads current sample rate from plugin (always within supported bounds).
    #[inline]
    pub fn sample_rate(&self) -> u32 {
        let sr = self.block().sample_rate.load(Ordering::Acquire);
        if (MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&sr) {
            sr
        } else {
            48_000
        }
    }

    /// Producer: writes stereo planar slices into the interleaved ring buffer.
    /// Real-time safe (no locks, no allocations, NaN/Inf sanitized).
    pub fn write_planar(&self, left: &[f32], right: &[f32], gain: f32, muted: bool) {
        let frames = left.len().min(right.len());
        if frames == 0 {
            return;
        }

        let b = self.block();
        b.plugin_muted
            .store(if muted { 1 } else { 0 }, Ordering::Relaxed);

        let safe_gain = if gain.is_finite() {
            gain.clamp(0.0, 4.0)
        } else {
            1.0
        };

        let now = now_tick_ms();
        let app_tick = b.app_last_tick_ms.load(Ordering::Acquire);
        let consumer_active =
            app_tick != 0 && app_tick <= now && now.saturating_sub(app_tick) <= CONSUMER_ACTIVE_TIMEOUT_MS;

        let start_wpos = b.write_pos.load(Ordering::Acquire);
        let mut wpos = start_wpos;
        let mut peak_l = 0.0f32;
        let mut peak_r = 0.0f32;

        if muted {
            if consumer_active {
                for _ in 0..frames {
                    let idx = (wpos as usize & RING_MASK) * 2;
                    b.samples[idx].store(0, Ordering::Relaxed);
                    b.samples[idx + 1].store(0, Ordering::Relaxed);
                    wpos = wpos.wrapping_add(1);
                }
                Self::commit_write_pos(b, start_wpos, wpos);
            }
        } else {
            for i in 0..frames {
                let l = sanitize_sample(left[i] * safe_gain);
                let r = sanitize_sample(right[i] * safe_gain);
                peak_l = peak_l.max(l.abs());
                peak_r = peak_r.max(r.abs());

                // Privacy gate: only write audio samples into shared memory when DAWStream.exe is active
                if consumer_active {
                    let idx = (wpos as usize & RING_MASK) * 2;
                    b.samples[idx].store(l.to_bits(), Ordering::Relaxed);
                    b.samples[idx + 1].store(r.to_bits(), Ordering::Relaxed);
                    wpos = wpos.wrapping_add(1);
                }
            }
            if consumer_active {
                Self::commit_write_pos(b, start_wpos, wpos);
            }
        }

        b.peak_l_bits.store(peak_l.to_bits(), Ordering::Relaxed);
        b.peak_r_bits.store(peak_r.to_bits(), Ordering::Relaxed);
        b.heartbeat.fetch_add(1, Ordering::Release);
    }

    /// Publishes a new write cursor only if no other producer advanced it meanwhile
    /// (e.g. during a "Take Over" hand-off), keeping `write_pos` consistent.
    #[inline]
    fn commit_write_pos(b: &SharedAudioBlock, start: u64, end: u64) {
        let _ = b
            .write_pos
            .compare_exchange(start, end, Ordering::AcqRel, Ordering::Relaxed);
    }

    /// Consumer: reads interleaved stereo frames into `out` (`out.len() == frames * 2`).
    /// Keeps a cushion of about `target_latency_frames` in the ring: after startup or an
    /// underrun it outputs silence until the cushion has been rebuilt, instead of
    /// running with zero headroom and glitching on every scheduling jitter.
    /// Returns the number of frames read from the ring (remainder is zero-filled).
    pub fn read_interleaved(&self, out: &mut [f32], target_latency_frames: u64) -> usize {
        let requested_frames = out.len() / 2;
        if requested_frames == 0 {
            return 0;
        }
        let requested = requested_frames as u64;

        let b = self.block();
        let wpos = b.write_pos.load(Ordering::Acquire);
        let mut rpos = b.read_pos.load(Ordering::Relaxed);

        let max_drift = (target_latency_frames * 3).max(requested * 4);
        let mut available = wpos.wrapping_sub(rpos);

        // Overrun (or cursor corruption): jump back to target latency.
        if available > (RING_FRAMES as u64) || available > max_drift {
            rpos = wpos.saturating_sub(target_latency_frames);
            available = wpos.wrapping_sub(rpos);
        }

        let mut buffering = self.consumer_buffering.load(Ordering::Relaxed);
        if !buffering && available < requested {
            // Underrun: stop draining and rebuild the cushion.
            buffering = true;
        }
        if buffering {
            if available >= target_latency_frames + requested {
                buffering = false;
            } else {
                self.consumer_buffering.store(true, Ordering::Relaxed);
                out[..requested_frames * 2].fill(0.0);
                b.read_pos.store(rpos, Ordering::Release);
                return 0;
            }
        }
        self.consumer_buffering.store(buffering, Ordering::Relaxed);

        let to_read = (available as usize).min(requested_frames);

        for i in 0..to_read {
            let idx = (rpos as usize & RING_MASK) * 2;
            let l = f32::from_bits(b.samples[idx].load(Ordering::Relaxed));
            let r = f32::from_bits(b.samples[idx + 1].load(Ordering::Relaxed));
            out[i * 2] = sanitize_sample(l);
            out[i * 2 + 1] = sanitize_sample(r);
            rpos = rpos.wrapping_add(1);
        }

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
        self.consumer_buffering.store(true, Ordering::Relaxed);
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
        (
            if l.is_finite() { l.clamp(0.0, 1.0) } else { 0.0 },
            if r.is_finite() { r.clamp(0.0, 1.0) } else { 0.0 },
        )
    }

    #[inline]
    pub fn plugin_muted(&self) -> bool {
        self.block().plugin_muted.load(Ordering::Relaxed) != 0
    }

    /// Consumer: signals that the companion app is alive and reading.
    #[inline]
    pub fn bump_app_heartbeat(&self) {
        let b = self.block();
        b.app_last_tick_ms.store(now_tick_ms(), Ordering::Release);
        b.app_heartbeat.fetch_add(1, Ordering::Release);
    }

    /// Producer: reads the companion app heartbeat counter.
    #[inline]
    pub fn app_heartbeat(&self) -> u64 {
        self.block().app_heartbeat.load(Ordering::Acquire)
    }

    /// Allocates a unique non-zero producer instance ID (fits in 32 bits).
    #[inline]
    pub fn alloc_instance_id(&self) -> u64 {
        loop {
            let id = self.block().next_instance_id.fetch_add(1, Ordering::AcqRel) as u32;
            if id != 0 {
                return id as u64;
            }
        }
    }

    #[inline]
    fn lease_id(instance_id: u64) -> Option<u32> {
        match u32::try_from(instance_id) {
            Ok(0) | Err(_) => None,
            Ok(id) => Some(id),
        }
    }

    /// Returns `true` if `instance_id` is (or successfully claimed) the active producer,
    /// and refreshes its lease. Automatically reclaims ownership if the previous owner has
    /// been silent for longer than `OWNER_LEASE_TIMEOUT_MS` (e.g. after a DAW crash).
    /// Owner id and lease timestamp live in one `AtomicU64`, so two instances can never
    /// both win a claim. Real-time safe.
    #[inline]
    pub fn is_or_claim_owner(&self, instance_id: u64) -> bool {
        let Some(id) = Self::lease_id(instance_id) else {
            return false;
        };
        let b = self.block();
        let now = now_tick_ms() as u32;
        let cur = b.owner_lease.load(Ordering::Acquire);
        let owner = lease_owner(cur);

        if owner == id {
            // Refresh lease; if a concurrent write changed it, re-check who owns it now.
            return match b.owner_lease.compare_exchange(
                cur,
                pack_lease(id, now),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => true,
                Err(actual) => lease_owner(actual) == id,
            };
        }

        let age = now.wrapping_sub(lease_tick(cur));
        let stale = owner == 0 || age > OWNER_LEASE_TIMEOUT_MS as u32;
        stale
            && b.owner_lease
                .compare_exchange(cur, pack_lease(id, now), Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
    }

    /// Forces `instance_id` to become the active producer ("Take Over").
    #[inline]
    pub fn force_claim_owner(&self, instance_id: u64) {
        if let Some(id) = Self::lease_id(instance_id) {
            self.block()
                .owner_lease
                .store(pack_lease(id, now_tick_ms() as u32), Ordering::Release);
        }
    }

    /// Releases ownership if `instance_id` currently owns the stream.
    #[inline]
    pub fn release_owner(&self, instance_id: u64) {
        let Some(id) = Self::lease_id(instance_id) else {
            return;
        };
        let b = self.block();
        let mut cur = b.owner_lease.load(Ordering::Acquire);
        while lease_owner(cur) == id {
            match b
                .owner_lease
                .compare_exchange(cur, 0, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return,
                Err(actual) => cur = actual,
            }
        }
    }

    #[inline]
    pub fn active_owner_id(&self) -> u64 {
        lease_owner(self.block().owner_lease.load(Ordering::Acquire)) as u64
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

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_name(tag: &str) -> String {
        format!(
            "Local\\DAWStream_Test_{}_{}_{}",
            tag,
            std::process::id(),
            now_tick_ms()
        )
    }

    #[test]
    fn write_and_read_roundtrip() {
        let name = unique_name("roundtrip");
        let producer = SharedAudioRing::open_or_create_named(&name).unwrap();
        let consumer = SharedAudioRing::open_or_create_named(&name).unwrap();
        consumer.bump_app_heartbeat();

        let left = [0.25f32, -0.5, 0.75, 1.0];
        let right = [-0.25f32, 0.5, -0.75, -1.0];
        producer.write_planar(&left, &right, 1.0, false);

        let mut out = [0.0f32; 8];
        let read = consumer.read_interleaved(&mut out, 0);
        assert_eq!(read, 4);
        assert_eq!(out, [0.25, -0.25, -0.5, 0.5, 0.75, -0.75, 1.0, -1.0]);
    }

    #[test]
    fn privacy_gate_blocks_writes_when_consumer_offline() {
        let name = unique_name("privacy");
        let producer = SharedAudioRing::open_or_create_named(&name).unwrap();
        let consumer = SharedAudioRing::open_or_create_named(&name).unwrap();

        // No app heartbeat yet -> audio samples must NOT be written into shared memory
        producer.write_planar(&[0.5; 4], &[0.5; 4], 1.0, false);
        let mut out = [0.0f32; 8];
        let read = consumer.read_interleaved(&mut out, 64);
        assert_eq!(read, 0);
        assert_eq!(out, [0.0; 8]);
    }

    #[test]
    fn nan_and_inf_are_sanitized() {
        let name = unique_name("sanitize");
        let ring = SharedAudioRing::open_or_create_named(&name).unwrap();
        ring.bump_app_heartbeat();

        let left = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 5.0];
        let right = [-5.0, f32::NAN, 0.5, -0.5];
        ring.write_planar(&left, &right, 1.0, false);

        let mut out = [0.0f32; 8];
        let read = ring.read_interleaved(&mut out, 0);
        assert_eq!(read, 4);
        for s in out {
            assert!(s.is_finite());
            assert!((-1.0..=1.0).contains(&s));
        }
        assert_eq!(out[0], 0.0);
        assert_eq!(out[1], -1.0);
        assert_eq!(out[6], 1.0);
        assert_eq!(out[7], -0.5);
    }

    #[test]
    fn multi_instance_guard_and_takeover() {
        let name = unique_name("multi");
        let ring = SharedAudioRing::open_or_create_named(&name).unwrap();

        let id1 = ring.alloc_instance_id();
        let id2 = ring.alloc_instance_id();
        assert_ne!(id1, id2);

        // First instance claims ownership
        assert!(ring.is_or_claim_owner(id1));
        ring.write_planar(&[0.1; 4], &[0.1; 4], 1.0, false);

        // Second instance stays in standby
        assert!(!ring.is_or_claim_owner(id2));

        // Explicit Take Over switches owner to id2
        ring.force_claim_owner(id2);
        assert!(ring.is_or_claim_owner(id2));
        assert!(!ring.is_or_claim_owner(id1));

        // Releasing id2 allows id1 to reclaim
        ring.release_owner(id2);
        assert!(ring.is_or_claim_owner(id1));
    }

    #[test]
    fn ring_buffer_wraparound() {
        let name = unique_name("wrap");
        let ring = SharedAudioRing::open_or_create_named(&name).unwrap();
        ring.bump_app_heartbeat();

        let chunk_l = [0.5f32; 1024];
        let chunk_r = [-0.5f32; 1024];
        let mut out = [0.0f32; 2048];

        // Exceed RING_FRAMES (65536) to exercise wrap-around
        for _ in 0..70 {
            ring.write_planar(&chunk_l, &chunk_r, 1.0, false);
            let n = ring.read_interleaved(&mut out, 0);
            assert_eq!(n, 1024);
            assert_eq!(out[0], 0.5);
            assert_eq!(out[1], -0.5);
        }
    }

    #[test]
    fn underrun_rebuilds_latency_cushion() {
        let name = unique_name("rebuffer");
        let ring = SharedAudioRing::open_or_create_named(&name).unwrap();
        ring.bump_app_heartbeat();
        ring.resync_consumer(8);
        let mut out = [1.0f32; 8]; // 4 frames

        // Not enough for target (8) + request (4): keep buffering, output silence
        ring.write_planar(&[0.5; 4], &[0.5; 4], 1.0, false);
        assert_eq!(ring.read_interleaved(&mut out, 8), 0);
        assert_eq!(out, [0.0; 8]);

        // 12 frames available -> playback starts and leaves an 8-frame cushion
        ring.write_planar(&[0.5; 8], &[0.5; 8], 1.0, false);
        assert_eq!(ring.read_interleaved(&mut out, 8), 4);
        assert_eq!(ring.read_interleaved(&mut out, 8), 4);
        assert_eq!(ring.read_interleaved(&mut out, 8), 4);

        // Ring empty -> underrun: go back to buffering, do not drain partial blocks
        assert_eq!(ring.read_interleaved(&mut out, 8), 0);
        ring.write_planar(&[0.5; 4], &[0.5; 4], 1.0, false);
        assert_eq!(ring.read_interleaved(&mut out, 8), 0);
    }

    #[test]
    fn concurrent_claims_have_single_winner() {
        use std::sync::{Arc, Barrier};
        for round in 0..50 {
            let name = unique_name(&format!("race{round}"));
            let ring = Arc::new(SharedAudioRing::open_or_create_named(&name).unwrap());
            let n = 8;
            let barrier = Arc::new(Barrier::new(n));
            let handles: Vec<_> = (0..n)
                .map(|_| {
                    let ring = ring.clone();
                    let barrier = barrier.clone();
                    let id = ring.alloc_instance_id();
                    std::thread::spawn(move || {
                        barrier.wait();
                        ring.is_or_claim_owner(id)
                    })
                })
                .collect();
            let winners = handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .filter(|w| *w)
                .count();
            assert_eq!(winners, 1);
        }
    }

    #[test]
    fn rejects_section_without_integrity_label() {
        let name = unique_name("squat");
        let wide: Vec<u16> = OsStr::new(&name).encode_wide().chain(std::iter::once(0)).collect();
        let size = std::mem::size_of::<SharedAudioBlock>() as u32;
        // Pre-create the section with a default (label-less) security descriptor
        let squat = unsafe {
            CreateFileMappingW(INVALID_HANDLE_VALUE, ptr::null(), PAGE_READWRITE, 0, size, wide.as_ptr())
        };
        assert!(!squat.is_null());
        assert!(SharedAudioRing::open_or_create_named(&name).is_err());
        unsafe { CloseHandle(squat) };
    }
}
