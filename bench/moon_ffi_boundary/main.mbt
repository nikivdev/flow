extern "C" fn flow_host_now_ns() -> UInt64 = "flow_host_now_ns"
extern "C" fn flow_host_noop(x : UInt64) -> UInt64 = "flow_host_noop"
extern "C" fn flow_host_add_u64(a : UInt64, b : UInt64) -> UInt64 = "flow_host_add_u64"
extern "C" fn flow_host_bench_iterations() -> UInt64 = "flow_host_bench_iterations"

fn bench_ffi_add(iterations : Int) -> (UInt64, UInt64) {
  let mut acc : UInt64 = 0UL
  let start = flow_host_now_ns()
  for i = 0; i < iterations; i = i + 1 {
    acc = flow_host_add_u64(acc, i.to_uint64())
  }
  let elapsed = flow_host_now_ns() - start
  (elapsed, acc)
}

fn bench_ffi_noop(iterations : Int) -> (UInt64, UInt64) {
  let mut acc : UInt64 = 0UL
  let start = flow_host_now_ns()
  for _i = 0; _i < iterations; _i = _i + 1 {
    acc = flow_host_noop(acc)
  }
  let elapsed = flow_host_now_ns() - start
  (elapsed, acc)
}

fn bench_moon_add(iterations : Int) -> (UInt64, UInt64) {
  let mut acc : UInt64 = 0UL
  let start = flow_host_now_ns()
  for i = 0; i < iterations; i = i + 1 {
    acc = acc + i.to_uint64()
  }
  let elapsed = flow_host_now_ns() - start
  (elapsed, acc)
}

fn parse_iterations() -> Int {
  flow_host_bench_iterations().to_int()
}

fn print_result(
  label : String,
  total_ns : UInt64,
  checksum : UInt64,
  iterations : Int,
) -> Unit {
  let per_op = total_ns / iterations.to_uint64()
  println(
    label +
    " ns_total=" + total_ns.to_string() +
    " ns_per_op=" + per_op.to_string() +
    " checksum=" + checksum.to_string(),
  )
}

fn main {
  let iterations = parse_iterations()
  println("moon_ffi_boundary iterations=" + iterations.to_string())

  let (moon_add_ns, moon_add_sum) = bench_moon_add(iterations)
  let (ffi_add_ns, ffi_add_sum) = bench_ffi_add(iterations)
  let (ffi_noop_ns, ffi_noop_sum) = bench_ffi_noop(iterations)

  print_result("moon_add", moon_add_ns, moon_add_sum, iterations)
  print_result("moon_ffi_add", ffi_add_ns, ffi_add_sum, iterations)
  print_result("moon_ffi_noop", ffi_noop_ns, ffi_noop_sum, iterations)
}
