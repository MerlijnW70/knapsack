// `cargo bench` entry point. harness = false, so this is just a main() that runs the
// shared A/B/C benchmark in the library (same output as `knapsack bench`).
fn main() {
    knapsack::bench::run();
}
