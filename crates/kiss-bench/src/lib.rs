//! Small timing helpers for ignored nextest performance tests.

use std::hint::black_box;
use std::time::Instant;

/// Measure a synchronous workload and print one stable, machine-readable row.
pub fn measure<T>(
    name: &str,
    samples: usize,
    iterations: usize,
    work: &str,
    mut workload: impl FnMut() -> T,
) {
    assert!(samples > 0, "a benchmark needs at least one sample");
    assert!(iterations > 0, "a benchmark needs at least one iteration");

    for _ in 0..iterations.min(3) {
        black_box(workload());
    }

    let mut elapsed = Vec::with_capacity(samples);
    for _ in 0..samples {
        let started = Instant::now();
        for _ in 0..iterations {
            black_box(workload());
        }
        elapsed.push(started.elapsed().as_nanos() / iterations as u128);
    }
    report(name, &mut elapsed, iterations, work);
}

/// Print pre-measured per-iteration samples in the suite output format.
pub fn report(name: &str, elapsed_ns: &mut [u128], iterations: usize, work: &str) {
    assert!(
        !elapsed_ns.is_empty(),
        "a benchmark needs at least one sample"
    );
    elapsed_ns.sort_unstable();
    let median = elapsed_ns[elapsed_ns.len() / 2];
    let p95_index = (elapsed_ns.len() * 95).div_ceil(100).saturating_sub(1);
    let p95 = elapsed_ns[p95_index];
    println!(
        "KISS_BENCH\t{name}\tmedian_ns={median}\tp95_ns={p95}\tsamples={}\titerations={iterations}\twork={work}",
        elapsed_ns.len()
    );
}
