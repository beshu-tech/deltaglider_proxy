fn main() {
    // Embed UTC build timestamp so the binary always knows when it was compiled.
    let now = time_now_utc();
    println!("cargo:rustc-env=DGP_BUILD_TIME={now}");
}

/// Minimal UTC timestamp without pulling in chrono for the build script.
fn time_now_utc() -> String {
    use std::process::Command;
    // Works on macOS, Linux, and CI runners
    let output = Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .expect("failed to run `date` command");
    String::from_utf8(output.stdout)
        .expect("invalid UTF-8 from date")
        .trim()
        .to_string()
}
