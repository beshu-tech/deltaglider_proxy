// SPDX-License-Identifier: BUSL-1.1

//! Source guard: every test spawn of the proxy binary sets `current_dir`.
//! The proxy writes state files (`.deltaglider_bootstrap_hash`, and the
//! config DB and its key file when `DGP_CONFIG` is unset) relative to its
//! cwd. A spawn that inherits the test process's cwd writes them into the
//! repo root, where a later dev run of the binary picks them up.

#[test]
fn spawns_set_current_dir() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests");
    let mut offenders = Vec::new();
    for entry in walkdir::WalkDir::new(dir) {
        let entry = entry.unwrap();
        if entry.path().extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(entry.path()).unwrap();
        // The needle is built at runtime so this file does not match itself.
        let bin_needle = format!("Command::new(env!(\"{}", "CARGO_BIN_EXE");
        let spawns = text.matches(&bin_needle).count() + text.matches("Command::new(BIN)").count();
        let cwds = text.matches(".current_dir(").count();
        if spawns > cwds {
            offenders.push(format!(
                "{}: {spawns} spawn(s), {cwds} current_dir",
                entry.path().display()
            ));
        }
    }
    assert!(
        offenders.is_empty(),
        "spawn the proxy with .current_dir(<temp dir>): {offenders:?}"
    );
}
