use std::println;
pub fn run() {
    let expected = std::env::var("PANEL_EXPECT_ARGUMENT").unwrap();
    assert_eq!(std::env::args().skip(1).collect::<Vec<_>>(), [expected]);
    println!("RUST_LAUNCH_OK");
    if std::env::var("PANEL_PROBE_SLEEP").is_ok() { std::thread::sleep(std::time::Duration::from_secs(30)); }
}
