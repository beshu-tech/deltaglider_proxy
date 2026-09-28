// SPDX-License-Identifier: BUSL-1.1

//! `config_file_writable`: can a persist (`atomic_write`) save the config?

use crate::config::config_file_writable;

fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

#[test]
fn a_writable_dir_is_writable_with_or_without_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("deltaglider_proxy.yaml");
    assert!(config_file_writable(&path), "absent file, writable dir");
    std::fs::write(&path, "{}").unwrap();
    assert!(config_file_writable(&path), "present file, writable dir");
}

#[test]
fn a_missing_dir_is_not_writable() {
    let dir = tempfile::tempdir().unwrap();
    assert!(!config_file_writable(&dir.path().join("nope/cfg.yaml")));
}

#[cfg(unix)]
#[test]
fn a_read_only_dir_is_not_writable() {
    use std::os::unix::fs::PermissionsExt;
    if is_root() {
        return; // root ignores the mode bits; a read-only mount still counts.
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cfg.yaml");
    std::fs::write(&path, "{}").unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    let writable = config_file_writable(&path);
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!writable, "the sibling tempfile cannot be created");
}
