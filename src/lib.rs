// SPDX-License-Identifier: BUSL-1.1

//! DeltaGlider Proxy - S3-compatible object storage with DeltaGlider deduplication
//!
//! This library provides the core functionality for the DeltaGlider Proxy S3 server.

pub mod admission;
pub mod api;
pub mod audit;
pub(crate) mod background;
pub mod bucket_policy;
pub mod bucket_usage;
pub mod cli;
pub mod config;
pub mod config_apply;
pub mod config_db;
pub mod config_db_sync;
pub mod config_sections;
pub mod coordination;
pub mod cors;
pub mod deltaglider;
pub mod event_delivery;
pub mod event_outbox;
pub mod iam;
pub mod init;
pub mod job_loop;
pub mod lifecycle;
pub mod logs;
pub mod maintenance;
pub mod metadata_cache;
pub mod metrics;
pub mod multipart;
pub mod rate_limiter;
pub mod replication;
pub mod s3_adapter_s3s;
pub mod security;
pub mod session;
pub mod slack_format;
pub mod storage;
pub mod tls;
pub(crate) mod transfer;
pub mod transfer_plan;
pub mod types;
pub mod usage_scanner;

/// Source guards: rules that a unit test cannot express per call site.
#[cfg(test)]
mod source_guards {
    use std::path::Path;

    fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// Every object HEAD the server sends is counted in
    /// `deltaglider_backend_head_requests_total`, so the counter can prove
    /// that a path (a client LIST, the folder-size scan) sends none. The CLI
    /// runs in its own process and has no metrics endpoint.
    #[test]
    fn every_server_head_request_is_counted() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        rust_files(&root.join("src"), &mut files);
        let mut offenders = Vec::new();
        for file in files {
            let rel = file
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if rel.starts_with("src/cli/") || rel == "src/lib.rs" {
                continue;
            }
            let text = std::fs::read_to_string(&file).unwrap();
            let lines: Vec<&str> = text.lines().collect();
            for (n, line) in lines.iter().enumerate() {
                if !line.contains(".head_object()") || line.trim_start().starts_with("//") {
                    continue;
                }
                let window = &lines[n.saturating_sub(8)..n];
                if !window
                    .iter()
                    .any(|l| l.contains("BACKEND_HEAD_REQUESTS.inc()"))
                {
                    offenders.push(format!("{rel}:{}", n + 1));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "increment BACKEND_HEAD_REQUESTS right before each head_object():\n{}",
            offenders.join("\n")
        );
    }

    /// Errors are classified on their type, status, or code, never on their
    /// Display text. `SdkError`'s Display is only "service error" (so a 404
    /// check on it never matches), and a typed engine error's text drifts
    /// from any hand-written pattern. Only the pure classifiers, which read
    /// the structured SDK signal, may match on these tokens.
    #[test]
    fn no_error_classification_on_display_text() {
        const ALLOWED: [&str; 2] = ["src/config_db_sync.rs", "src/storage/s3.rs"];
        const TOKENS: [&str; 16] = [
            "NoSuchKey",
            "NoSuchBucket",
            "NoSuchUpload",
            "NotFound",
            "Not Found",
            "not found",
            "nosuchkey",
            "404",
            "403",
            "412",
            "501",
            "PreconditionFailed",
            "NotImplemented",
            "ChecksumMismatch",
            "InvalidAccessKeyId",
            "SignatureDoesNotMatch",
        ];
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        rust_files(&root.join("src"), &mut files);
        let mut offenders = Vec::new();
        for file in files {
            let rel = file
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if ALLOWED.contains(&rel.as_str()) {
                continue;
            }
            let text = std::fs::read_to_string(&file).unwrap();
            for (n, line) in text.lines().enumerate() {
                let hit = TOKENS
                    .iter()
                    .any(|t| line.contains(&format!(".contains(\"{t}\")")));
                if hit && !line.contains("assert") && !line.contains("TOKENS") {
                    offenders.push(format!("{rel}:{}: {}", n + 1, line.trim()));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "classify errors on their type/status/code, not their text:\n{}",
            offenders.join("\n")
        );
    }
}
