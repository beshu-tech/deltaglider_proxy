// SPDX-License-Identifier: BUSL-1.1

fn main() {
    // Embed UTC build timestamp so the binary always knows when it was compiled.
    let now = time_now_utc();
    println!("cargo:rustc-env=DGP_BUILD_TIME={now}");

    // Force recompilation when the embedded UI dist changes.
    // rust-embed embeds demo/s3-browser/ui/dist/ at compile time, but cargo
    // doesn't track changes to non-Rust files. This tells cargo to recompile
    // whenever the dist directory is modified (e.g., after npm run build).
    //
    // NOTE: `rerun-if-changed` on a DIRECTORY only reacts to that directory's
    // own mtime (entries added/removed), NOT to in-place content changes of
    // files inside it. Hashed JS/CSS bundles get NEW filenames on every code
    // change, so dist/ churns and re-embeds — but the screenshots have STABLE
    // names (synced from docs/screenshots/ by ui/scripts/copy-screenshots.mjs).
    // Re-shooting a screenshot to the same name would NOT re-embed without
    // tracking each file. So we walk dist/ and emit a rerun line per file.
    println!("cargo:rerun-if-changed=demo/s3-browser/ui/dist");
    emit_rerun_for_tree("demo/s3-browser/ui/dist");
}

/// Recursively emit `cargo:rerun-if-changed` for every file under `root` so a
/// content change to any embedded asset (notably the stable-named screenshots)
/// triggers a re-embed. Silently no-ops if the dir doesn't exist yet (a fresh
/// checkout builds the UI first).
fn emit_rerun_for_tree(root: &str) {
    use std::fs;
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(path_str) = path.to_str() else {
            continue;
        };
        if path.is_dir() {
            emit_rerun_for_tree(path_str);
        } else {
            println!("cargo:rerun-if-changed={path_str}");
        }
    }
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
