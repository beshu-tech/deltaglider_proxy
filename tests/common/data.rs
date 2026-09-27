// SPDX-License-Identifier: BUSL-1.1

//! Test data: deterministic generators and the filesystem xattr reader.

use rand::{Rng, SeedableRng};

// === Data generators ===

/// Generate deterministic binary data
pub fn generate_binary(size: usize, seed: u64) -> Vec<u8> {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let mut data = vec![0u8; size];
    rng.fill(&mut data[..]);
    data
}

/// Deterministic pseudo-random, incompressible body (xorshift). Stored
/// passthrough (not delta-eligible) and large enough to span multiple
/// multipart parts. Shared by `streaming_copy_test` and
/// `large_object_e2e_test`.
pub fn big_passthrough_body(len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    let mut x: u64 = 0x1234_5678_9abc_def0;
    while v.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.truncate(len);
    v
}

/// Mutate binary data by changing a percentage of bytes
pub fn mutate_binary(data: &[u8], change_ratio: f64) -> Vec<u8> {
    let mut result = data.to_vec();
    let changes = (data.len() as f64 * change_ratio) as usize;
    let mut rng = rand::thread_rng();

    for _ in 0..changes {
        let idx = rng.gen_range(0..result.len());
        result[idx] = rng.gen();
    }

    result
}

/// Walk a filesystem-backend data directory and return every file's
/// `user.dg.metadata` xattr parsed as JSON. The key matches
/// `src/storage/xattr_meta.rs::XATTR_NAME` — tests depend on the
/// concrete name rather than the constant so they also catch the
/// case where the constant is accidentally renamed without a test
/// update.
///
/// Files without a `user.dg.metadata` xattr are skipped silently
/// (directories, partial writes, CAS-style staged blobs, etc.).
/// Shared with the encryption integration suite so we don't have N
/// copies of the same walkdir + xattr::get + serde_json::parse
/// ladder.
#[cfg(unix)]
pub fn read_xattr_metadata(
    data_dir: &std::path::Path,
) -> Vec<(std::path::PathBuf, serde_json::Value)> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(data_dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let bytes = match xattr::get(entry.path(), "user.dg.metadata") {
            Ok(Some(b)) => b,
            _ => continue,
        };
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            out.push((entry.path().to_path_buf(), v));
        }
    }
    out
}
