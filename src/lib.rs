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
#[doc(hidden)]
pub mod fuzz_entry;
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

    /// A log excerpt of a string is cut with `security::str_prefix`, never
    /// with a byte-index slice ending at `s.len().min(n)`: an index inside a
    /// multi-byte char panics, and the OAuth callback died on a client-sent
    /// `code` that way.
    #[test]
    fn no_byte_index_string_excerpts() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        rust_files(&root.join("src"), &mut files);
        // Built at runtime so this test's own source is no hit.
        let needle = [".len()", ".min("].concat();
        // Byte buffers, not strings: slicing them anywhere is fine.
        const BYTES: [&str; 1] = ["hex::encode(&st.buf[.."];
        let mut offenders = Vec::new();
        for file in files {
            let text = std::fs::read_to_string(&file).unwrap();
            for (n, line) in text.lines().enumerate() {
                if line.contains("[..")
                    && line.contains(needle.as_str())
                    && !BYTES.iter().any(|b| line.contains(b))
                {
                    offenders.push(format!("{}:{}", file.display(), n + 1));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "use security::str_prefix for string excerpts:\n{}",
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
        const ALLOWED: [&str; 3] = [
            "src/config_db_sync.rs",
            "src/storage/s3.rs",
            "src/coordination/cas.rs",
        ];
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

    /// Admin handlers map a config-DB error to its status through
    /// `api::admin::db_error_status` (missing row 404, UNIQUE violation 409,
    /// the rest 500), never through a hand-picked code: `delete_user`
    /// answered 404 for any error, `update_provider` 500 for a missing row.
    /// A DB call is a `.map_err(` whose statement calls `db.<method>(`.
    #[test]
    fn admin_db_errors_map_through_db_error_status() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        rust_files(&root.join("src/api/admin"), &mut files);
        let is_db_call = |text: &str| {
            text.match_indices("db.").any(|(i, _)| {
                let before = text[..i].chars().next_back();
                let ident_start = !before.is_some_and(|c| c.is_alphanumeric() || c == '_');
                let rest = &text[i + 3..];
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                ident_start && !name.is_empty() && rest[name.len()..].starts_with('(')
            })
        };
        let mut offenders = Vec::new();
        for file in files {
            let rel = file
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(&file).unwrap();
            let lines: Vec<&str> = text.lines().collect();
            let tests_from = test_module_lines(&text).first().map(|(n, _)| *n);
            for (i, line) in lines.iter().enumerate() {
                if tests_from.is_some_and(|t| i + 1 >= t) {
                    break;
                }
                let Some(at) = line.find(".map_err(") else {
                    continue;
                };
                // The statement: back to the previous `;`, `{` or `}` line.
                let mut start = i;
                while start > 0 {
                    let prev = lines[start - 1].trim_end();
                    if prev.ends_with(';') || prev.ends_with('{') || prev.ends_with('}') {
                        break;
                    }
                    start -= 1;
                }
                let head: String = lines[start..i]
                    .iter()
                    .copied()
                    .chain(std::iter::once(&line[..at]))
                    .collect::<Vec<_>>()
                    .concat()
                    .split_whitespace()
                    .collect();
                if !is_db_call(&head) {
                    continue;
                }
                // The closure: until its parentheses balance.
                let mut depth = 0i64;
                let mut body = String::new();
                'scan: for l in &lines[i..] {
                    let from = if body.is_empty() { at } else { 0 };
                    for c in l[from..].chars() {
                        body.push(c);
                        match c {
                            '(' => depth += 1,
                            ')' => {
                                depth -= 1;
                                if depth == 0 {
                                    break 'scan;
                                }
                            }
                            _ => {}
                        }
                    }
                    body.push('\n');
                }
                if body.contains("StatusCode::") && !body.contains("db_error_status") {
                    offenders.push(format!("{rel}:{}: {}", i + 1, line.trim()));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "map config-DB errors with super::db_error_status(&e), not a fixed code:\n{}",
            offenders.join("\n")
        );
    }

    /// Is `FileMetadata {` at `idx` a struct literal (not a type position
    /// such as `-> FileMetadata {`, `impl FileMetadata {`)?
    fn is_file_metadata_literal(line: &str, idx: usize) -> bool {
        let before = line[..idx]
            .trim_end_matches(|c: char| c.is_alphanumeric() || c == '_' || c == ':')
            .trim_end();
        !["->", "impl", "struct", "for", "enum"]
            .iter()
            .any(|t| before.ends_with(t))
    }

    /// Test fixtures come from the real write path or a recorded fixture,
    /// never from a hand-built shape production does not emit: such
    /// fixtures kept C1 (open-access only), D14 (schema gate) and D17
    /// (backfill) green. So test code (src test modules and tests/) holds
    /// no `FileMetadata { .. }` struct literal (use a `FileMetadata::new_*`
    /// constructor or the engine), and integration tests do not hand-write
    /// DG metadata onto stored objects (S3 user metadata, headers, xattrs).
    ///
    /// Allowed: a test whose subject IS a foreign or corrupt shape.
    #[test]
    fn test_fixtures_come_from_the_write_path() {
        const ALLOWED: [(&str, &str); 3] = [
            (
                "tests/replication_test.rs",
                "plants a foreign writer's partial dg-* metadata on purpose",
            ),
            (
                "tests/cli_s3_purge_test.rs",
                "dg-expires-at is written only by the Python toolchain",
            ),
            (
                "tests/metadata_validation_test.rs",
                "writes corrupt xattrs to test graceful degradation",
            ),
        ];
        // Built at runtime so this file's own text is no hit.
        let hand_written_meta = [
            [".metadata(\"", "dg-"].concat(),
            ["insert(\"", "dg-"].concat(),
            [".header(\"x-amz-meta-", "dg-"].concat(),
            ["insert(\"x-amz-meta-", "dg-"].concat(),
            ["xattr::", "set("].concat(),
        ];
        let literal = ["FileMetadata", " {"].concat();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut src = Vec::new();
        rust_files(&root.join("src"), &mut src);
        let test_files = out_of_line_test_modules(&src);
        let mut integration = Vec::new();
        rust_files(&root.join("tests"), &mut integration);
        assert!(!integration.is_empty(), "scan found tests/");
        let mut offenders = Vec::new();
        let mut allowed_hits = std::collections::BTreeSet::new();
        for file in src.iter().chain(&integration) {
            let rel = file
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if rel == "src/lib.rs" {
                continue;
            }
            let is_integration = rel.starts_with("tests/");
            let text = std::fs::read_to_string(file).unwrap();
            let lines: Vec<(usize, &str)> = if is_integration || test_files.contains(file) {
                text.lines().enumerate().map(|(i, l)| (i + 1, l)).collect()
            } else {
                test_module_lines(&text)
            };
            for (n, line) in lines {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                let struct_literal = line
                    .match_indices(literal.as_str())
                    .any(|(i, _)| is_file_metadata_literal(line, i));
                let meta_write =
                    is_integration && hand_written_meta.iter().any(|x| line.contains(x.as_str()));
                if !(struct_literal || meta_write) {
                    continue;
                }
                if ALLOWED.iter().any(|(f, _)| *f == rel) {
                    allowed_hits.insert(rel.clone());
                } else {
                    offenders.push(format!("{rel}:{n}: {}", line.trim()));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "a test builds a fixture shape by hand; store it through the proxy \
             (or use a FileMetadata constructor), or add the file to ALLOWED \
             with a reason:\n{}",
            offenders.join("\n")
        );
        for (f, why) in ALLOWED {
            assert!(
                allowed_hits.contains(f),
                "{f} ({why}) no longer hand-builds metadata: drop it from ALLOWED"
            );
        }
    }

    #[test]
    fn file_metadata_literal_detection() {
        let lit = |l: &str| {
            let i = l.find("FileMetadata {").unwrap();
            is_file_metadata_literal(l, i)
        };
        assert!(lit("        FileMetadata {"));
        assert!(lit("    let m = crate::types::FileMetadata {"));
        assert!(lit("        Ok(FileMetadata {"));
        assert!(!lit("    fn meta() -> FileMetadata {"));
        assert!(!lit("    fn meta() -> crate::types::FileMetadata {"));
        assert!(!lit("impl FileMetadata {"));
        assert!(!lit("pub struct FileMetadata {"));
        assert!(!lit("impl Default for FileMetadata {"));
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
