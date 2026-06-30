//! # Shared-memory state transport — the seqlock'd "latest value" slot
//!
//! A durable **latest-wins state** channel (the generic retained-channel idea, see
//! [`crate::LocalBus::retain`]) only ever needs the *newest* value, never a history of them.
//! That makes a FIFO ring the wrong shape — it would grow unboundedly if the reader fell behind,
//! and deliver stale values the reader must then skip. The right shape is a single **slot**:
//! one fixed buffer the writer overwrites in place and the reader reads. (Frame uses this for
//! its hot Scene channel; nothing here knows about scenes.)
//!
//! We protect that slot with a **seqlock** (a sequence-counter lock, the classic
//! single-writer / many-reader pattern — see Mara Bos, *Rust Atomics and Locks*, ch. 4):
//!
//!   * the **writer never blocks** — there is no backpressure, no allocation, no growth; it
//!     stamps an odd sequence, overwrites the bytes, stamps the next even sequence;
//!   * the **reader always observes a complete, latest value** — it snapshots the sequence,
//!     copies the bytes, and re-checks the sequence; a value that changed (or was odd, i.e.
//!     a write in flight) is retried, so a torn read is never returned;
//!   * a **reader restart re-syncs for free** — the slot is writer-owned shared memory, so a
//!     freshly respawned reader re-opens the same mapping/file and immediately reads the last
//!     value. This is the shared-memory equivalent of a retained-channel replay, so on this
//!     path that replay is simply not needed.
//!
//! The payload is **opaque bytes** — the writer encodes whatever it likes (Frame ships a
//! postcard-encoded `SceneSpec`); this module stays free of any domain dependency: the slot
//! moves a `&[u8]`, nothing more.
//!
//! ## Backing store
//!
//! On Unix (Linux and macOS), we use POSIX shared memory (`shm_open` and `shm_unlink`), which
//! creates a memory-backed file descriptor and maps it using `memmap2`. On Linux, this is
//! backed by `/dev/shm` (tmpfs), while on macOS it resides entirely in-memory.
//!
//! On Windows, we use named shared memory mapped files backed by the system paging file
//! (`CreateFileMappingW` with `INVALID_HANDLE_VALUE` and `MapViewOfFile`), which resides
//! entirely in memory and avoids disk writes and file-locking cleanup issues.

use std::sync::atomic::{fence, AtomicU64, Ordering};

/// Env var carrying the slot's identifier (the backing file's path) from writer to reader.
/// The writer sets it before spawning the reader; the child inherits and opens it.
pub const SHMEM_ID_ENV: &str = "MICRO_STATE_SHMEM_ID";

/// Env var that toggles the shared-memory state path. Default **on**; set to `0`/`false`/
/// `off` to force the caller's fallback transport (the safe escape hatch if shmem setup fails
/// or misbehaves on a host). Parsed by [`shmem_enabled`].
pub const SHMEM_TOGGLE_ENV: &str = "MICRO_STATE_SHMEM";

/// Whether the shared-memory state path is enabled (the [`SHMEM_TOGGLE_ENV`] toggle).
/// Absent or anything but a falsey value ⇒ enabled.
pub fn shmem_enabled() -> bool {
    match std::env::var(SHMEM_TOGGLE_ENV) {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no"),
        Err(_) => true,
    }
}

// --- slot layout ---------------------------------------------------------------
//
// [0 ..  8)  seq : AtomicU64  — seqlock counter (even = stable, odd = write in progress)
// [8 .. 16)  len : AtomicU64  — current payload length in bytes
// [16 ..  )  data            — the opaque payload, up to `CAP`

const OFF_SEQ: usize = 0;
const OFF_LEN: usize = 8;
const HEADER: usize = 16;

/// Default payload ceiling (bytes) when `MICRO_BUS_CAP_BYTES` is unset: 128 MiB.
const DEFAULT_CAP: usize = 128 * 1024 * 1024;

/// Payload ceiling in bytes, resolved once per writer/reader from the environment.
///
/// A large payload (e.g. Frame's textured/meshed SceneSpec — GLB meshes + KTX2 maps) can run to
/// tens of MB, so the ceiling is configurable via `MICRO_BUS_CAP_BYTES` and defaults generously.
/// The backing file is sparse — only touched pages are ever resident — so a large ceiling is free
/// (a tiny payload uses a few hundred bytes). A blob larger than this is rejected by
/// [`StateWriter::write`]; the caller then falls back to its other transport.
///
/// Writer and reader must agree on the size: the reader is typically spawned as a child of the
/// writer and inherits its environment, so both observe the same value — and the default
/// covers the case where neither sets it. The value is stamped into each `StateWriter`/
/// `StateReader` at construction, so it's read once, never per frame.
fn cap_bytes() -> usize {
    std::env::var("MICRO_BUS_CAP_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n >= 4096)
        .unwrap_or(DEFAULT_CAP)
}

/// Bounded reader retry budget: a single infrequent writer means a collision is rare and
/// clears in nanoseconds, but we never spin unboundedly — past this we report "no fresh
/// value this tick" and the caller simply polls again.
const READ_RETRIES: u32 = 1024;

/// Read the `AtomicU64` living at `off` inside a mapped region. The mapping is page-aligned
/// and `off` is a multiple of 8, so the cast is well-aligned; the atomic governs all access
/// to the bytes around it, which is exactly what a seqlock is.
///
/// SAFETY: `base` must point at a live mapping of at least `off + 8` bytes (always true for
/// our `HEADER + cap`-sized maps), and every concurrent access to this location must go through the
/// returned atomic — which it does (the seqlock protocol is the only accessor of `seq`/`len`).
unsafe fn atomic_at(base: *const u8, off: usize) -> &'static AtomicU64 {
    &*(base.add(off) as *const AtomicU64)
}

// ==========================================
// UNIX IMPLEMENTATION (Linux & macOS)
// ==========================================

#[cfg(unix)]
fn new_shm_name() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("/micro-state-{}-{}", std::process::id(), nanos)
}

#[cfg(unix)]
pub struct StateWriter {
    id: String,
    mmap: memmap2::MmapMut,
    _file: std::fs::File,
    cap: usize,
}

#[cfg(unix)]
impl StateWriter {
    /// Create a fresh slot: allocate the backing POSIX shared memory object and map it.
    pub fn create() -> std::io::Result<Self> {
        use std::os::fd::FromRawFd;

        let id = new_shm_name();
        let name_cstr = std::ffi::CString::new(id.clone())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

        let fd = unsafe {
            libc::shm_open(
                name_cstr.as_ptr(),
                libc::O_CREAT | libc::O_RDWR | libc::O_EXCL,
                0o600,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }

        let cap = cap_bytes();
        let total = HEADER + cap;
        let ret = unsafe { libc::ftruncate(fd, total as libc::off_t) };
        if ret < 0 {
            let err = std::io::Error::last_os_error();
            unsafe {
                libc::close(fd);
                libc::shm_unlink(name_cstr.as_ptr());
            }
            return Err(err);
        }

        // Safety: fd is owned by us, std::fs::File takes ownership and will close it on drop.
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        let mmap = unsafe { memmap2::MmapOptions::new().len(total).map_mut(&file)? };

        Ok(Self { id, mmap, _file: file, cap })
    }

    /// The slot identifier to pass to the sidecar via [`SHMEM_ID_ENV`].
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Publish `bytes` as the new latest payload. Never blocks.
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), super::BusError> {
        if bytes.len() > self.cap {
            return Err(format!("state blob {} B exceeds shmem slot cap {} B", bytes.len(), self.cap));
        }
        let base = self.mmap.as_ptr();
        // SAFETY: atomic_at is safe as our region is sized to HEADER + cap.
        let seq = unsafe { atomic_at(base, OFF_SEQ) };
        let len = unsafe { atomic_at(base, OFF_LEN) };

        let start = seq.load(Ordering::Relaxed);
        seq.store(start.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);

        len.store(bytes.len() as u64, Ordering::Relaxed);
        self.mmap[HEADER..HEADER + bytes.len()].copy_from_slice(bytes);

        fence(Ordering::Release);
        seq.store(start.wrapping_add(2), Ordering::Release);
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for StateWriter {
    fn drop(&mut self) {
        if let Ok(name_cstr) = std::ffi::CString::new(self.id.clone()) {
            unsafe {
                libc::shm_unlink(name_cstr.as_ptr());
            }
        }
    }
}

#[cfg(unix)]
pub struct StateReader {
    mmap: memmap2::Mmap,
    _file: std::fs::File,
    cap: usize,
}

#[cfg(unix)]
impl StateReader {
    /// Open the slot the host created, by its POSIX shared memory name.
    pub fn open(id: &str) -> std::io::Result<Self> {
        use std::os::fd::FromRawFd;

        let name_cstr = std::ffi::CString::new(id)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

        let fd = unsafe {
            libc::shm_open(
                name_cstr.as_ptr(),
                libc::O_RDONLY,
                0,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }

        let cap = cap_bytes();
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        let mmap = unsafe { memmap2::MmapOptions::new().len(HEADER + cap).map(&file)? };

        Ok(Self { mmap, _file: file, cap })
    }

    /// Copy out the latest payload **iff it changed** since `last_seq`.
    pub fn read_latest(&self, last_seq: &mut u64) -> Option<Vec<u8>> {
        let base = self.mmap.as_ptr();
        let seq = unsafe { atomic_at(base, OFF_SEQ) };
        let len = unsafe { atomic_at(base, OFF_LEN) };

        for _ in 0..READ_RETRIES {
            let s1 = seq.load(Ordering::Acquire);
            if s1 & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            if s1 == 0 {
                return None;
            }
            if s1 == *last_seq {
                return None;
            }
            let n = len.load(Ordering::Relaxed) as usize;
            if n > self.cap {
                std::hint::spin_loop();
                continue;
            }
            let mut out = vec![0u8; n];
            out.copy_from_slice(&self.mmap[HEADER..HEADER + n]);
            fence(Ordering::Acquire);
            let s2 = seq.load(Ordering::Acquire);
            if s1 == s2 {
                *last_seq = s1;
                return Some(out);
            }
        }
        None
    }
}

// ==========================================
// WINDOWS IMPLEMENTATION (Named Shared Memory)
// ==========================================

#[cfg(windows)]
type HANDLE = *mut std::ffi::c_void;
#[cfg(windows)]
type LPVOID = *mut std::ffi::c_void;

#[cfg(windows)]
const PAGE_READWRITE: u32 = 0x04;
#[cfg(windows)]
const FILE_MAP_ALL_ACCESS: u32 = 0xF001F;
#[cfg(windows)]
const FILE_MAP_READ: u32 = 0x0004;

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn CreateFileMappingW(
        hFile: HANDLE,
        lpFileMappingAttributes: *mut std::ffi::c_void,
        flProtect: u32,
        dwMaximumSizeHigh: u32,
        dwMaximumSizeLow: u32,
        lpName: *const u16,
    ) -> HANDLE;

    fn OpenFileMappingW(
        dwDesiredAccess: u32,
        bInheritHandle: i32,
        lpName: *const u16,
    ) -> HANDLE;

    fn MapViewOfFile(
        hFileMappingObject: HANDLE,
        dwDesiredAccess: u32,
        dwFileOffsetHigh: u32,
        dwFileOffsetLow: u32,
        dwNumberOfBytesToMap: usize,
    ) -> LPVOID;

    fn UnmapViewOfFile(lpBaseAddress: LPVOID) -> i32;

    fn CloseHandle(hObject: HANDLE) -> i32;
}

#[cfg(windows)]
fn new_shm_name() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("Local\\micro-state-{}-{}", std::process::id(), nanos)
}

#[cfg(windows)]
fn to_wide_string(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(windows)]
pub struct StateWriter {
    id: String,
    ptr: *mut u8,
    handle: HANDLE,
    cap: usize,
}

#[cfg(windows)]
unsafe impl Send for StateWriter {}
#[cfg(windows)]
unsafe impl Sync for StateWriter {}

#[cfg(windows)]
impl StateWriter {
    /// Create a fresh slot: allocate the backing named shared memory object and map it.
    pub fn create() -> std::io::Result<Self> {
        let id = new_shm_name();
        let wide_name = to_wide_string(&id);
        let cap = cap_bytes();
        let total = HEADER + cap;

        let handle = unsafe {
            CreateFileMappingW(
                !0 as HANDLE, // INVALID_HANDLE_VALUE: backed by paging file
                std::ptr::null_mut(),
                PAGE_READWRITE,
                // Split the 64-bit size across the high/low dwords. With the cap now configurable,
                // `total` can exceed u32::MAX; passing only `total as u32` (high dword 0) would
                // silently undersize the mapping object while MapViewOfFile below still requests
                // the full `total`, yielding a failed or truncated map.
                (total as u64 >> 32) as u32,
                (total as u64 & 0xFFFF_FFFF) as u32,
                wide_name.as_ptr(),
            )
        };

        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }

        let ptr = unsafe {
            MapViewOfFile(
                handle,
                FILE_MAP_ALL_ACCESS,
                0,
                0,
                total,
            )
        };

        if ptr.is_null() {
            let err = std::io::Error::last_os_error();
            unsafe {
                CloseHandle(handle);
            }
            return Err(err);
        }

        Ok(Self {
            id,
            ptr: ptr as *mut u8,
            handle,
            cap,
        })
    }

    /// The slot identifier to pass to the sidecar via [`SHMEM_ID_ENV`].
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Publish `bytes` as the new latest payload. Never blocks.
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), super::BusError> {
        if bytes.len() > self.cap {
            return Err(format!("state blob {} B exceeds shmem slot cap {} B", bytes.len(), self.cap));
        }

        let base = self.ptr;
        let seq = unsafe { atomic_at(base, OFF_SEQ) };
        let len = unsafe { atomic_at(base, OFF_LEN) };

        let start = seq.load(Ordering::Relaxed);
        seq.store(start.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);

        len.store(bytes.len() as u64, Ordering::Relaxed);
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), base.add(HEADER), bytes.len());
        }

        fence(Ordering::Release);
        seq.store(start.wrapping_add(2), Ordering::Release);
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for StateWriter {
    fn drop(&mut self) {
        unsafe {
            UnmapViewOfFile(self.ptr as LPVOID);
            CloseHandle(self.handle);
        }
    }
}

#[cfg(windows)]
pub struct StateReader {
    ptr: *mut u8,
    handle: HANDLE,
    cap: usize,
}

#[cfg(windows)]
unsafe impl Send for StateReader {}
#[cfg(windows)]
unsafe impl Sync for StateReader {}

#[cfg(windows)]
impl StateReader {
    /// Open the slot the host created, by its named shared memory name.
    pub fn open(id: &str) -> std::io::Result<Self> {
        let wide_name = to_wide_string(id);

        let cap = cap_bytes();
        let handle = unsafe {
            OpenFileMappingW(
                FILE_MAP_READ,
                0,
                wide_name.as_ptr(),
            )
        };

        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }

        let ptr = unsafe {
            MapViewOfFile(
                handle,
                FILE_MAP_READ,
                0,
                0,
                HEADER + cap,
            )
        };

        if ptr.is_null() {
            let err = std::io::Error::last_os_error();
            unsafe {
                CloseHandle(handle);
            }
            return Err(err);
        }

        Ok(Self {
            ptr: ptr as *mut u8,
            handle,
            cap,
        })
    }

    /// Copy out the latest payload **iff it changed** since `last_seq`.
    pub fn read_latest(&self, last_seq: &mut u64) -> Option<Vec<u8>> {
        let base = self.ptr;
        let seq = unsafe { atomic_at(base, OFF_SEQ) };
        let len = unsafe { atomic_at(base, OFF_LEN) };

        for _ in 0..READ_RETRIES {
            let s1 = seq.load(Ordering::Acquire);
            if s1 & 1 != 0 {
                std::hint::spin_loop();
                continue;
            }
            if s1 == 0 {
                return None;
            }
            if s1 == *last_seq {
                return None;
            }
            let n = len.load(Ordering::Relaxed) as usize;
            if n > self.cap {
                std::hint::spin_loop();
                continue;
            }
            let mut out = vec![0u8; n];
            unsafe {
                std::ptr::copy_nonoverlapping(base.add(HEADER), out.as_mut_ptr(), n);
            }
            fence(Ordering::Acquire);
            let s2 = seq.load(Ordering::Acquire);
            if s1 == s2 {
                *last_seq = s1;
                return Some(out);
            }
        }
        None
    }
}

#[cfg(windows)]
impl Drop for StateReader {
    fn drop(&mut self) {
        unsafe {
            UnmapViewOfFile(self.ptr as LPVOID);
            CloseHandle(self.handle);
        }
    }
}

// ==========================================
// TESTS
// ==========================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering as O};
    use std::sync::Arc;

    #[test]
    fn write_then_read_latest_is_change_tracked() {
        let mut w = StateWriter::create().unwrap();
        let r = StateReader::open(w.id()).unwrap();

        let mut seen = 0u64;
        // Empty slot → nothing yet.
        assert_eq!(r.read_latest(&mut seen), None);

        w.write(b"alpha").unwrap();
        assert_eq!(r.read_latest(&mut seen).as_deref(), Some(&b"alpha"[..]));
        // Unchanged → None (the render loop won't rebuild).
        assert_eq!(r.read_latest(&mut seen), None);

        // Latest-wins: only the newest survives, history is overwritten.
        w.write(b"beta").unwrap();
        w.write(b"gamma").unwrap();
        assert_eq!(r.read_latest(&mut seen).as_deref(), Some(&b"gamma"[..]));
        assert_eq!(r.read_latest(&mut seen), None);
    }

    #[test]
    fn oversized_blob_is_rejected_value_preserved() {
        let mut w = StateWriter::create().unwrap();
        let r = StateReader::open(w.id()).unwrap();
        w.write(b"keep").unwrap();
        let too_big = vec![0u8; w.cap + 1];
        assert!(w.write(&too_big).is_err());
        // The prior value must still be readable (write was rejected, not half-applied).
        let mut seen = 0u64;
        assert_eq!(r.read_latest(&mut seen).as_deref(), Some(&b"keep"[..]));
    }

    #[test]
    fn reader_never_sees_a_torn_frame_under_concurrent_writes() {
        let mut w = StateWriter::create().unwrap();
        let r = StateReader::open(w.id()).unwrap();
        let stop = Arc::new(AtomicBool::new(false));

        let writer = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut k: u8 = 1;
                while !stop.load(O::Relaxed) {
                    let frame = vec![k; 4096];
                    w.write(&frame).unwrap();
                    k = k.wrapping_add(1).max(1);
                }
            })
        };

        let mut seen = 0u64;
        let mut reads = 0;
        for _ in 0..200_000 {
            if let Some(frame) = r.read_latest(&mut seen) {
                assert_eq!(frame.len(), 4096);
                let first = frame[0];
                assert!(frame.iter().all(|&b| b == first), "torn frame: not all bytes equal");
                reads += 1;
            }
        }
        stop.store(true, O::Relaxed);
        writer.join().unwrap();
        assert!(reads > 0, "reader should have observed at least one frame");
    }
}
