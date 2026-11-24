#[cfg(not(feature = "alpha"))]
compile_error!("缺少 alpha feature");
#[path = "../probe.rs"]
mod probe;
fn main() { probe::run(); }
