#[cfg(not(all(feature = "alpha", feature = "beta")))]
compile_error!("缺少 alpha 或 beta feature");
#[path = "../probe.rs"]
mod probe;
fn main() { probe::run(); }
