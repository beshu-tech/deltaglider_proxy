// SPDX-License-Identifier: BUSL-1.1

//! THE way to open a SQLite connection in this crate.
//!
//! SQLCipher's `sqlite3_initialize` marks SQLite initialized BEFORE it runs
//! SQLCipher's own init (`sqlcipher_extra_init`, outside the init mutex). A
//! second thread that opens its first connection in that gap sees "already
//! initialized" and fails `PRAGMA key` with "sqlcipher not initialized". The
//! first connection of a process is where this bites: at boot, and in the
//! test harness, where many threads open their first DB at once. Every open
//! goes through [`open`] / [`open_in_memory`], which run the first
//! initialization to its end exactly once before any connection opens.
//! `sqlite_opens_go_through_the_init_gate` guards the call sites.

use rusqlite::Connection;
use std::path::Path;

fn initialized() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        // SAFETY: sqlite3_initialize takes no arguments and is safe to call
        // at any time; it returns an error code we cannot act on here (the
        // open below reports the same failure).
        let _ = unsafe { rusqlite::ffi::sqlite3_initialize() };
    });
}

pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    initialized();
    Connection::open(path)
}

pub fn open_in_memory() -> rusqlite::Result<Connection> {
    initialized();
    Connection::open_in_memory()
}

#[cfg(test)]
mod tests {
    /// Child mode of the race test: many threads open and key their first
    /// connection at the same instant in a fresh process.
    const CHILD_ENV: &str = "DGP_TEST_SQLCIPHER_FIRST_OPEN_CHILD";

    #[test]
    fn first_opens_in_a_fresh_process_all_succeed() {
        if std::env::var_os(CHILD_ENV).is_some() {
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(32));
            let threads: Vec<_> = (0..32)
                .map(|_| {
                    let b = barrier.clone();
                    std::thread::spawn(move || {
                        b.wait();
                        let c = super::open_in_memory().unwrap();
                        c.pragma_update(None, "key", "pw")
                    })
                })
                .collect();
            for t in threads {
                t.join().unwrap().expect("PRAGMA key on a first connection");
            }
            return;
        }
        let exe = std::env::current_exe().unwrap();
        for _ in 0..40 {
            let out = std::process::Command::new(&exe)
                .args([
                    "sqlite_open::tests::first_opens_in_a_fresh_process_all_succeed",
                    "--exact",
                    "--test-threads=1",
                ])
                .env(CHILD_ENV, "1")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stdout)
            );
        }
    }

    /// Every connection opens through the gate above.
    #[test]
    fn sqlite_opens_go_through_the_init_gate() {
        fn walk(dir: &std::path::Path, out: &mut Vec<String>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") && !p.ends_with("sqlite_open.rs")
                {
                    let text = std::fs::read_to_string(&p).unwrap();
                    for (i, l) in text.lines().enumerate() {
                        if l.contains(concat!("Connection", "::open")) {
                            out.push(format!("{}:{}", p.display(), i + 1));
                        }
                    }
                }
            }
        }
        let mut hits = Vec::new();
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut hits,
        );
        assert!(hits.is_empty(), "open through crate::sqlite_open: {hits:?}");
    }
}
