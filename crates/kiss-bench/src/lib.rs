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

fn measure_iterations<T>(iterations: usize, workload: &mut impl FnMut() -> T) -> u128 {
    let started = Instant::now();
    for _ in 0..iterations {
        black_box(workload());
    }
    started.elapsed().as_nanos() / iterations as u128
}

/// Measure two matched workloads in alternating sample order.
///
/// This reduces process drift when the expected difference is much smaller
/// than the total operation time.
pub fn measure_pair<A, B>(
    names: (&str, &str),
    samples: usize,
    iterations: usize,
    work: (&str, &str),
    mut first: impl FnMut() -> A,
    mut second: impl FnMut() -> B,
) {
    assert!(samples > 0, "a benchmark needs at least one sample");
    assert!(iterations > 0, "a benchmark needs at least one iteration");

    for _ in 0..iterations.min(3) {
        black_box(first());
        black_box(second());
    }

    let mut first_elapsed = Vec::with_capacity(samples);
    let mut second_elapsed = Vec::with_capacity(samples);
    for sample in 0..samples {
        let (first_ns, second_ns) = if sample % 2 == 0 {
            (
                measure_iterations(iterations, &mut first),
                measure_iterations(iterations, &mut second),
            )
        } else {
            let second_ns = measure_iterations(iterations, &mut second);
            let first_ns = measure_iterations(iterations, &mut first);
            (first_ns, second_ns)
        };
        first_elapsed.push(first_ns);
        second_elapsed.push(second_ns);
    }
    report(names.0, &mut first_elapsed, iterations, work.0);
    report(names.1, &mut second_elapsed, iterations, work.1);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn paired_measurement_alternates_sample_order() {
        let calls = RefCell::new(Vec::new());
        measure_pair(
            ("first", "second"),
            2,
            1,
            ("one", "one"),
            || calls.borrow_mut().push("first"),
            || calls.borrow_mut().push("second"),
        );
        assert_eq!(
            calls.into_inner(),
            ["first", "second", "first", "second", "second", "first"]
        );
    }
}
