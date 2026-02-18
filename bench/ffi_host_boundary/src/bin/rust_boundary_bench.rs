use std::hint::black_box;

use flow_ffi_host_boundary::{flow_host_add_u64, flow_host_noop, monotonic_now_ns, rust_fn_add, rust_inline_add};

#[derive(Debug)]
struct BenchResult {
    label: &'static str,
    ns_total: u64,
    ns_per_op: f64,
    checksum: u64,
}

fn finish(label: &'static str, iterations: u64, start: u64, acc: u64) -> BenchResult {
    let end = monotonic_now_ns();
    let total = end.saturating_sub(start);
    BenchResult {
        label,
        ns_total: total,
        ns_per_op: total as f64 / iterations as f64,
        checksum: acc,
    }
}

fn bench_inline_add(iterations: u64) -> BenchResult {
    let mut acc = black_box(0_u64);
    let start = monotonic_now_ns();
    for i in 0..iterations {
        acc = black_box(rust_inline_add(black_box(acc), black_box(i)));
    }
    finish("rust_inline_add", iterations, start, acc)
}

fn bench_fn_add(iterations: u64) -> BenchResult {
    let mut acc = black_box(0_u64);
    let start = monotonic_now_ns();
    for i in 0..iterations {
        acc = black_box(rust_fn_add(black_box(acc), black_box(i)));
    }
    finish("rust_fn_add", iterations, start, acc)
}

fn bench_extern_add(iterations: u64) -> BenchResult {
    let mut acc = black_box(0_u64);
    let start = monotonic_now_ns();
    for i in 0..iterations {
        acc = black_box(flow_host_add_u64(black_box(acc), black_box(i)));
    }
    finish("rust_extern_add", iterations, start, acc)
}

fn bench_noop(iterations: u64) -> BenchResult {
    let mut acc = black_box(0_u64);
    let start = monotonic_now_ns();
    for _ in 0..iterations {
        acc = black_box(flow_host_noop(black_box(acc)));
    }
    finish("rust_extern_noop", iterations, start, acc)
}

fn parse_iters() -> u64 {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--iters" {
            if let Some(value) = args.next() {
                if let Ok(parsed) = value.parse::<u64>() {
                    if parsed > 0 {
                        return parsed;
                    }
                }
            }
        }
    }
    10_000_000
}

fn print_result(result: &BenchResult) {
    println!(
        "{} ns_total={} ns_per_op={:.4} checksum={}",
        result.label, result.ns_total, result.ns_per_op, result.checksum
    );
}

fn main() {
    let iterations = parse_iters();
    println!("rust_boundary_bench iterations={}", iterations);

    let inline = bench_inline_add(iterations);
    let fn_call = bench_fn_add(iterations);
    let extern_call = bench_extern_add(iterations);
    let noop = bench_noop(iterations);

    print_result(&inline);
    print_result(&fn_call);
    print_result(&extern_call);
    print_result(&noop);
}
