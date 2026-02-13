//! Zero-cost request tracing via mmap ring buffer.
//!
//! Inspired by fishx's observe.rs - uses mmap + atomic index for lock-free,
//! allocation-free request recording.

use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::ptr::{null_mut, write_unaligned};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use libc::{CLOCK_MONOTONIC, MAP_SHARED, PROT_READ, PROT_WRITE};

// Magic bytes to identify trace files
const TRACE_MAGIC: &[u8; 8] = b"PROXYTRC";
const TRACE_VERSION: u32 = 1;

// Record layout - 128 bytes per request
const TRACE_PATH_BYTES: usize = 64;
const TRACE_RECORD_SIZE: usize = 128;
const TRACE_HEADER_SIZE: usize = 64;
const TRACE_DEFAULT_SIZE: usize = 16 * 1024 * 1024; // 16MB default

// Field indices in the record (as u64 words)
const IDX_TS_NS: usize = 0;
const IDX_REQ_ID: usize = 1;
const IDX_LATENCY_STATUS: usize = 2; // latency_us (32) | status (16) | method (8) | flags (8)
const IDX_BYTES: usize = 3; // bytes_in (32) | bytes_out (32)
const IDX_TARGET_PATH_LEN: usize = 4; // target_idx (8) | path_len (8) | trace_id_high (48)
const IDX_TRACE_ID_LOW: usize = 5;
const IDX_PATH_HASH: usize = 6;
const IDX_UPSTREAM_LATENCY: usize = 7; // upstream_latency_us (32) | reserved (32)
// Remaining 64 bytes = path prefix

/// HTTP methods encoded as u8
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Unknown = 0,
    Get = 1,
    Post = 2,
    Put = 3,
    Delete = 4,
    Patch = 5,
    Head = 6,
    Options = 7,
    Connect = 8,
    Trace = 9,
}

impl From<&str> for Method {
    fn from(s: &str) -> Self {
        match s.to_uppercase().as_str() {
            "GET" => Method::Get,
            "POST" => Method::Post,
            "PUT" => Method::Put,
            "DELETE" => Method::Delete,
            "PATCH" => Method::Patch,
            "HEAD" => Method::Head,
            "OPTIONS" => Method::Options,
            "CONNECT" => Method::Connect,
            "TRACE" => Method::Trace,
            _ => Method::Unknown,
        }
    }
}

/// Trace record header (64 bytes, at start of mmap file)
#[repr(C)]
struct TraceHeader {
    magic: [u8; 8],
    version: u32,
    record_size: u32,
    capacity: u64,
    write_index: AtomicU64,
    req_counter: AtomicU64,
    // Target names stored after header, before records
    target_count: u32,
    _reserved: [u8; 20],
}

/// A single trace record (128 bytes)
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TraceRecord {
    words: [u64; 8],
    path: [u8; TRACE_PATH_BYTES],
}

impl TraceRecord {
    pub fn new() -> Self {
        Self {
            words: [0; 8],
            path: [0; TRACE_PATH_BYTES],
        }
    }

    #[inline]
    pub fn set_timestamp(&mut self, ts_ns: u64) {
        self.words[IDX_TS_NS] = ts_ns;
    }

    #[inline]
    pub fn set_req_id(&mut self, req_id: u64) {
        self.words[IDX_REQ_ID] = req_id;
    }

    #[inline]
    pub fn set_latency_status(&mut self, latency_us: u32, status: u16, method: Method, flags: u8) {
        self.words[IDX_LATENCY_STATUS] =
            (latency_us as u64) << 32 | (status as u64) << 16 | (method as u64) << 8 | flags as u64;
    }

    #[inline]
    pub fn set_bytes(&mut self, bytes_in: u32, bytes_out: u32) {
        self.words[IDX_BYTES] = (bytes_in as u64) << 32 | bytes_out as u64;
    }

    #[inline]
    pub fn set_target_and_trace_id(&mut self, target_idx: u8, path_len: u8, trace_id: u128) {
        let trace_id_high = (trace_id >> 64) as u64;
        let trace_id_low = trace_id as u64;
        self.words[IDX_TARGET_PATH_LEN] = (target_idx as u64) << 56
            | (path_len as u64) << 48
            | (trace_id_high & 0xFFFF_FFFF_FFFF);
        self.words[IDX_TRACE_ID_LOW] = trace_id_low;
    }

    #[inline]
    pub fn set_path_hash(&mut self, hash: u64) {
        self.words[IDX_PATH_HASH] = hash;
    }

    #[inline]
    pub fn set_upstream_latency(&mut self, upstream_latency_us: u32) {
        self.words[IDX_UPSTREAM_LATENCY] = (upstream_latency_us as u64) << 32;
    }

    #[inline]
    pub fn set_path(&mut self, path: &str) {
        let bytes = path.as_bytes();
        let len = bytes.len().min(TRACE_PATH_BYTES);
        self.path[..len].copy_from_slice(&bytes[..len]);
    }

    // Getters for reading records
    #[inline]
    pub fn timestamp(&self) -> u64 {
        self.words[IDX_TS_NS]
    }

    #[inline]
    pub fn req_id(&self) -> u64 {
        self.words[IDX_REQ_ID]
    }

    #[inline]
    pub fn latency_us(&self) -> u32 {
        (self.words[IDX_LATENCY_STATUS] >> 32) as u32
    }

    #[inline]
    pub fn status(&self) -> u16 {
        ((self.words[IDX_LATENCY_STATUS] >> 16) & 0xFFFF) as u16
    }

    #[inline]
    pub fn method(&self) -> Method {
        match ((self.words[IDX_LATENCY_STATUS] >> 8) & 0xFF) as u8 {
            1 => Method::Get,
            2 => Method::Post,
            3 => Method::Put,
            4 => Method::Delete,
            5 => Method::Patch,
            6 => Method::Head,
            7 => Method::Options,
            8 => Method::Connect,
            9 => Method::Trace,
            _ => Method::Unknown,
        }
    }

    #[inline]
    pub fn flags(&self) -> u8 {
        (self.words[IDX_LATENCY_STATUS] & 0xFF) as u8
    }

    #[inline]
    pub fn bytes_in(&self) -> u32 {
        (self.words[IDX_BYTES] >> 32) as u32
    }

    #[inline]
    pub fn bytes_out(&self) -> u32 {
        (self.words[IDX_BYTES] & 0xFFFF_FFFF) as u32
    }

    #[inline]
    pub fn target_idx(&self) -> u8 {
        (self.words[IDX_TARGET_PATH_LEN] >> 56) as u8
    }

    #[inline]
    pub fn path_len(&self) -> u8 {
        ((self.words[IDX_TARGET_PATH_LEN] >> 48) & 0xFF) as u8
    }

    #[inline]
    pub fn trace_id(&self) -> u128 {
        let high = (self.words[IDX_TARGET_PATH_LEN] & 0xFFFF_FFFF_FFFF) as u128;
        let low = self.words[IDX_TRACE_ID_LOW] as u128;
        (high << 64) | low
    }

    #[inline]
    pub fn path_hash(&self) -> u64 {
        self.words[IDX_PATH_HASH]
    }

    #[inline]
    pub fn upstream_latency_us(&self) -> u32 {
        (self.words[IDX_UPSTREAM_LATENCY] >> 32) as u32
    }

    #[inline]
    pub fn path(&self) -> &str {
        let len = self.path_len() as usize;
        std::str::from_utf8(&self.path[..len.min(TRACE_PATH_BYTES)]).unwrap_or("")
    }

    /// Check if this is an error response
    #[inline]
    pub fn is_error(&self) -> bool {
        self.status() >= 400
    }

    /// Check if this is a slow request (> threshold_ms)
    #[inline]
    pub fn is_slow(&self, threshold_ms: u32) -> bool {
        self.latency_us() > threshold_ms * 1000
    }
}

impl Default for TraceRecord {
    fn default() -> Self {
        Self::new()
    }
}

/// The trace buffer state (mmap handle)
pub struct TraceBuffer {
    _map: *mut u8,
    _map_len: usize,
    header: *mut TraceHeader,
    records: *mut u8,
    capacity: u64,
    start_time: Instant,
}

// Safety: The mmap is process-local and we use atomic operations
unsafe impl Send for TraceBuffer {}
unsafe impl Sync for TraceBuffer {}

impl TraceBuffer {
    /// Initialize a new trace buffer at the given path
    pub fn init(dir: &PathBuf, size: usize) -> Option<Self> {
        if std::fs::create_dir_all(dir).is_err() {
            return None;
        }

        let pid = unsafe { libc::getpid() };
        let filename = format!("trace.{}.bin", pid);
        let path = dir.join(filename);

        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .ok()?;

        // Ensure file is the right size
        if set_file_len(&file, size).is_err() {
            return None;
        }

        let map = unsafe {
            libc::mmap(
                null_mut(),
                size,
                PROT_READ | PROT_WRITE,
                MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if map == libc::MAP_FAILED {
            return None;
        }

        let header = map as *mut TraceHeader;
        let records = unsafe { (map as *mut u8).add(TRACE_HEADER_SIZE) };
        let capacity = ((size - TRACE_HEADER_SIZE) / TRACE_RECORD_SIZE) as u64;

        if capacity == 0 {
            unsafe {
                libc::munmap(map, size);
            }
            return None;
        }

        // Initialize or validate header
        unsafe {
            if (*header).magic != *TRACE_MAGIC
                || (*header).version != TRACE_VERSION
                || (*header).record_size != TRACE_RECORD_SIZE as u32
                || (*header).capacity != capacity
            {
                write_unaligned(
                    header,
                    TraceHeader {
                        magic: *TRACE_MAGIC,
                        version: TRACE_VERSION,
                        record_size: TRACE_RECORD_SIZE as u32,
                        capacity,
                        write_index: AtomicU64::new(0),
                        req_counter: AtomicU64::new(0),
                        target_count: 0,
                        _reserved: [0; 20],
                    },
                );
            }
        }

        Some(TraceBuffer {
            _map: map as *mut u8,
            _map_len: size,
            header,
            records,
            capacity,
            start_time: Instant::now(),
        })
    }

    /// Record a completed request (zero allocations)
    #[inline]
    pub fn record(&self, record: &TraceRecord) {
        let idx = unsafe { (*self.header).write_index.fetch_add(1, Ordering::Relaxed) };
        let slot = (idx % self.capacity) as usize;
        let dst = unsafe { self.records.add(slot * TRACE_RECORD_SIZE) as *mut TraceRecord };
        unsafe { write_unaligned(dst, *record) };
    }

    /// Get the next request ID (monotonically increasing)
    #[inline]
    pub fn next_req_id(&self) -> u64 {
        unsafe { (*self.header).req_counter.fetch_add(1, Ordering::Relaxed) }
    }

    /// Get current write index
    #[inline]
    pub fn write_index(&self) -> u64 {
        unsafe { (*self.header).write_index.load(Ordering::Relaxed) }
    }

    /// Get capacity (number of records)
    #[inline]
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Read a record at a given index (wraps around)
    #[inline]
    pub fn read(&self, idx: u64) -> TraceRecord {
        let slot = (idx % self.capacity) as usize;
        let src = unsafe { self.records.add(slot * TRACE_RECORD_SIZE) as *const TraceRecord };
        unsafe { std::ptr::read_unaligned(src) }
    }

    /// Iterate over recent records (most recent first)
    pub fn recent(&self, count: usize) -> Vec<TraceRecord> {
        let write_idx = self.write_index();
        let count = count.min(write_idx as usize).min(self.capacity as usize);
        let mut records = Vec::with_capacity(count);

        for i in 0..count {
            let idx = write_idx.saturating_sub(1 + i as u64);
            records.push(self.read(idx));
        }

        records
    }

    /// Iterate over records matching a predicate
    pub fn filter<F>(&self, count: usize, predicate: F) -> Vec<TraceRecord>
    where
        F: Fn(&TraceRecord) -> bool,
    {
        let write_idx = self.write_index();
        let max_scan = (self.capacity as usize).min(write_idx as usize);
        let mut records = Vec::new();

        for i in 0..max_scan {
            if records.len() >= count {
                break;
            }
            let idx = write_idx.saturating_sub(1 + i as u64);
            let record = self.read(idx);
            if predicate(&record) {
                records.push(record);
            }
        }

        records
    }

    /// Get timestamp of buffer creation
    pub fn start_time(&self) -> Instant {
        self.start_time
    }
}

impl Drop for TraceBuffer {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self._map as *mut libc::c_void, self._map_len);
        }
    }
}

/// Global trace buffer (lazily initialized)
static TRACE_BUFFER: OnceLock<Option<TraceBuffer>> = OnceLock::new();

/// Initialize the global trace buffer
pub fn init_global(dir: PathBuf, size: usize) -> bool {
    TRACE_BUFFER
        .get_or_init(|| TraceBuffer::init(&dir, size))
        .is_some()
}

/// Get the global trace buffer
pub fn global() -> Option<&'static TraceBuffer> {
    TRACE_BUFFER.get().and_then(|b| b.as_ref())
}

/// Record a request to the global buffer
#[inline]
pub fn record(record: &TraceRecord) {
    if let Some(buf) = global() {
        buf.record(record);
    }
}

/// Get next request ID from global buffer
#[inline]
pub fn next_req_id() -> u64 {
    global().map(|b| b.next_req_id()).unwrap_or(0)
}

// Helper: get monotonic time in nanoseconds
pub fn now_ns() -> u64 {
    unsafe {
        let mut ts = std::mem::MaybeUninit::<libc::timespec>::uninit();
        if libc::clock_gettime(CLOCK_MONOTONIC, ts.as_mut_ptr()) == 0 {
            let ts = ts.assume_init();
            return (ts.tv_sec as u64)
                .saturating_mul(1_000_000_000)
                .saturating_add(ts.tv_nsec as u64);
        }
    }
    0
}

// Helper: FNV-1a hash for path strings
pub fn hash_path(path: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in path.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

// Helper: set file length
fn set_file_len(file: &std::fs::File, size: usize) -> std::io::Result<()> {
    let fd = file.as_raw_fd();
    let res = unsafe { libc::ftruncate(fd, size as libc::off_t) };
    if res == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Get the default trace directory
pub fn default_trace_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("flow")
        .join("proxy")
}

/// Get the default trace size
pub fn default_trace_size() -> usize {
    TRACE_DEFAULT_SIZE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_roundtrip() {
        let mut record = TraceRecord::new();
        record.set_timestamp(12345678);
        record.set_req_id(42);
        record.set_latency_status(1500, 200, Method::Get, 0);
        record.set_bytes(100, 2048);
        record.set_target_and_trace_id(1, 10, 0xDEADBEEF);
        record.set_path_hash(hash_path("/api/users"));
        record.set_upstream_latency(1200);
        record.set_path("/api/users");

        assert_eq!(record.timestamp(), 12345678);
        assert_eq!(record.req_id(), 42);
        assert_eq!(record.latency_us(), 1500);
        assert_eq!(record.status(), 200);
        assert_eq!(record.method(), Method::Get);
        assert_eq!(record.bytes_in(), 100);
        assert_eq!(record.bytes_out(), 2048);
        assert_eq!(record.target_idx(), 1);
        assert_eq!(record.upstream_latency_us(), 1200);
        assert_eq!(record.path(), "/api/users");
    }
}
