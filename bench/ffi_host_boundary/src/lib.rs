use std::hint::black_box;

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn flow_host_now_ns() -> u64 {
    monotonic_now_ns()
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn flow_host_noop(x: u64) -> u64 {
    x.wrapping_add(1)
}

#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn flow_host_add_u64(a: u64, b: u64) -> u64 {
    a.wrapping_add(b)
}

#[unsafe(no_mangle)]
pub extern "C" fn flow_host_bench_iterations() -> u64 {
    std::env::var("FLOW_FFI_ITERS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(10_000_000)
}

#[inline(always)]
pub fn rust_inline_add(a: u64, b: u64) -> u64 {
    a.wrapping_add(b)
}

#[inline(never)]
pub fn rust_fn_add(a: u64, b: u64) -> u64 {
    a.wrapping_add(b)
}

pub fn monotonic_now_ns() -> u64 {
    unsafe {
        let mut ts: libc::timespec = std::mem::zeroed();
        if libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) != 0 {
            return 0;
        }
        (ts.tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(ts.tv_nsec as u64)
    }
}
