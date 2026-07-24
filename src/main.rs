//! Thin binary entry point. All logic lives in `cprog::run()` (see docs/architecture.md).

fn main() {
    std::process::exit(cprog::run());
}
