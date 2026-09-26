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
pub mod sqlite_open;
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

    /// Lines of every `#[cfg(test)] mod NAME { ... }` body in `text`, with
    /// their 1-based line numbers. Brace counting is per line: good enough
    /// for rustfmt-formatted sources (the scan covers all of `src/`).
    fn test_module_lines(text: &str) -> Vec<(usize, &str)> {
        let lines: Vec<&str> = text.lines().collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < lines.len() {
            if lines[i].trim() != "#[cfg(test)]" {
                i += 1;
                continue;
            }
            let mut j = i + 1;
            while j < lines.len() && lines[j].trim_start().starts_with("#[") {
                j += 1;
            }
            let head = lines.get(j).map(|l| l.trim_start()).unwrap_or("");
            let head = head.strip_prefix("pub(crate) ").unwrap_or(head);
            if !(head.starts_with("mod ") && head.ends_with('{')) {
                i = j;
                continue;
            }
            let mut depth: i64 = 0;
            let mut k = j;
            while k < lines.len() {
                depth += lines[k].matches('{').count() as i64;
                depth -= lines[k].matches('}').count() as i64;
                out.push((k + 1, lines[k]));
                if depth <= 0 && k > j {
                    break;
                }
                k += 1;
            }
            i = k + 1;
        }
        out
    }

    /// Files declared as `#[cfg(test)] mod NAME;` (test module in its own file).
    fn out_of_line_test_modules(files: &[std::path::PathBuf]) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        for file in files {
            let text = std::fs::read_to_string(file).unwrap();
            let lines: Vec<&str> = text.lines().collect();
            for w in lines.windows(2) {
                let decl = w[1].trim();
                if w[0].trim() != "#[cfg(test)]"
                    || !decl.starts_with("mod ")
                    || !decl.ends_with(';')
                {
                    continue;
                }
                let name = &decl["mod ".len()..decl.len() - 1];
                let dir = if file
                    .file_name()
                    .is_some_and(|n| n == "mod.rs" || n == "lib.rs")
                {
                    file.parent().unwrap().to_path_buf()
                } else {
                    file.with_extension("")
                };
                out.push(dir.join(format!("{name}.rs")));
            }
        }
        out
    }

    /// Unit tests do not read the process environment. A test that reads it
    /// passes or fails (or skips itself) by what the runner exports: the
    /// nightly job used to set `DGP_BACKEND_ALLOW_LOCAL` for the whole job,
    /// and a lib test skipped itself there. Inject the env instead (the
    /// `*_from(env: EnvLookup)` pattern, e.g. `replay_window_from`).
    ///
    /// Allowed: files whose tests test env handling itself, serialised on a
    /// lock, plus two reads that are not config.
    #[test]
    fn test_modules_do_not_read_process_env() {
        const ALLOWED: [(&str, &str); 7] = [
            (
                "src/api/admin/auth.rs",
                "cookie-flag env parsing, under LOCK",
            ),
            (
                "src/storage/s3.rs",
                "the SSRF env override, under SSRF_ENV_LOCK",
            ),
            (
                "src/coordination/health.rs",
                "unsets the SSRF override, under SSRF_ENV_LOCK",
            ),
            (
                "src/cli/aws_creds.rs",
                "the AWS env credential chain, under ENV_LOCK",
            ),
            (
                "src/config/mod.rs",
                "${env:} expansion, under ENV_GUARD_LOCK",
            ),
            (
                "src/sqlite_open.rs",
                "child-process marker of a re-exec race test",
            ),
            (
                "src/api/admin/config/document_level.rs",
                "reads HOME to prove it is NOT expanded",
            ),
        ];
        // Built at runtime so this file's own text is no hit.
        let needles = [
            ["env::", "var("].concat(),
            ["env::", "var_os("].concat(),
            ["env::", "vars("].concat(),
            ["env_", "bool("].concat(),
            ["env_", "parse("].concat(),
            ["env_", "parse_with_default("].concat(),
            ["config::", "process_env"].concat(),
        ];
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        rust_files(&root.join("src"), &mut files);
        let test_files = out_of_line_test_modules(&files);
        assert!(
            !test_files.is_empty(),
            "scan found the out-of-line test modules"
        );
        let mut offenders = Vec::new();
        let mut allowed_hits = std::collections::BTreeSet::new();
        for file in files {
            let rel = file
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(&file).unwrap();
            // A `#[cfg(test)] mod x;` file is test code throughout.
            let lines: Vec<(usize, &str)> = if test_files.contains(&file) {
                text.lines().enumerate().map(|(i, l)| (i + 1, l)).collect()
            } else {
                test_module_lines(&text)
            };
            for (n, line) in lines {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                if needles.iter().any(|x| line.contains(x.as_str())) {
                    if ALLOWED.iter().any(|(f, _)| *f == rel) {
                        allowed_hits.insert(rel.clone());
                    } else {
                        offenders.push(format!("{rel}:{n}: {}", line.trim()));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "unit tests read the process env; inject an EnvLookup instead, or \
             add the file to ALLOWED with a reason and a lock:\n{}",
            offenders.join("\n")
        );
        // A stale allow-list entry hides the next offender in that file.
        for (f, why) in ALLOWED {
            assert!(
                allowed_hits.contains(f),
                "{f} ({why}) no longer reads env: drop it from ALLOWED"
            );
        }
    }
}
