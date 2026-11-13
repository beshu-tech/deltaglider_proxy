// SPDX-License-Identifier: BUSL-1.1

//! DeltaGlider Proxy - S3-compatible object storage with DeltaGlider deduplication
//!
//! This library provides the core functionality for the DeltaGlider Proxy S3 server.

// clippy.toml bans calls that production code makes in one place only (the
// sqlite gate, the spool, the SSRF-checked endpoint, ...). Tests may make
// them: a test temp dir is not a scratch file of the proxy.
#![cfg_attr(test, allow(clippy::disallowed_methods))]

pub mod admission;
pub mod api;
pub mod audit;
pub mod background;
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
pub mod data_dir_lock;
pub mod deltaglider;
pub mod disk_watch;
pub mod event_delivery;
pub mod event_outbox;
#[doc(hidden)]
pub mod fuzz_entry;
pub mod glob_filter;
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
pub mod serve;
pub mod session;
pub mod shutdown;
pub(crate) mod single_flight;
pub mod slack_format;
pub mod sqlite_open;
pub mod storage;
pub mod tls;
pub(crate) mod transfer;
pub mod transfer_plan;
pub mod types;
pub mod usage_scanner;
pub mod zip_stream;

#[cfg(test)]
mod test_root;

/// THE source scanner that every source guard uses: one file walker and one
/// definition of "production lines". A guard that cut a file at its first
/// `#[cfg(test)]` stopped at the first test-only ITEM (a `#[cfg(test)] fn`)
/// and never saw the production code after it.
#[cfg(test)]
pub(crate) mod source_scan {
    use std::path::{Path, PathBuf};

    /// The `syn` view of the sources (`src/source_scan/ast.rs`).
    pub(crate) mod ast;

    /// The crate root (`CARGO_MANIFEST_DIR`).
    pub(crate) fn root() -> &'static Path {
        Path::new(env!("CARGO_MANIFEST_DIR"))
    }

    /// Every `.rs` file under `dir` (relative to the crate root), sorted.
    pub(crate) fn rust_files(dir: &str) -> Vec<PathBuf> {
        files(dir, "rs")
    }

    /// Every file with extension `ext` under `dir` (relative to the crate
    /// root), sorted. THE walker: no guard keeps its own.
    pub(crate) fn files(dir: &str, ext: &str) -> Vec<PathBuf> {
        fn walk(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, ext, out);
                } else if path.extension().is_some_and(|e| e == ext) {
                    out.push(path);
                }
            }
        }
        let mut out = Vec::new();
        walk(&root().join(dir), ext, &mut out);
        out.sort();
        out
    }

    /// The text of `path` (relative to the crate root).
    pub(crate) fn read(path: &str) -> String {
        std::fs::read_to_string(root().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    /// `path` relative to the crate root, with `/` separators.
    pub(crate) fn rel(path: &Path) -> String {
        path.strip_prefix(root())
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    /// Lines of every `#[cfg(test)] mod NAME { ... }` body in `text`, with
    /// their 1-based line numbers. Brace counting is per line: good enough
    /// for rustfmt-formatted sources (the scan covers all of `src/`).
    pub(crate) fn test_module_lines(text: &str) -> Vec<(usize, &str)> {
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

    /// `(name, text)` of the function that holds line `at` (0-based) of
    /// `text`: the nearest `fn` header above it, to the closing brace at the
    /// header's indentation. Text-based, for rustfmt-formatted sources.
    pub(crate) fn enclosing_fn(text: &str, at: usize) -> (String, String) {
        let lines: Vec<&str> = text.lines().collect();
        let header = |l: &str| -> Option<String> {
            let l = l.trim_start();
            let l = l
                .strip_prefix("pub(crate) ")
                .or(l.strip_prefix("pub(super) "))
                .or(l.strip_prefix("pub "))
                .unwrap_or(l);
            let l = l.strip_prefix("async ").unwrap_or(l);
            let rest = l.strip_prefix("fn ")?;
            Some(rest.split(['(', '<']).next()?.to_string())
        };
        let Some(start) = (0..=at.min(lines.len().saturating_sub(1)))
            .rev()
            .find(|i| header(lines[*i]).is_some())
        else {
            return (String::new(), String::new());
        };
        let indent = lines[start].len() - lines[start].trim_start().len();
        // (No bare brace in a literal: the production-line mask counts them.)
        let close = format!("{}{}", " ".repeat(indent), '\u{7d}');
        let end = lines[start..]
            .iter()
            .position(|l| l.starts_with(&close))
            .map_or(lines.len() - 1, |p| start + p);
        (
            header(lines[start]).unwrap_or_default(),
            lines[start..=end].join("\n"),
        )
    }

    /// The text of `fn name` in `text` (see [`enclosing_fn`]).
    pub(crate) fn fn_body(text: &str, name: &str) -> String {
        let at = text
            .lines()
            .position(|l| l.contains(&format!("fn {name}(")))
            .unwrap_or_else(|| panic!("fn {name} exists"));
        enclosing_fn(text, at).1
    }

    /// `mask[i]` is true when line `i` (0-based) of `text` is production
    /// code: outside every `#[cfg(test)] mod` body. A test-only item
    /// (`#[cfg(test)] fn`) counts as production: the scan stays strict.
    pub(crate) fn prod_mask(text: &str) -> Vec<bool> {
        let mut mask = vec![true; text.lines().count()];
        for (n, _) in test_module_lines(text) {
            mask[n - 1] = false;
        }
        mask
    }

    /// Production lines of `text`, with 1-based line numbers.
    pub(crate) fn prod_lines(text: &str) -> Vec<(usize, &str)> {
        let mask = prod_mask(text);
        text.lines()
            .enumerate()
            .filter(|(i, _)| mask[*i])
            .map(|(i, l)| (i + 1, l))
            .collect()
    }

    /// The text of production function `name` in `file` (from its `fn`
    /// line to the next `fn` line).
    pub(crate) fn prod_fn_text(file: &str, name: &str) -> String {
        let text = prod_text(&read(file));
        let start = text
            .find(&format!("fn {name}("))
            .or_else(|| text.find(&format!("fn {name}<")))
            .unwrap_or_else(|| panic!("{file}: no fn {name}"));
        let rest = &text[start + 3..];
        let end = ["\nfn ", " fn "]
            .iter()
            .filter_map(|p| rest.find(p))
            .min()
            .unwrap_or(rest.len());
        rest[..end].to_string()
    }

    /// Production code of `text` as one string (for multi-line matches).
    pub(crate) fn prod_text(text: &str) -> String {
        prod_lines(text)
            .into_iter()
            .map(|(_, l)| l)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Files declared as `#[cfg(test)] mod NAME;` (test module in its own
    /// file), also with a `#[path = "FILE.rs"]` line between the two.
    pub(crate) fn out_of_line_test_modules(files: &[PathBuf]) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for file in files {
            let text = std::fs::read_to_string(file).unwrap();
            let lines: Vec<&str> = text.lines().collect();
            for (i, w) in lines.windows(2).enumerate() {
                if let Some(path) = w[1]
                    .trim()
                    .strip_prefix("#[path = \"")
                    .and_then(|p| p.strip_suffix("\"]"))
                {
                    if w[0].trim() == "#[cfg(test)]"
                        && lines.get(i + 2).is_some_and(|l| l.trim().ends_with(';'))
                    {
                        out.push(file.parent().unwrap().join(path));
                    }
                    continue;
                }
                let decl = w[1].trim();
                let decl = decl.strip_prefix("pub(crate) ").unwrap_or(decl);
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

    /// `(rel path, text)` of every production source file under `dir`: the
    /// out-of-line test modules (`#[cfg(test)] mod x;`) are left out.
    pub(crate) fn prod_sources(dir: &str) -> Vec<(String, String)> {
        let tests = out_of_line_test_modules(&rust_files("src"));
        rust_files(dir)
            .into_iter()
            .filter(|f| !tests.contains(f))
            .map(|f| (rel(&f), std::fs::read_to_string(&f).unwrap()))
            .collect()
    }

    #[test]
    fn prod_lines_skip_test_modules_only() {
        let src = "fn a() {}\n#[cfg(test)]\nfn only_in_tests() {}\nfn b() {}\n\
                   #[cfg(test)]\nmod tests {\n    fn t() { x() }\n}\nfn c() {}\n";
        let prod: Vec<&str> = prod_lines(src).into_iter().map(|(_, l)| l).collect();
        assert_eq!(
            prod,
            [
                "fn a() {}",
                "#[cfg(test)]",
                "fn only_in_tests() {}",
                "fn b() {}",
                "#[cfg(test)]",
                "fn c() {}"
            ],
            "code after a test-only item is production; a test module is not"
        );
        assert_eq!(prod_lines(src)[5].0, 9, "line numbers are the file's");
        assert!(prod_text(src).contains("fn b() {}\n#[cfg(test)]\nfn c() {}"));
    }

    #[test]
    fn scan_finds_sources_and_out_of_line_test_files() {
        let files = rust_files("src");
        assert!(files.len() > 100, "scan found the sources");
        let tests = out_of_line_test_modules(&files);
        assert!(!tests.is_empty(), "found the out-of-line test modules");
        assert!(
            tests
                .iter()
                .any(|t| rel(t) == "src/s3_adapter_s3s/copy_cost_tests.rs"),
            "a `#[path]` test module counts"
        );
        assert!(prod_sources("src")
            .iter()
            .all(|(rel, _)| { !tests.iter().any(|t| crate::source_scan::rel(t) == *rel) }));
    }
}

/// The served section schemas (`GET /config/defaults?section=`, `config
/// schema`, the admin YAML editor) must accept the YAML the loader accepts.
/// A small draft-07 checker (the subset schemars emits) runs the product
/// docs' `# validate` blocks, the prod-shape fixture and the example's
/// lifecycle block against them.
#[cfg(test)]
mod schema_conformance {
    use serde_json::Value;

    fn resolve<'a>(root: &'a Value, s: &'a Value) -> &'a Value {
        match s.get("$ref").and_then(Value::as_str) {
            Some(r) => {
                let name = r.trim_start_matches("#/definitions/");
                resolve(root, &root["definitions"][name])
            }
            None => s,
        }
    }

    fn type_ok(t: &str, v: &Value) -> bool {
        match t {
            "null" => v.is_null(),
            "boolean" => v.is_boolean(),
            "string" => v.is_string(),
            "integer" => v.is_i64() || v.is_u64(),
            "number" => v.is_number(),
            "array" => v.is_array(),
            "object" => v.is_object(),
            _ => true,
        }
    }

    /// Errors of `v` against `schema` (empty = valid).
    pub(crate) fn check(root: &Value, schema: &Value, v: &Value, path: &str) -> Vec<String> {
        let s = resolve(root, schema);
        if s == &Value::Bool(true) {
            return vec![];
        }
        let mut errs = Vec::new();
        if let Some(t) = s.get("type") {
            let ok = match t {
                Value::String(t) => type_ok(t, v),
                Value::Array(ts) => ts.iter().filter_map(Value::as_str).any(|t| type_ok(t, v)),
                _ => true,
            };
            if !ok {
                return vec![format!("{path}: {v} is not of type {t}")];
            }
        }
        if let Some(Value::Array(allowed)) = s.get("enum") {
            if !allowed.contains(v) {
                errs.push(format!("{path}: {v} is not one of {allowed:?}"));
            }
        }
        if let (Some(p), Some(text)) = (s.get("pattern").and_then(Value::as_str), v.as_str()) {
            if !regex_lite::Regex::new(p).unwrap().is_match(text) {
                errs.push(format!("{path}: {v} does not match {p}"));
            }
        }
        if let Some(c) = s.get("const") {
            if c != v {
                errs.push(format!("{path}: {v} is not {c}"));
            }
        }
        for key in ["anyOf", "oneOf"] {
            if let Some(Value::Array(alts)) = s.get(key) {
                let passing = alts
                    .iter()
                    .filter(|a| check(root, a, v, path).is_empty())
                    .count();
                if passing == 0 || (key == "oneOf" && passing > 1) {
                    errs.push(format!(
                        "{path}: {v} matches {passing} of the {key} branches"
                    ));
                }
            }
        }
        if let Some(Value::Array(all)) = s.get("allOf") {
            for a in all {
                errs.extend(check(root, a, v, path));
            }
        }
        if let Value::Object(map) = v {
            let props = s.get("properties").and_then(Value::as_object);
            if let Some(Value::Array(req)) = s.get("required") {
                for r in req.iter().filter_map(Value::as_str) {
                    if !map.contains_key(r) {
                        errs.push(format!("{path}: missing required `{r}`"));
                    }
                }
            }
            for (k, val) in map {
                let sub = format!("{path}.{k}");
                match props.and_then(|p| p.get(k)) {
                    Some(ps) => errs.extend(check(root, ps, val, &sub)),
                    None => match s.get("additionalProperties") {
                        Some(Value::Bool(false)) => errs.push(format!("{sub}: unknown field")),
                        Some(ap @ Value::Object(_)) => errs.extend(check(root, ap, val, &sub)),
                        _ => {}
                    },
                }
            }
        }
        if let (Value::Array(items), Some(is)) = (v, s.get("items")) {
            for (i, item) in items.iter().enumerate() {
                errs.extend(check(root, is, item, &format!("{path}[{i}]")));
            }
        }
        errs
    }

    fn section_schema(name: &str) -> Value {
        crate::cli::config::section_schema(name).unwrap_or_else(|| panic!("not a section: {name}"))
    }

    /// Errors of a sectioned YAML document against the section schemas.
    fn check_document(label: &str, yaml: &str) -> Vec<String> {
        let doc: Value = serde_yaml::from_str(yaml).unwrap_or_else(|e| panic!("{label}: {e}"));
        let Value::Object(map) = doc else {
            return vec![];
        };
        let mut errs = Vec::new();
        for (section, body) in &map {
            let schema = section_schema(section);
            errs.extend(check(
                &schema,
                &schema,
                body,
                &format!("{label}: {section}"),
            ));
        }
        errs
    }

    /// `# validate` blocks of `docs/product` (what `check-docs-yaml-examples.sh`
    /// lints), as `(file:line, yaml)`.
    fn docs_validate_blocks() -> Vec<(String, String)> {
        let mut out = Vec::new();
        for f in crate::source_scan::files("docs/product", "md") {
            let text = std::fs::read_to_string(&f).unwrap();
            let lines: Vec<&str> = text.lines().collect();
            let mut i = 0;
            while i + 1 < lines.len() {
                let fence = lines[i];
                let indent = fence.len() - fence.trim_start().len();
                if fence.trim() == "```yaml" && lines[i + 1].trim() == "# validate" {
                    let mut body = String::new();
                    let mut j = i + 2;
                    while j < lines.len() && !lines[j].trim_start().starts_with("```") {
                        body.push_str(lines[j].get(indent..).unwrap_or(""));
                        body.push('\n');
                        j += 1;
                    }
                    out.push((format!("{}:{}", crate::source_scan::rel(&f), i + 1), body));
                    i = j;
                }
                i += 1;
            }
        }
        out
    }

    #[test]
    fn served_schemas_accept_the_documented_yaml() {
        let blocks = docs_validate_blocks();
        assert!(
            blocks.len() > 10,
            "found the docs blocks ({})",
            blocks.len()
        );
        let mut errs = Vec::new();
        for (label, yaml) in &blocks {
            errs.extend(check_document(label, yaml));
        }
        let fixture = std::fs::read_to_string(
            crate::source_scan::root().join("tests/fixtures/prod_shape_config.yaml"),
        )
        .unwrap();
        errs.extend(check_document("prod_shape_config.yaml", &fixture));
        assert!(
            errs.is_empty(),
            "schema refuses valid YAML:\n{}",
            errs.join("\n")
        );
    }

    /// The loader types a whole-value `${env:NAME}` by its field
    /// (`config::lenient`), so the editor schema must accept one in a
    /// bool, an integer and an enum field, and still refuse plain text.
    #[test]
    fn served_schema_accepts_env_refs_in_non_string_scalars() {
        let doc = "storage:
  backends:
    - name: hetzner-fsn1
      type: s3
      force_path_style: ${env:S3_PATH_STYLE:-false}
  default_backend: hetzner-fsn1
  buckets:
    releases:
      compression: ${env:COMPRESS}
access:
  iam_mode: ${env:IAM_MODE:-gui}
advanced:
  max_object_size: ${env:MAX_OBJECT_SIZE}
  max_delta_ratio: ${env:RATIO:-0.5}
";
        let errs = check_document("env refs", doc);
        assert!(
            errs.is_empty(),
            "schema refuses env refs:\n{}",
            errs.join("\n")
        );
        for bad in [
            "storage:\n  buckets:\n    releases:\n      compression: maybe\n",
            "access:\n  iam_mode: sometimes\n",
            "advanced:\n  max_object_size: big\n",
            "advanced:\n  max_object_size: ${env:SIZE}0\n",
            "advanced:\n  max_object_size: ${iam:username}\n",
        ] {
            assert!(!check_document("bad", bad).is_empty(), "accepted: {bad}");
        }
    }

    /// The example's commented `lifecycle:` block (every action shape).
    #[test]
    fn served_schema_accepts_the_example_lifecycle_block() {
        let example = std::fs::read_to_string(
            crate::source_scan::root().join("deltaglider_proxy.example.yaml"),
        )
        .unwrap();
        let mut body = String::from("storage:\n");
        let mut inside = false;
        for line in example.lines() {
            if line == "  # lifecycle:" {
                inside = true;
            } else if inside && !(line == "  #" || line.starts_with("  #   ")) {
                break;
            }
            if inside {
                body.push_str("  ");
                body.push_str(line.strip_prefix("  # ").unwrap_or(""));
                body.push('\n');
            }
        }
        assert!(body.contains("retain-newest"), "found the block:\n{body}");
        // The loader accepts it ...
        let parsed: crate::config_sections::StorageSection =
            serde_yaml::from_str(body.trim_start_matches("storage:\n")).unwrap();
        assert_eq!(parsed.lifecycle.rules.len(), 3);
        // ... and so must the schema.
        let errs = check_document("example lifecycle", &body);
        assert!(
            errs.is_empty(),
            "schema refuses valid YAML:\n{}",
            errs.join("\n")
        );
    }

    /// Every value a custom-serde type SERIALIZES to (what the canonical
    /// export writes and the editor then shows) validates.
    #[test]
    fn served_schema_accepts_every_lifecycle_action_form() {
        use crate::config_sections::*;
        let actions = [
            LifecycleAction::Delete,
            LifecycleAction::Transition(LifecycleTransitionAction {
                destination: LifecycleDestination {
                    bucket: "releases".into(),
                    prefix: "cold/".into(),
                },
                delete_source_after_success: true,
            }),
            LifecycleAction::RetainNewest(LifecycleRetainNewestAction {
                count: 3,
                qualify: LifecycleQualifySpec {
                    min_size_bytes: Some(1),
                    min_age: Some("1h".into()),
                },
                protect_younger_than: Some("1d".into()),
            }),
        ];
        let schema = serde_json::to_value(schemars::schema_for!(LifecycleAction)).unwrap();
        for a in actions {
            let v = serde_json::to_value(&a).unwrap();
            let errs = check(&schema, &schema, &v, "action");
            assert!(errs.is_empty(), "{v}: {errs:?}");
        }
        // And the schema still refuses what the loader refuses.
        for bad in [
            serde_json::json!("archive-please"),
            serde_json::json!({"type": "transition"}),
            serde_json::json!({"type": "retain-newest"}),
        ] {
            assert!(
                !check(&schema, &schema, &bad, "action").is_empty(),
                "{bad} passes"
            );
        }
    }

    /// `lenient::bool_or_string` fields load `"true"` / `"false"` strings
    /// too (an `${env:…}` value arrives as one); the schema says so.
    #[test]
    fn served_schema_accepts_string_bools_the_loader_accepts() {
        let yaml = r#"
storage:
  backends:
    - name: aws-dr
      type: s3
      region: eu-central-1
      force_path_style: "true"
      allow_local: "false"
      encryption:
        mode: sse-kms
        kms_key_id: alias/dgp
        bucket_key_enabled: "false"
"#;
        let doc: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        let _: crate::config_sections::StorageSection =
            serde_yaml::from_value(doc["storage"].clone()).expect("the loader accepts it");
        let errs = check_document("string bools", yaml);
        assert!(
            errs.is_empty(),
            "schema refuses valid YAML:\n{}",
            errs.join("\n")
        );
        let bad = yaml.replace("\"false\"\n      encryption", "\"nope\"\n      encryption");
        assert!(
            !check_document("bad bool", &bad).is_empty(),
            "a non-bool string passes"
        );
    }

    /// The class guard: a type with a hand-written `Deserialize` (or a
    /// field with a custom `deserialize_with`) accepts a wire form that a
    /// derived schema does not describe, so it carries its own schema.
    #[test]
    fn custom_deserializers_carry_their_own_schema() {
        let sources = crate::source_scan::prod_sources("src");
        let all: String = sources.iter().map(|(_, t)| t.as_str()).collect();
        let mut offenders = Vec::new();
        for (rel, text) in &sources {
            let prod = crate::source_scan::prod_text(text);
            for part in prod.split("impl<'de> Deserialize<'de> for ").skip(1) {
                let ty: String = part
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                let derived = prod.contains(&format!("JsonSchema)]\npub enum {ty} "))
                    || prod.contains(&format!("JsonSchema)]\npub struct {ty} "));
                // `#[serde(remote = "Self")]`: the derived deserializer is the
                // inherent fn that the trait impl calls after a check that
                // only refuses input, so the derived schema describes it.
                let own = all.contains(&format!("JsonSchema for {ty} "))
                    || prod.contains(&format!("#[schemars(rename = \"{ty}\")]\npub enum {ty} "))
                        && prod.contains("remote = \"Self\"");
                if derived || !own {
                    offenders.push(format!("{rel}: {ty}"));
                }
            }
            let custom = prod
                .matches("deserialize_with = \"lenient::bool_or_string\"")
                .count();
            let schema = prod
                .matches("schema_with = \"lenient::bool_or_string_schema\"")
                .count();
            if custom != schema {
                offenders.push(format!(
                    "{rel}: {custom} bool_or_string fields, {schema} schemas"
                ));
            }
        }
        assert!(
            offenders.is_empty(),
            "a custom deserializer without a matching schema: {offenders:?}"
        );
    }

    #[test]
    fn checker_truth_table() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["a"],
            "additionalProperties": false,
            "properties": {
                "a": {"type": "integer"},
                "b": {"oneOf": [{"type": "string", "enum": ["x"]}, {"$ref": "#/definitions/D"}]}
            },
            "definitions": {"D": {"type": "object", "properties": {"k": {"type": "boolean"}}}}
        });
        let ok = |v: Value| check(&schema, &schema, &v, "$").is_empty();
        assert!(ok(serde_json::json!({"a": 1})));
        assert!(ok(serde_json::json!({"a": 1, "b": "x"})));
        assert!(ok(serde_json::json!({"a": 1, "b": {"k": true}})));
        assert!(!ok(serde_json::json!({})), "required");
        assert!(!ok(serde_json::json!({"a": "1"})), "type");
        assert!(!ok(serde_json::json!({"a": 1, "c": 1})), "unknown field");
        assert!(!ok(serde_json::json!({"a": 1, "b": "y"})), "enum");
        assert!(!ok(serde_json::json!({"a": 1, "b": {"k": 1}})), "ref");
    }
}

/// Source guards: rules that a unit test cannot express per call site.
#[cfg(test)]
mod source_guards {
    use crate::source_scan::{
        out_of_line_test_modules, prod_mask, rel as rel_path, root, rust_files, test_module_lines,
    };

    /// F13 (#145): everything that can stop a boot for an environment reason
    /// (the state-directory lock, the spool, the listener, TLS, the bootstrap
    /// hash, the xdelta3 CLI) is made in ONE function,
    /// `startup::claim_environment`. That call is the first `.await` of
    /// `async_main`, and nothing before it spawns, so a boot that cannot
    /// serve never ran a job or wrote to a backend. A step that moves above
    /// it (a coordination probe, a backend write) turns this red.
    #[test]
    fn the_environment_is_claimed_before_the_first_worker_spawns() {
        let main = include_str!("main.rs");
        let body = &main[main.find("async fn async_main").expect("async_main")..];
        let body = &body[..body.find("\nfn ").unwrap_or(body.len())];
        // Code lines only: comments name the steps too.
        let code: Vec<&str> = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect();
        let first_await = code
            .iter()
            .position(|l| l.contains(".await"))
            .expect("async_main awaits");
        assert!(
            code[first_await].contains("startup::claim_environment("),
            "the first `.await` of async_main must be startup::claim_environment, \
             found: {}",
            code[first_await].trim()
        );
        let first_spawn = code
            .iter()
            .position(|l| l.contains("spawn"))
            .expect("async_main spawns workers");
        assert!(
            first_spawn > first_await,
            "nothing in async_main may spawn before startup::claim_environment: {}",
            code[first_spawn].trim()
        );
        // The function holds every step, in an order that makes the lock first.
        let startup = include_str!("startup.rs");
        let f = &startup[startup
            .find("pub async fn claim_environment")
            .expect("claim_environment")..];
        let f = &f[..f.find("\n}\n").expect("end of claim_environment")];
        let mut at = 0;
        for step in [
            "data_dir_lock::acquire(",
            "SpoolDir::claim_shared(",
            "bind_listener(",
            "init_tls(",
            "ensure_bootstrap_password_hash()",
            "is_cli_available()",
        ] {
            let pos = f
                .find(step)
                .unwrap_or_else(|| panic!("claim_environment no longer calls {step}"));
            assert!(pos >= at, "{step} is out of order in claim_environment");
            at = pos;
        }
        // The spawn inside is the blocking lock wait, awaited at once.
        assert_eq!(
            f.matches("spawn").count(),
            1,
            "claim_environment starts no worker"
        );
        for outside in ["data_dir_lock::acquire(", "bind_listener(", "init_tls("] {
            assert!(
                !body.contains(outside),
                "async_main calls {outside} itself, not through claim_environment"
            );
        }
        assert!(
            !main.contains("TcpListener::bind"),
            "main.rs binds only through startup::bind_listener, before the workers"
        );
    }

    /// F59 (#145): the exit never waits for the blocking pool without a
    /// bound. A plain drop of the runtime waits for every blocking task
    /// (the codec ceiling is 2 hours). The bound is `shutdown_timeout`, read
    /// from `DGP_SHUTDOWN_TIMEOUT_SECS`.
    #[test]
    fn the_runtime_never_waits_for_blocking_tasks_without_a_bound() {
        let main = include_str!("main.rs");
        assert!(
            main.contains("runtime.shutdown_timeout(")
                && main.contains("tuning.shutdown_timeout_secs"),
            "main.rs must end the runtime with shutdown_timeout"
        );
        assert_eq!(
            main.matches("Builder::new_multi_thread()").count(),
            1,
            "one runtime, one bounded exit"
        );
    }

    /// F22 (#145): every filesystem data root gets the stale-temp sweep at
    /// boot, in a background task (never awaited on the boot path), with the
    /// age rule of `STALE_TEMP_MAX_AGE`. Only `filesystem/sweep.rs` decides
    /// who is stale; `.dg-tmp.` is made by one function (`internal_temp_in`).
    #[test]
    fn the_boot_sweeps_stale_temp_files_in_the_background() {
        let main = include_str!("main.rs");
        let startup = include_str!("startup.rs");
        assert!(
            main.contains("startup::spawn_stale_temp_sweeps(&config);"),
            "async_main no longer starts the stale temp sweeps"
        );
        let f = &startup[startup
            .find("pub fn spawn_stale_temp_sweeps(config: &Config) {")
            .expect("startup::spawn_stale_temp_sweeps")..];
        let f = &f[..f.find("\n}\n").expect("end of the function")];
        let call = f
            .find("sweep_stale_temp_files(")
            .expect("the function no longer sweeps stale temp files");
        let spawn = f[..call]
            .rfind("spawn_blocking")
            .expect("sweep is not spawned");
        assert!(
            call - spawn < 200 && !f[spawn..].contains(".await"),
            "the sweep must run inside spawn_blocking, not on the boot path"
        );
        assert!(
            f.contains("filesystem_roots()") && f.contains("STALE_TEMP_MAX_AGE"),
            "the sweep covers every filesystem root with the stale-age rule"
        );
        let write = include_str!("storage/filesystem/write.rs");
        assert_eq!(
            write.matches(".prefix(INTERNAL_TEMP_PREFIX)").count(),
            1,
            "one function names the temp files"
        );
    }

    /// #120: production code names the `dg-sealed` key only where a sealed
    /// object is handled: the encrypting wrapper (which seals and opens),
    /// the key's definition, and the inner-backend code that must recognise
    /// a sealed shape so that it never derives clear facts from it. A new
    /// file here is a place where the seal could leave the wrapper.
    #[test]
    fn the_seal_key_is_named_only_where_a_sealed_shape_is_handled() {
        const ALLOWED: &[&str] = &[
            "src/types.rs",
            "src/storage/encrypting/seal.rs",
            "src/storage/encrypting/mod.rs",
            "src/storage/encrypting/v2.rs",
            "src/storage/list_size_cache.rs",
            "src/storage/traits.rs",
            "src/storage/s3/metadata.rs",
            "src/storage/s3/heal.rs",
        ];
        // Any spelling: the string, or the constant under any path or
        // alias (`mk::SEALED`, `use ...meta_keys::{SEALED}`, `SEALED_KEY`).
        fn names_the_seal(code: &str) -> bool {
            code.contains("dg-sealed")
                || code
                    .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .any(|token| token == "SEALED" || token == "SEALED_KEY")
        }
        for (line, names) in [
            ("let k = mk::SEALED;", true),
            ("use crate::types::meta_keys::{SEALED, TOOL};", true),
            ("m.get(seal::SEALED_KEY)", true),
            ("\"x-amz-meta-dg-sealed\"", true),
            ("SEAL_IDENTITY_PREFIX", false),
            ("UNSEALED_COUNT", false),
        ] {
            assert_eq!(names_the_seal(line), names, "{line}");
        }
        let mut bad = Vec::new();
        for (file, text) in crate::source_scan::prod_sources("src") {
            if ALLOWED.contains(&file.as_str()) {
                continue;
            }
            for (n, line) in crate::source_scan::prod_lines(&text) {
                let code = line.split("//").next().unwrap_or("");
                if names_the_seal(code) {
                    bad.push(format!("{file}:{n}"));
                }
            }
        }
        assert!(bad.is_empty(), "the seal key outside its places: {bad:?}");
    }

    /// NEW-07: a key is a delta baseline only when its last segment IS
    /// `reference.bin` (`types::is_reference_key`). A text match on the
    /// whole key (`ends_with`, `contains`) also takes `api-reference.bin`:
    /// `s3 sync --delete` deleted such client files.
    #[test]
    fn reference_keys_are_matched_by_the_one_rule() {
        let mut bad = Vec::new();
        for src in crate::source_scan::ast::src() {
            for (n, what) in reference_text_matches(&src.prod) {
                bad.push(format!("{}:{n}: {what}", src.rel));
            }
        }
        assert!(bad.is_empty(), "use types::is_reference_key: {bad:?}");
        assert!(crate::types::is_reference_key("releases/v1/reference.bin"));
        assert!(crate::types::is_reference_key("reference.bin"));
        assert!(!crate::types::is_reference_key("docs/api-reference.bin"));
        assert!(!crate::types::is_reference_key("reference.bin/x"));
    }

    /// Does `lit` name `reference.bin` without a `/` right before it? The
    /// segment-anchored `"/reference.bin"` is a correct test of the last
    /// segment; a bare `"reference.bin"` suffix also takes `api-reference.bin`.
    fn unanchored_reference(lit: &str) -> bool {
        lit.match_indices("reference.bin")
            .any(|(i, _)| !lit[..i].ends_with('/'))
    }

    /// `(line, what)` of every text test of a key against `reference.bin`:
    /// a text method (`ends_with`, `contains`, `strip_suffix`, `rsplit`, ...)
    /// with a literal that names it, however the call is laid out, and a `==`
    /// against `.to_string()` / `.as_str()` text of it. The old scan wanted
    /// `ends_with("reference.bin` or `contains("reference.bin` on one line.
    fn reference_text_matches(f: &crate::source_scan::ast::Facts) -> Vec<(usize, String)> {
        let mut out: Vec<(usize, String)> = f
            .calls
            .iter()
            .filter(|c| {
                c.method && crate::source_scan::ast::TEXT_METHODS.contains(&c.name.as_str())
            })
            .filter(|c| c.arg_strings.iter().any(|l| unanchored_reference(l)))
            .map(|c| (c.line, format!(".{}(\"..reference.bin..\")", c.name)))
            .collect();
        out.extend(
            f.cmps
                .iter()
                .filter(|c| {
                    [&c.lhs, &c.rhs]
                        .iter()
                        .any(|side| side.strings.iter().any(|l| unanchored_reference(l)))
                        && [&c.lhs, &c.rhs].iter().any(|side| {
                            side.method
                                .as_deref()
                                .is_some_and(|m| matches!(m, "to_string" | "as_str" | "as_ref"))
                        })
                })
                .map(|c| (c.line, "key == \"..reference.bin\"".to_string())),
        );
        // A bare file name against the whole literal (`name == "reference.bin"`)
        // is a second copy of the rule too: it is `types::REFERENCE_FILE_NAME`
        // (the listing hides baselines through the one rule, and the S3 and
        // filesystem listings must agree: `s3_and_the_filesystem_list_the_same_keys`).
        out.extend(
            f.cmps
                .iter()
                .filter(|c| {
                    [&c.lhs, &c.rhs]
                        .iter()
                        .any(|side| side.strings.iter().any(|l| l == "reference.bin"))
                })
                .map(|c| (c.line, "name == \"reference.bin\"".to_string())),
        );
        out.sort();
        out.dedup();
        out
    }

    #[test]
    fn the_reference_key_guard_sees_split_and_other_text_methods() {
        use crate::source_scan::ast::parse;
        let n = |t: &str| reference_text_matches(&parse("f.rs", t).prod).len();
        assert_eq!(n("fn f() { k.ends_with(\"reference.bin\"); }"), 1);
        assert_eq!(n("fn f() { k.contains(\"x-reference.bin\"); }"), 1);
        assert_eq!(
            n("fn f() { k.ends_with(\"/reference.bin\"); }"),
            0,
            "anchored"
        );
        assert_eq!(
            n("fn f() { k.contains(\"/.dg/reference.bin\"); }"),
            0,
            "anchored"
        );
        // Bypasses of the one-line scan.
        assert_eq!(
            n("fn f() { k\n    .ends_with(\n        \"reference.bin\",\n    ); }"),
            1,
            "split"
        );
        assert_eq!(
            n("fn f() { k.strip_suffix(\"reference.bin\"); }"),
            1,
            "strip_suffix"
        );
        assert_eq!(
            n("fn f() { if k.as_str() == \"x-reference.bin\" {} }"),
            1,
            "=="
        );
        assert_eq!(n("fn f() { crate::types::is_reference_key(k); }"), 0);
        assert_eq!(n("fn f() { // k.ends_with(\"reference.bin\")\n }"), 0);
    }

    /// #120: the v1 at-rest formats have no reader and no writer. Their
    /// names may appear only as the stored marker that keeps older releases
    /// failing closed (`seal::DISK_MARKER`).
    #[test]
    fn no_v1_encryption_format_remains() {
        let mut bad = Vec::new();
        for (file, text) in crate::source_scan::prod_sources("src") {
            for (n, line) in crate::source_scan::prod_lines(&text) {
                let code = line.split("//").next().unwrap_or("");
                let v1 = [
                    "aes-256-gcm-v1",
                    "DGE1",
                    "encrypt_chunk",
                    "ChunkFramer",
                    "CHUNK_MARKER_VALUE",
                ]
                .iter()
                .any(|k| code.contains(k));
                let marker = code.contains("aes-256-gcm-chunked-v1")
                    && !(file == "src/storage/encrypting/seal.rs" && code.contains("DISK_MARKER"));
                if v1 || marker {
                    bad.push(format!("{file}:{n}: {}", line.trim()));
                }
            }
        }
        assert!(bad.is_empty(), "v1 encryption code: {bad:?}");
    }

    /// N3 (`docs/plan/encryption-needs-atomic-writes.md`): the removed
    /// read-policy field of an encryption block and its `serve` plumbing
    /// stay gone. No identifier of them in `src/`, the UI source, or
    /// `docs/product`. The exceptions: the one config error that names
    /// the field (and the tests that assert it), the troubleshooting entry
    /// of that error, and the release history (upgrade guide, changelog). The metric
    /// `deltaglider_plaintext_reads_total` is another identifier.
    #[test]
    fn no_plaintext_reads_remains() {
        use crate::source_scan::{files, root};
        // Built at runtime, so that this file does not match itself.
        let removed: Vec<String> = [
            ("plaintext", "_reads"),
            ("serve", "_plaintext"),
            ("serves", "_plaintext"),
            ("serves", "_plaintext_deprecated"),
            ("Plaintext", "Reads"),
            ("ServeDeprecation", "Notice"),
        ]
        .iter()
        .map(|(a, b)| format!("{a}{b}"))
        .collect();
        let allowed = |rel: &str, line: &str| match rel {
            "src/config/mod.rs" => line.contains("const REMOVED: &str"),
            "src/config/tests/bucket_encryption_tests.rs" => true,
            "docs/product/how-to/upgrade.md" | "docs/product/changelog.md" => true,
            // The entry for the error, by its words.
            "docs/product/how-to/troubleshooting.md" => line.contains("was removed"),
            _ => false,
        };
        let mut bad = Vec::new();
        let mut scanned = 0;
        for (dir, exts) in [
            ("src", &["rs"][..]),
            ("demo/s3-browser/ui/src", &["ts", "tsx"][..]),
            ("docs/product", &["md"][..]),
        ] {
            for ext in exts {
                for path in files(dir, ext) {
                    scanned += 1;
                    let rel = path
                        .strip_prefix(root())
                        .unwrap()
                        .to_string_lossy()
                        .to_string();
                    let text = std::fs::read_to_string(&path).unwrap();
                    for (n, line) in text.lines().enumerate() {
                        if names_any(line, &removed) && !allowed(&rel, line) {
                            bad.push(format!("{rel}:{}: {}", n + 1, line.trim()));
                        }
                    }
                }
            }
        }
        assert!(scanned > 500, "the scan found the sources ({scanned})");
        assert!(
            bad.is_empty(),
            "the field is removed (N3), name none of it:\n{}",
            bad.join("\n")
        );
        // The matcher sees each removed name as a whole identifier only.
        for name in &removed {
            for line in [
                format!("{name}: serve"),
                format!("cfg.{name} = true;"),
                format!("e.config.{name}()"),
                format!("`encryption.{name}`"),
            ] {
                assert!(names_any(&line, &removed), "{line}");
            }
            for line in [format!("deltaglider_{name}_total"), format!("rule_{name}(")] {
                assert!(!names_any(&line, &removed), "{line}");
            }
        }
    }

    /// Does `line` name one of `names` as a whole identifier (not as part
    /// of a longer one)?
    fn names_any(line: &str, names: &[String]) -> bool {
        let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
        names.iter().any(|name| {
            line.match_indices(name.as_str()).any(|(at, _)| {
                let before = line[..at].chars().next_back();
                let after = line[at + name.len()..].chars().next();
                !before.is_some_and(ident) && !after.is_some_and(ident)
            })
        })
    }

    /// Review B1: what S3 does with no IAM user has ONE derivation
    /// (`Config::empty_iam_outcome` → `IamState::from_outcome`). Production
    /// code outside `iam/mod.rs` may match on `IamState::Disabled` /
    /// `IamState::Legacy`, never build one: a hand-built `Disabled` was how
    /// "no pair" turned into open access (N1).
    #[test]
    fn open_or_legacy_iam_state_is_built_in_one_place() {
        let mut bad = Vec::new();
        for src in crate::source_scan::ast::src() {
            if src.rel == "src/iam/mod.rs" {
                continue;
            }
            for (n, what) in open_or_legacy_state_values(&src.prod) {
                bad.push(format!("{}:{n}: {what}", src.rel));
            }
        }
        assert!(
            bad.is_empty(),
            "IamState built by hand:\n{}",
            bad.join("\n")
        );
    }

    /// `(line, what)` of every VALUE `IamState::Disabled` or
    /// `IamState::Legacy` in `f`: a unit use, a call (`Legacy(pair)`), a
    /// function pointer (`.map(IamState::Legacy)`). A pattern (a `match`
    /// arm, `if let`, `let .. else`, `matches!`) is no value, wherever it
    /// stands in the file; the old scan told them apart by `=>` and a few
    /// words on the same line.
    fn open_or_legacy_state_values(f: &crate::source_scan::ast::Facts) -> Vec<(usize, String)> {
        f.paths
            .iter()
            .filter(|p| p.expr && p.segs.len() >= 2)
            .filter(|p| {
                let n = p.segs.len();
                p.segs[n - 2] == "IamState"
                    && matches!(p.segs[n - 1].as_str(), "Disabled" | "Legacy")
            })
            .map(|p| (p.line, p.segs[p.segs.len() - 2..].join("::")))
            .collect()
    }

    #[test]
    fn the_iam_state_guard_sees_an_arm_that_builds_open_access() {
        use crate::source_scan::ast::parse;
        let n = |t: &str| open_or_legacy_state_values(&parse("f.rs", t).prod).len();
        assert_eq!(
            n("fn f() { let s = match k { None => IamState::Disabled, _ => x }; }"),
            1
        );
        assert_eq!(n("fn f() { let s = IamState::Legacy(pair); }"), 1);
        assert_eq!(
            n("fn f() { let s = pair.map(IamState::Legacy); }"),
            1,
            "function pointer"
        );
        // The old scan skipped any line with `if let` or `matches!(`, so a
        // value built on such a line passed.
        assert_eq!(
            n("fn f() { let s = if let Some(p) = pair { IamState::Legacy(p) } else { IamState::Disabled }; }"),
            2
        );
        assert_eq!(n("fn f() { let b = matches!(s, IamState::Disabled); }"), 0);
        assert_eq!(
            n("fn f() { match s { IamState::Disabled => 1, IamState::Legacy(_) => 2, _ => 3 }; }"),
            0
        );
        assert_eq!(n("fn f() { if let IamState::Legacy(a) = &s {} }"), 0);
        assert_eq!(n("fn f() { // IamState::Disabled\n }"), 0);
    }

    #[test]
    fn deployment_pins_follow_the_proxy_version() {
        // The release PR commits every file release-prep.sh stamps: a hand
        // list in the workflow left out the stamped docs (v2.0.3).
        let workflow = crate::source_scan::read(".github/workflows/prepare-release.yml");
        assert!(
            workflow.contains("git add -u"),
            "prepare-release.yml must stage every file release-prep.sh changes (git add -u)"
        );
        let field = |text: &str, prefix: &str| -> String {
            text.lines()
                .find_map(|l| l.trim().strip_prefix(prefix))
                .unwrap_or_else(|| panic!("no `{prefix}` line"))
                .trim()
                .trim_matches('"')
                .to_string()
        };
        let proxy = field(&crate::source_scan::read("Cargo.toml"), "version = ");
        let crd = crate::source_scan::read("operator/src/crd.rs");
        assert!(
            crd.contains(&format!("beshultd/deltaglider_proxy:{proxy}\"")),
            "operator DEFAULT_IMAGE is not the proxy version {proxy}"
        );
        let chart = crate::source_scan::read("charts/deltaglider-proxy/Chart.yaml");
        assert_eq!(field(&chart, "appVersion:"), proxy, "chart appVersion");
        let operator = field(
            &crate::source_scan::read("operator/Cargo.toml"),
            "version = ",
        );
        let deploy = crate::source_scan::read("operator/deploy/operator.yaml");
        assert!(
            deploy.contains(&format!("beshultd/deltaglider-operator:{operator}")),
            "operator/deploy/operator.yaml does not pin operator {operator}"
        );
        // The docs that name one image version (release-prep.sh stamps them
        // too). Every `X.Y.Z` after the prefix is the proxy version.
        for (path, prefix) in [
            (
                "docs/product/how-to/upgrade.md",
                "beshultd/deltaglider_proxy:",
            ),
            ("operator/README.md", "beshultd/deltaglider_proxy:"),
            ("DOCKERHUB.md", "| `"),
            (
                "docs/product/tutorials/first-delta-savings.md",
                "Starting DeltaGlider Proxy v",
            ),
            (
                "examples/docker-compose/secrets.env.example",
                "DGP_IMAGE_TAG=",
            ),
        ] {
            let text = crate::source_scan::read(path);
            let versions: Vec<&str> = text
                .match_indices(prefix)
                .filter_map(|(i, _)| {
                    let rest = &text[i + prefix.len()..];
                    let end = rest
                        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
                        .unwrap_or(rest.len());
                    let v = &rest[..end];
                    (v.split('.').count() == 3 && v.split('.').all(|p| !p.is_empty())).then_some(v)
                })
                .collect();
            assert!(!versions.is_empty(), "{path}: no `{prefix}X.Y.Z` to stamp");
            for v in versions {
                assert_eq!(v, proxy, "{path} names {v}, the proxy is {proxy}");
            }
        }
    }

    /// The release image compiles with the toolchain that rust-toolchain.toml
    /// pins. v2.0.0's image build failed because the Dockerfile still pinned
    /// rust:1.92 while the AWS SDK needed 1.94.1 and the repo used 1.98.
    #[test]
    fn dockerfile_rust_matches_the_pinned_toolchain() {
        let toolchain = crate::source_scan::read("rust-toolchain.toml");
        let channel = toolchain
            .lines()
            .find_map(|l| l.trim().strip_prefix("channel = "))
            .expect("rust-toolchain.toml has a channel")
            .trim_matches('"')
            .to_string();
        let minor = channel.rsplitn(2, '.').last().unwrap().to_string();
        // The operator image builds with context operator/, where the root
        // rust-toolchain.toml is not visible: its pin must follow too.
        for path in ["Dockerfile", "operator/Dockerfile"] {
            let dockerfile = crate::source_scan::read(path);
            let pins: Vec<&str> = dockerfile
                .lines()
                .filter_map(|l| l.trim().strip_prefix("FROM rust:"))
                .collect();
            assert!(!pins.is_empty(), "{path} has no FROM rust: stage");
            for pin in pins {
                let version = pin.split('-').next().unwrap();
                assert!(
                    version == channel || version == minor,
                    "{path} pins rust:{pin}, rust-toolchain.toml pins {channel}: bump them together"
                );
            }
        }
    }

    /// A step of a job that runs in a `container:` runs under `sh`, not
    /// bash, unless it names a shell. `set -o pipefail` fails under `sh`,
    /// and the Docker Hub description sync failed that way in v2.0.0 to
    /// v2.0.2, hidden by `continue-on-error`.
    #[test]
    fn container_job_steps_with_bash_syntax_name_their_shell() {
        let shell_of = |v: &serde_yaml::Value| {
            v.get("defaults")
                .and_then(|d| d.get("run"))
                .and_then(|r| r.get("shell"))
                .is_some()
        };
        let mut offenders = Vec::new();
        for file in crate::source_scan::files(".github/workflows", "yml") {
            let rel = rel_path(&file);
            let doc: serde_yaml::Value =
                serde_yaml::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
            let Some(jobs) = doc.get("jobs").and_then(|j| j.as_mapping()) else {
                continue;
            };
            for (name, job) in jobs {
                if job.get("container").is_none() || shell_of(&doc) || shell_of(job) {
                    continue;
                }
                let steps = job.get("steps").and_then(|s| s.as_sequence());
                for step in steps.into_iter().flatten() {
                    let run = step.get("run").and_then(|r| r.as_str()).unwrap_or("");
                    if run.contains("pipefail") && step.get("shell").is_none() {
                        let step_name = step.get("name").and_then(|n| n.as_str());
                        offenders.push(format!("{rel} {name:?} {step_name:?}"));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "add `shell: bash` to these container-job steps:\n{}",
            offenders.join("\n")
        );
    }

    /// Every object HEAD the server sends is counted in
    /// `deltaglider_backend_head_requests_total`, so the counter can prove
    /// that a path (a client LIST, the folder-size scan) sends none. The CLI
    /// runs in its own process and has no metrics endpoint.
    #[test]
    fn every_server_head_request_is_counted() {
        let mut offenders = Vec::new();
        for src in crate::source_scan::ast::src() {
            // The CLI runs in its own process: no metrics endpoint.
            if src.rel.starts_with("src/cli/") {
                continue;
            }
            offenders.extend(
                uncounted_heads(src)
                    .into_iter()
                    .map(|n| format!("{}:{n}", src.rel)),
            );
        }
        assert!(
            offenders.is_empty(),
            "increment BACKEND_HEAD_REQUESTS right before each head_object():\n{}",
            offenders.join("\n")
        );
    }

    /// Lines of the `.head_object()` calls of `src` with no
    /// `BACKEND_HEAD_REQUESTS.inc()` in the SAME fn, 0-8 lines above. (The
    /// old scan took any `inc()` text 8 lines above, even one in the fn
    /// that ends there.)
    fn uncounted_heads(src: &crate::source_scan::ast::Source) -> Vec<usize> {
        let f = &src.prod;
        f.calls
            .iter()
            .filter(|c| c.method && c.name == "head_object" && c.argc == 0)
            .filter(|h| {
                !f.calls.iter().any(|i| {
                    i.method
                        && i.name == "inc"
                        && i.root_ident.as_deref() == Some("BACKEND_HEAD_REQUESTS")
                        && i.func == h.func
                        && i.line <= h.line
                        && h.line - i.line <= 8
                })
            })
            .map(|h| h.line)
            .collect()
    }

    #[test]
    fn the_head_counter_guard_needs_the_increment_in_the_same_fn() {
        use crate::source_scan::ast::parse;
        let n = |t: &str| uncounted_heads(&parse("f.rs", t)).len();
        assert_eq!(
            n("fn a() { BACKEND_HEAD_REQUESTS.inc(); c.head_object().send(); }"),
            0
        );
        // An increment in the fn above (within 8 lines) counted for this one.
        assert_eq!(
            n("fn a() { BACKEND_HEAD_REQUESTS.inc(); }\nfn b() { c.head_object().send(); }"),
            1
        );
        // An increment that is more than 8 lines up.
        let far = format!(
            "fn a() {{ BACKEND_HEAD_REQUESTS.inc();{}c.head_object(); }}",
            "\n".repeat(10)
        );
        assert_eq!(n(&far), 1);
        // A commented increment.
        assert_eq!(
            n("fn a() { // BACKEND_HEAD_REQUESTS.inc()\n c.head_object(); }"),
            1
        );
    }

    /// Every GET body of the S3 backend is read through
    /// `S3Backend::body_stream` (`src/storage/s3/body.rs`), which resumes a
    /// body that breaks off. A body read anywhere else fails the whole
    /// request when the backend stalls once (#102). The `.body` field of a
    /// response is the sign; `.body(..)` is the request builder of a PUT.
    #[test]
    fn every_s3_get_body_is_read_through_the_resuming_stream() {
        let mut offenders = Vec::new();
        for src in crate::source_scan::ast::src() {
            // body.rs IS the resuming stream. The coordination plane (leases,
            // locks, the config DB sync, the CAS probes) has its own client
            // and reads small blobs, where a broken read fails the attempt
            // on purpose.
            if src.rel == "src/storage/s3/body.rs"
                || src.rel == "src/storage/fake_s3.rs"
                || src.rel.starts_with("src/coordination/")
                || src.rel == "src/config_db_sync.rs"
            {
                continue;
            }
            offenders.extend(
                sdk_body_reads(src)
                    .into_iter()
                    .map(|n| format!("{}:{n}", src.rel)),
            );
        }
        assert!(
            offenders.is_empty(),
            "read a GET response body with S3Backend::body_stream or collect_body:\n{}",
            offenders.join("\n")
        );
    }

    /// Lines where `src` reads the `body` of an S3 SDK response: a `.body`
    /// field read or a `GetObjectOutput { body, .. }` pattern. In scope: every
    /// file under `src/storage/` and every file that names `aws_sdk_s3`
    /// (the old scan listed only `src/storage`, so a new file elsewhere that
    /// used the SDK escaped). `.body(..)` is the request builder of a PUT: a
    /// call, not a field.
    fn sdk_body_reads(src: &crate::source_scan::ast::Source) -> Vec<usize> {
        if !(src.rel.starts_with("src/storage/") || src.prod.mentions("aws_sdk_s3")) {
            return Vec::new();
        }
        let f = &src.prod;
        let mut out: Vec<usize> = f
            .fields
            .iter()
            .filter(|x| x.name == "body")
            .map(|x| x.line)
            .collect();
        out.extend(
            f.pat_fields
                .iter()
                .filter(|p| p.field == "body" && p.path_last.ends_with("Output"))
                .map(|p| p.line),
        );
        out.sort();
        out
    }

    #[test]
    fn the_s3_body_guard_sees_a_destructuring_and_a_file_outside_storage() {
        use crate::source_scan::ast::parse;
        let n = |rel: &str, t: &str| sdk_body_reads(&parse(rel, t)).len();
        assert_eq!(n("src/storage/x.rs", "fn f(o: O) { let b = o.body; }"), 1);
        // A request builder, a method of the same name: not a read.
        assert_eq!(
            n("src/storage/x.rs", "fn f() { b.body(x); o.body_len(); }"),
            0
        );
        // The destructuring form: no `.body` text anywhere.
        assert_eq!(
            n(
                "src/storage/x.rs",
                "fn f(o: O) { let GetObjectOutput { body, .. } = o; }"
            ),
            1
        );
        // A file outside `src/storage` that uses the SDK is in scope; one
        // that does not is not (axum and s3s have bodies of their own).
        let sdk = "use aws_sdk_s3::Client;\nfn f(o: O) { let b = o.body; }";
        assert_eq!(n("src/cli/new.rs", sdk), 1);
        assert_eq!(n("src/cli/new.rs", "fn f(o: O) { let b = o.body; }"), 0);
    }

    /// s3s replaces every header of an error response with the error's own
    /// header map, so a header map on an error with an XML body drops its
    /// `Content-Type` and, with it, the `<RequestId>`. s3s also writes an
    /// XML body for a 304; the S3 response layer removes it
    /// (`strip_forbidden_body`). Only `s3_adapter_s3s::error_with_headers`,
    /// which puts the `Content-Type` back for a status with a body, sets a
    /// header map. A 503's `Retry-After` comes from
    /// `api::errors::ensure_retry_after` on the response.
    #[test]
    fn s3s_error_headers_go_through_error_with_headers() {
        let mut offenders = Vec::new();
        for src in crate::source_scan::ast::src() {
            for c in src.prod.calls_named("set_headers") {
                let in_helper = src.rel == "src/s3_adapter_s3s/mod.rs"
                    && src.fn_name(c.func) == "error_with_headers";
                if !in_helper {
                    offenders.push(format!("{}:{}", src.rel, c.line));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "an s3s error header map replaces Content-Type; use \
             s3_adapter_s3s::error_with_headers or the S3 response layer:\n{}",
            offenders.join("\n")
        );
    }

    /// A log excerpt of a string is cut with `security::str_prefix`, never
    /// with a byte-index slice ending at `s.len().min(n)`: an index inside a
    /// multi-byte char panics, and the OAuth callback died on a client-sent
    /// `code` that way.
    #[test]
    fn no_byte_index_string_excerpts() {
        let mut offenders = Vec::new();
        for src in crate::source_scan::ast::src() {
            offenders.extend(
                byte_index_excerpts(&src.prod)
                    .into_iter()
                    .map(|n| format!("{}:{n}", src.rel)),
            );
        }
        assert!(
            offenders.is_empty(),
            "use security::str_prefix for string excerpts:\n{}",
            offenders.join("\n")
        );
    }

    /// Lines of `s[..s.len().min(n)]`-shaped slices in `f`: a range whose
    /// bounds use both `len` and `min` (`n.min(s.len())`,
    /// `std::cmp::min(s.len(), n)`). The old scan took `[..` and `.len()`
    /// and `.min(` on one line, so `cmp::min` and a line break passed.
    fn byte_index_excerpts(f: &crate::source_scan::ast::Facts) -> Vec<usize> {
        f.indexes
            .iter()
            .filter(|i| i.ranged)
            .filter(|i| {
                i.bound_idents.iter().any(|x| x == "len")
                    && i.bound_idents.iter().any(|x| x == "min")
            })
            .map(|i| i.line)
            .collect()
    }

    #[test]
    fn the_excerpt_guard_sees_cmp_min_and_split_forms() {
        use crate::source_scan::ast::parse;
        let n = |t: &str| byte_index_excerpts(&parse("f.rs", t).prod).len();
        assert_eq!(n("fn f() { &s[..s.len().min(40)]; }"), 1);
        assert_eq!(n("fn f() { &s[..40.min(s.len())]; }"), 1);
        assert_eq!(
            n("fn f() { &s[..std::cmp::min(s.len(), 40)]; }"),
            1,
            "cmp::min"
        );
        assert_eq!(
            n("fn f() { &s[..\n    s.len()\n        .min(40)]; }"),
            1,
            "split"
        );
        assert_eq!(
            n("fn f() { &s[..len]; &s[2..s.len()]; &s[..n.min(8)]; }"),
            0
        );
        assert_eq!(
            n("fn f() { let c = crate::security::str_prefix(s, 40); }"),
            0
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
            "src/storage/s3/tests.rs",
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
        let mut offenders = Vec::new();
        let names = named_strings(crate::source_scan::ast::src());
        for src in crate::source_scan::ast::src() {
            if ALLOWED.contains(&src.rel.as_str()) {
                continue;
            }
            for (n, what) in text_matches(src, &TOKENS, &names) {
                offenders.push(format!("{}:{n}: {what}", src.rel));
            }
        }
        assert!(
            offenders.is_empty(),
            "classify errors on their type/status/code, not their text:\n{}",
            offenders.join("\n")
        );
    }

    /// The strings a name stands for: every `const` and `static` item of
    /// `srcs`, by name (the last segment of its path), with the strings of
    /// the names its initialiser uses (`const A: &str = B;`), four deep.
    fn named_strings<'a>(
        srcs: impl IntoIterator<Item = &'a crate::source_scan::ast::Source>,
    ) -> std::collections::HashMap<String, Vec<String>> {
        use std::collections::HashMap;
        let mut raw: HashMap<String, (Vec<String>, Vec<String>)> = HashMap::new();
        for src in srcs {
            for b in src.prod.bindings.iter().chain(&src.test.bindings) {
                if b.item {
                    let e = raw.entry(b.name.clone()).or_default();
                    e.0.extend(b.init_strings.iter().cloned());
                    e.1.extend(b.init_idents.iter().cloned());
                }
            }
        }
        let mut out: HashMap<String, Vec<String>> =
            raw.iter().map(|(k, v)| (k.clone(), v.0.clone())).collect();
        for _ in 0..4 {
            for (name, (_, idents)) in &raw {
                let more: Vec<String> = idents
                    .iter()
                    .filter(|i| *i != name)
                    .flat_map(|i| out.get(i).cloned().unwrap_or_default())
                    .collect();
                let e = out.get_mut(name).unwrap();
                for m in more {
                    if !e.contains(&m) {
                        e.push(m);
                    }
                }
            }
        }
        out
    }

    /// `(line, what)` of every place `src` tests a string against a token
    /// of `tokens`, outside an `assert!`:
    /// - a text method (`contains`, `starts_with`, `find`, `strip_prefix`,
    ///   `eq`, ...) with a literal that holds a token, wherever the literal
    ///   sits in the arguments (`&"T"`, `["T", "U"]`, a line break). A name
    ///   in the arguments stands for its string: a `const`/`static` item
    ///   anywhere in the crate (`names`), or a `let` of the same fn
    ///   (`let k = "T"; m.contains(k)`);
    /// - `text.to_string() == "T"` (and `to_lowercase`, ...): a comparison
    ///   of rendered text, the other side a literal or a name as above. A
    ///   comparison of a structured value (`code == "NoSuchKey"` on
    ///   `err.code()`) is not text and passes;
    /// - rendered text (`to_string()`, `message()` ...) tested against a
    ///   value whose string the guard cannot find (a fn parameter, a call):
    ///   the guard cannot tell it from a token, so it fails closed. Case
    ///   folding (`to_lowercase`) is no Display text and is exempt.
    ///
    /// Words match as a case-insensitive substring (`<Code>NoSuchKey</Code>`),
    /// status numbers exactly. The old scan looked for the exact text
    /// `.contains("T")` on one line.
    fn text_matches(
        src: &crate::source_scan::ast::Source,
        tokens: &[&str],
        names: &std::collections::HashMap<String, Vec<String>>,
    ) -> Vec<(usize, String)> {
        const RENDERED: [&str; 5] = [
            "to_string",
            "to_lowercase",
            "to_ascii_lowercase",
            "to_uppercase",
            "message",
        ];
        // Display text: tested against an unknown value, the guard fails
        // closed. Case folding (`to_lowercase` of an e-mail) is no Display.
        const DISPLAY: [&str; 2] = ["to_string", "message"];
        // The methods that compare the whole receiver with an argument.
        const COMPARE: [&str; 7] = [
            "contains",
            "starts_with",
            "ends_with",
            "eq_ignore_ascii_case",
            "eq",
            "ne",
            "strip_prefix",
        ];
        let hit = |lit: &str| {
            tokens.iter().any(|t| {
                if t.chars().all(|c| c.is_ascii_digit()) {
                    lit == *t
                } else {
                    lit.to_ascii_lowercase().contains(&t.to_ascii_lowercase())
                }
            })
        };
        // The strings that `idents` (of code in fn `func`) stand for.
        let resolve = |func: usize, idents: &[String]| -> Vec<String> {
            let mut out = Vec::new();
            for i in idents {
                out.extend(names.get(i).cloned().unwrap_or_default());
                for b in src
                    .prod
                    .bindings
                    .iter()
                    .filter(|b| !b.item && b.plain && b.func == func && &b.name == i)
                {
                    out.extend(b.init_strings.iter().cloned());
                    for k in &b.init_idents {
                        out.extend(names.get(k).cloned().unwrap_or_default());
                    }
                }
            }
            out
        };
        let mut out = Vec::new();
        for c in &src.prod.calls {
            if !c.method
                || c.in_assert
                || !crate::source_scan::ast::TEXT_METHODS.contains(&c.name.as_str())
            {
                continue;
            }
            let named = resolve(c.func, &c.arg_idents_all);
            if let Some(l) = c.arg_strings.iter().chain(&named).find(|l| hit(l)) {
                out.push((c.line, format!(".{}(.. {l:?} ..)", c.name)));
            } else if COMPARE.contains(&c.name.as_str())
                && c.receiver_idents
                    .iter()
                    .any(|r| DISPLAY.contains(&r.as_str()))
                && c.arg_strings.is_empty()
                && named.is_empty()
                && !c.arg_idents_all.is_empty()
            {
                out.push((
                    c.line,
                    format!(
                        ".{}(..) of rendered text with a value that has no known string",
                        c.name
                    ),
                ));
            }
        }
        for c in &src.prod.cmps {
            if c.in_assert {
                continue;
            }
            for (lit, other) in [(&c.lhs, &c.rhs), (&c.rhs, &c.lhs)] {
                let rendered = other
                    .method
                    .as_deref()
                    .is_some_and(|m| RENDERED.contains(&m));
                if !rendered {
                    continue;
                }
                let named = resolve(c.func, &lit.idents);
                if let Some(l) = lit.strings.iter().chain(&named).find(|l| hit(l)) {
                    out.push((c.line, format!("rendered text == {l:?}")));
                } else if !lit.literal
                    && lit.strings.is_empty()
                    && named.is_empty()
                    && other
                        .method
                        .as_deref()
                        .is_some_and(|m| DISPLAY.contains(&m))
                {
                    out.push((
                        c.line,
                        "rendered text == a value that has no known string".to_string(),
                    ));
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    #[test]
    fn the_text_match_guard_sees_split_aliased_and_rendered_forms() {
        use crate::source_scan::ast::parse;
        let t = ["NoSuch", "Key"].concat();
        let tokens = [t.as_str(), "404"];
        let lines = |body: &str| -> Vec<usize> {
            let src = parse("f.rs", &format!("fn f() {{\n{body}\n}}"));
            text_matches(&src, &tokens, &named_strings([&src]))
                .into_iter()
                .map(|(n, _)| n)
                .collect()
        };
        assert_eq!(lines(&format!("if e.contains(\"{t}\") {{}}")), [2]);
        // rustfmt breaks a long condition after the `(`.
        assert_eq!(
            lines(&format!(
                "if message\n    .contains(\n        \"{t}\",\n    )\n{{}}"
            )),
            [3]
        );
        for method in [
            "starts_with",
            "ends_with",
            "eq_ignore_ascii_case",
            "find",
            "strip_prefix",
        ] {
            assert_eq!(
                lines(&format!("if m.{method}(\"{t}\") {{}}")),
                [2],
                "{method}"
            );
        }
        assert_eq!(lines(&format!("m.contains(&\"{t}\");")), [2]);
        // Bypasses of the old one-line text scan.
        // 1. an assert on the same line exempted the whole line;
        assert_eq!(
            lines(&format!(
                "if e.contains(\"{t}\") {{ debug_assert!(true); }}"
            )),
            [2]
        );
        // 2. a literal inside a pattern list or a longer string;
        assert_eq!(
            lines(&format!("e.contains([\"x\", \"{t}\"].as_ref());")),
            [2]
        );
        assert_eq!(lines(&format!("e.contains(\"<Code>{t}</Code>\");")), [2]);
        // 3. rendered text compared with `==`.
        assert_eq!(lines(&format!("if e.to_string() == \"{t}\" {{}}")), [2]);
        assert_eq!(lines("if \"404\" != e.to_string() {}"), [2]);
        // Not a hit: another literal, a bare mention, a structured compare,
        // a status number inside a longer literal, an assert.
        assert!(lines("m.contains(\"other\");").is_empty());
        assert!(lines(&format!("let k = \"{t}\";")).is_empty());
        assert!(lines(&format!("if code == \"{t}\" {{}}")).is_empty());
        assert!(lines("m.contains(\"4040\");").is_empty());
        assert!(lines(&format!("assert!(m.contains(\"{t}\"));")).is_empty());
    }

    /// A token held in a name is still a token; text of an error tested
    /// against a value with no known string fails closed.
    #[test]
    fn the_text_match_guard_resolves_names_and_fails_closed() {
        use crate::source_scan::ast::parse;
        let t = ["NoSuch", "Key"].concat();
        let tokens = [t.as_str(), "404"];
        let file = |text: &str| -> usize {
            let src = parse("f.rs", text);
            text_matches(&src, &tokens, &named_strings([&src])).len()
        };
        let f = |item: &str, body: &str| file(&format!("{item}\nfn f(e: &E) -> bool {{ {body} }}"));
        // A `const`/`static` item, through `&`, a path, a chain, a `concat!`.
        let konst = format!("const K: &str = \"{t}\";");
        assert_eq!(f(&konst, "e.to_string().contains(K)"), 1, "const");
        assert_eq!(f(&konst, "e.to_string().contains(&K)"), 1, "&const");
        assert_eq!(f(&konst, "e.to_string().contains(self::K)"), 1, "path");
        assert_eq!(f(&konst, "e.to_string() == K"), 1, "== const");
        assert_eq!(f(&konst, "K == e.to_string()"), 1, "const ==");
        assert_eq!(
            f(&format!("static K: &str = \"{t}\";"), "m.starts_with(K)"),
            1,
            "static"
        );
        assert_eq!(
            f(&format!("{konst}\nconst L: &str = K;"), "m.contains(L)"),
            1,
            "const of a const"
        );
        assert_eq!(
            f(
                "const K: &str = concat!(\"NoSuch\", \"Key\");",
                "m.contains(K)"
            ),
            1,
            "concat!"
        );
        assert_eq!(f("", &format!("let k = \"{t}\"; m.contains(k)")), 1, "let");
        assert_eq!(
            f(&konst, "let k = K; m.contains(k)"),
            1,
            "let bound to a const"
        );
        // A const that holds no token passes.
        assert_eq!(f("const K: &str = \"other\";", "m.contains(K)"), 0);
        // Rendered text against a value with no known string: closed.
        assert_eq!(f("", "e.to_string().contains(needle)"), 1, "unknown ident");
        assert_eq!(f("", "e.to_string() == other()"), 1, "unknown call");
        assert_eq!(f("", "e.message().starts_with(&p.prefix)"), 1, "a field");
        // A structured value and a plain text compare are no error text.
        assert_eq!(f("", "name.contains(needle)"), 0);
        assert_eq!(f("", "e.to_string().contains('x')"), 0, "a char");
        assert_eq!(f("", "e.code() == other()"), 0);
        assert_eq!(f("", "e.to_lowercase().ends_with(&suffix)"), 0, "case fold");
    }

    /// A request handler audits with the request's headers, so the entry
    /// names the client (user agent, forwarded IP). An empty header map
    /// dropped both (`backend_probe`, DeleteObjects/CopySource denials,
    /// the backup restore). Background tasks have no request and are out of
    /// scope (they live outside src/api and the adapter).
    #[test]
    fn request_audits_carry_the_request_headers() {
        const ALLOWED: [(&str, &str); 0] = [];
        let mut offenders = Vec::new();
        let mut allowed_hits = std::collections::BTreeSet::new();
        let mut audits = 0;
        for src in crate::source_scan::ast::src() {
            if !(src.rel.starts_with("src/s3_adapter_s3s/") || src.rel.starts_with("src/api/")) {
                continue;
            }
            audits += src.prod.calls_named("audit_log").count();
            for n in audits_with_empty_headers(src) {
                if ALLOWED.iter().any(|(f, _)| *f == src.rel) {
                    allowed_hits.insert(src.rel.clone());
                } else {
                    offenders.push(format!("{}:{n}", src.rel));
                }
            }
        }
        assert!(audits > 20, "scan found the audit calls ({audits})");
        assert!(
            offenders.is_empty(),
            "pass the request's headers to audit_log, not an empty map:\n{}",
            offenders.join("\n")
        );
        for (f, why) in ALLOWED {
            assert!(
                allowed_hits.contains(f),
                "{f} ({why}) no longer audits with an empty map: drop it from ALLOWED"
            );
        }
    }

    /// Lines of the `audit_log(..)` calls of `src` that pass an empty
    /// `HeaderMap`: built in the argument (`&HeaderMap::new()`), or a local
    /// of the same fn bound to one (`let h = HeaderMap::new();`). The old
    /// scan balanced parentheses over lines of text and looked ten lines up
    /// for the `let`.
    fn audits_with_empty_headers(src: &crate::source_scan::ast::Source) -> Vec<usize> {
        let f = &src.prod;
        let empty = |c: &str| c.ends_with("HeaderMap::new") || c.ends_with("HeaderMap::default");
        f.calls_named("audit_log")
            .filter(|c| {
                c.arg_calls.iter().any(|x| empty(x))
                    || c.arg_idents.iter().flatten().any(|name| {
                        f.bindings.iter().any(|b| {
                            b.func == c.func
                                && &b.name == name
                                && b.init_calls.iter().any(|x| empty(x))
                        })
                    })
            })
            .map(|c| c.line)
            .collect()
    }

    #[test]
    fn the_audit_guard_sees_a_far_binding_and_an_inline_map() {
        use crate::source_scan::ast::parse;
        let n = |t: &str| audits_with_empty_headers(&parse("f.rs", t)).len();
        assert_eq!(
            n("fn f() { audit_log(\"a\", u, t, &HeaderMap::new()); }"),
            1
        );
        assert_eq!(
            n("fn f() { let h = HeaderMap::new(); audit_log(\"a\", u, t, &h); }"),
            1
        );
        // The binding is 30 lines above: the ten-line look-back missed it.
        let far = format!(
            "fn f() {{ let h = HeaderMap::new();{}audit_log(\"a\", u, t, &h); }}",
            "\n".repeat(30)
        );
        assert_eq!(n(&far), 1, "far binding");
        assert_eq!(n("fn f(h: &HeaderMap) { audit_log(\"a\", u, t, h); }"), 0);
        assert_eq!(n("fn f() { let h = HeaderMap::new(); }\nfn g(h: &HeaderMap) { audit_log(\"a\", u, t, h); }"), 0);
    }

    /// `deltaglider_proxy.example.yaml` is the canonical example: every
    /// field of the four sections appears in it (commented out is fine),
    /// so a new setting cannot ship without an example. The field names
    /// come from the sections' JSON Schemas, nested types included.
    #[test]
    fn example_config_names_every_field() {
        fn property_names(v: &serde_json::Value, out: &mut std::collections::BTreeSet<String>) {
            match v {
                serde_json::Value::Object(map) => {
                    if let Some(serde_json::Value::Object(props)) = map.get("properties") {
                        out.extend(props.keys().cloned());
                    }
                    map.values().for_each(|x| property_names(x, out));
                }
                serde_json::Value::Array(items) => {
                    items.iter().for_each(|x| property_names(x, out))
                }
                _ => {}
            }
        }
        use crate::config_sections::{
            AccessSection, AdmissionSection, AdvancedSection, StorageSection,
        };
        let schemas = [
            serde_json::to_value(schemars::schema_for!(AdmissionSection)).unwrap(),
            serde_json::to_value(schemars::schema_for!(AccessSection)).unwrap(),
            serde_json::to_value(schemars::schema_for!(StorageSection)).unwrap(),
            serde_json::to_value(schemars::schema_for!(AdvancedSection)).unwrap(),
        ];
        let mut names = std::collections::BTreeSet::new();
        schemas.iter().for_each(|s| property_names(s, &mut names));
        assert!(
            names.len() > 50,
            "schema walk found the fields ({})",
            names.len()
        );
        let example =
            std::fs::read_to_string(root().join("deltaglider_proxy.example.yaml")).unwrap();
        let missing: Vec<&String> = names
            .iter()
            .filter(|n| !example.contains(&format!("{n}:")))
            .collect();
        assert!(
            missing.is_empty(),
            "fields missing from deltaglider_proxy.example.yaml: {missing:?}"
        );
    }

    /// Every integration-test spawn of the proxy binary sets `current_dir`.
    /// The proxy writes state files (`.deltaglider_bootstrap_hash`, and the
    /// config DB and its key file when `DGP_CONFIG` is unset) relative to
    /// its cwd. A spawn that inherits the test process's cwd writes them
    /// into the repo root, where a later dev run of the binary picks them up.
    ///
    /// Per spawn, not per file: the old guard compared the number of spawns
    /// with the number of `.current_dir(` in the whole file, so an unrelated
    /// `.current_dir(` (a `git` command) paid for a spawn that had none.
    #[test]
    fn integration_test_spawns_set_current_dir() {
        let files = crate::source_scan::ast::tests();
        assert!(files.len() > 50, "scan found tests/ ({})", files.len());
        let mut offenders = Vec::new();
        let bins = binary_path_names(files);
        for src in files {
            for n in spawns_without_cwd(src, &src.test, &bins) {
                offenders.push(format!("{}:{n}", src.rel));
            }
        }
        assert!(
            offenders.is_empty(),
            "spawn the proxy with .current_dir(<temp dir>): {offenders:?}"
        );
    }

    /// `coordination::durable_node_id` creates `<dir>/node-id` when no env
    /// names the node. Only the boot path (`startup.rs`), which owns the data
    /// directory, may call it; a label for a witness or a log line goes
    /// through `node_id_label`, which never writes. A lib test that reached
    /// the persisting form left `node-id` in the repository root.
    #[test]
    fn only_startup_persists_the_node_id() {
        const ALLOWED: [&str; 2] = ["src/startup.rs", "src/coordination/mod.rs"];
        let name = ["durable_node", "_id"].concat();
        let mut offenders = Vec::new();
        for src in crate::source_scan::ast::src() {
            if ALLOWED.contains(&src.rel.as_str()) {
                continue;
            }
            offenders.extend(
                names_path_leaf(src, &name)
                    .into_iter()
                    .map(|n| format!("{}:{n}", src.rel)),
            );
        }
        assert!(
            offenders.is_empty(),
            "use coordination::node_id_label (never writes) outside startup.rs:\n{}",
            offenders.join("\n")
        );
    }

    /// Lines where `src` names `leaf` as the last segment of any path: a
    /// call, a `use`, an `as` alias, a function pointer, a `<T as Tr>::leaf`.
    /// The old scan looked for the text `leaf(` on one line.
    fn names_path_leaf(src: &crate::source_scan::ast::Source, leaf: &str) -> Vec<usize> {
        let mut out: Vec<usize> = src
            .prod
            .paths
            .iter()
            .filter(|p| p.segs.last().is_some_and(|s| s == leaf))
            .map(|p| p.line)
            .collect();
        out.sort();
        out.dedup();
        out
    }

    #[test]
    fn the_leaf_guard_sees_aliases_and_pointers() {
        use crate::source_scan::ast::parse;
        let n = |t: &str| names_path_leaf(&parse("f.rs", t), "durable").len();
        assert_eq!(n("fn f() { a::durable(x); }"), 1);
        assert_eq!(n("fn f() { a::\ndurable\n(x); }"), 1, "split over lines");
        assert_eq!(
            n("use a::durable as d;\nfn f() { d(x); }"),
            1,
            "alias: the use names it"
        );
        assert_eq!(n("fn f() { let g = a::durable; }"), 1, "function pointer");
        assert_eq!(n("fn f() { // a::durable(x)\n let s = \"durable(\"; }"), 0);
    }

    /// Is one of `ms` `env!("CARGO_BIN_EXE_..")`?
    fn names_bin_exe(ms: &[(String, String)]) -> bool {
        ms.iter()
            .any(|(n, t)| (n == "env" || n == "option_env") && t.contains("CARGO_BIN_EXE_"))
    }

    /// The names that stand for the path of the proxy binary, in all of
    /// `files`: a `const`/`let` bound to `env!("CARGO_BIN_EXE_..")` (or to
    /// a value built from it, `PathBuf::from(env!(..))`), one bound to a
    /// name or a call of a name of this set (`let b = bin();`,
    /// `let c = b.clone();`), and a fn whose tail or `return` is one of
    /// those, to a fixed point. A helper `fn proxy_bin() -> &'static str {
    /// env!(..) }` in `tests/common/` is one.
    fn binary_path_names(
        files: &[crate::source_scan::ast::Source],
    ) -> std::collections::HashSet<String> {
        let mut names = std::collections::HashSet::new();
        loop {
            let before = names.len();
            for src in files {
                for f in [&src.prod, &src.test] {
                    for b in &f.bindings {
                        if names_bin_exe(&b.init_value_macros)
                            || b.init_root.as_ref().is_some_and(|r| names.contains(r))
                        {
                            names.insert(b.name.clone());
                        }
                    }
                    for v in &f.fn_values {
                        if names_bin_exe(&v.macros)
                            || v.root.as_ref().is_some_and(|r| names.contains(r))
                        {
                            names.insert(v.name.clone());
                        }
                    }
                }
            }
            if names.len() == before {
                return names;
            }
        }
    }

    /// The one function of `tests/common/` that builds a command for the
    /// proxy binary (and sets the parent-death signal on it).
    const PROXY_BINARY_HELPER: &str = "proxy_binary";

    /// A `X::new(arg)` where `arg` holds `env!("CARGO_BIN_EXE_..")` or a
    /// name of `bins`.
    fn is_direct_proxy_spawn(
        c: &crate::source_scan::ast::Call,
        bins: &std::collections::HashSet<String>,
    ) -> bool {
        !c.method
            && c.segs.len() >= 2
            && c.segs.last().is_some_and(|s| s == "new")
            && (names_bin_exe(&c.arg_macros) || c.arg_idents_all.iter().any(|i| bins.contains(i)))
    }

    /// Lines of the direct spawns of the proxy binary in `f` outside the
    /// helper [`PROXY_BINARY_HELPER`]. Such a command has no parent-death
    /// signal: when its `cargo test` is killed, the proxy runs on and holds
    /// a build slot of the host.
    fn spawns_outside_the_helper(
        src: &crate::source_scan::ast::Source,
        f: &crate::source_scan::ast::Facts,
        bins: &std::collections::HashSet<String>,
    ) -> Vec<usize> {
        f.calls
            .iter()
            .filter(|c| {
                is_direct_proxy_spawn(c, bins) && src.fn_name(c.func) != PROXY_BINARY_HELPER
            })
            .map(|c| c.line)
            .collect()
    }

    /// Every integration-test run of the proxy binary goes through
    /// `common::proxy_binary()`, which makes the child end with its parent
    /// (`PR_SET_PDEATHSIG`). A proxy that survived its `cargo test` ran for
    /// 31 hours and held a host build slot.
    #[test]
    fn integration_test_spawns_end_with_their_parent() {
        let files = crate::source_scan::ast::tests();
        assert!(files.len() > 50, "scan found tests/ ({})", files.len());
        let bins = binary_path_names(files);
        let mut offenders = Vec::new();
        for src in files {
            for n in spawns_outside_the_helper(src, &src.test, &bins) {
                offenders.push(format!("{}:{n}", src.rel));
            }
        }
        assert!(
            offenders.is_empty(),
            "build the command with common::proxy_binary(): {offenders:?}"
        );
        // The helper exists and is the one place that names the binary.
        let helper = files
            .iter()
            .filter(|s| {
                s.test.calls.iter().any(|c| {
                    is_direct_proxy_spawn(c, &bins) && s.fn_name(c.func) == PROXY_BINARY_HELPER
                })
            })
            .count();
        assert_eq!(helper, 1, "exactly one file defines {PROXY_BINARY_HELPER}");
    }

    #[test]
    fn the_parent_death_guard_sees_each_direct_spawn() {
        use crate::source_scan::ast::parse;
        let bin = ["CARGO_BIN", "_EXE_deltaglider_proxy"].concat();
        let outside = |text: &str| {
            let files = [parse("fixture.rs", text)];
            let bins = binary_path_names(&files);
            spawns_outside_the_helper(&files[0], &files[0].prod, &bins).len()
        };
        assert_eq!(
            outside(&format!(
                "fn proxy_binary() -> C {{ Command::new(env!(\"{bin}\")) }}"
            )),
            0,
            "the helper"
        );
        assert_eq!(
            outside(&format!(
                "fn a() {{ Command::new(env!(\"{bin}\")).output(); }}"
            )),
            1
        );
        assert_eq!(
            outside(&format!(
                "use std::process::Command as C;\nfn a() {{ C::new(env!(\"{bin}\")); }}"
            )),
            1,
            "alias"
        );
        assert_eq!(
            outside(&format!(
                "const BIN: &str = env!(\"{bin}\");\nfn a() {{ Command::new(BIN); }}"
            )),
            1,
            "const"
        );
        assert_eq!(
            outside("fn a() { common::proxy_binary().output(); }"),
            0,
            "the helper's callers"
        );
        // The cwd guard counts a call of the helper as a spawn.
        let no_cwd = |text: &str| {
            let files = [parse("fixture.rs", text)];
            let bins = binary_path_names(&files);
            spawns_without_cwd(&files[0], &files[0].prod, &bins).len()
        };
        assert_eq!(no_cwd("fn a() { common::proxy_binary().output(); }"), 1);
        assert_eq!(
            no_cwd("fn a() { common::proxy_binary().current_dir(d).output(); }"),
            0
        );
    }

    /// 1-based lines of the proxy spawns in `src` whose builder never gets a
    /// `.current_dir(..)`. A spawn is a `X::new(arg)` where `arg` holds
    /// `env!("CARGO_BIN_EXE_..")` or a name of `bins` (a `const`, a `let`,
    /// a fn returning the path, `bin.clone()`), whatever `X` is called
    /// (`use std::process::Command as C` still counts). The cwd
    /// is set on the builder chain (`Command::new(B).current_dir(d)`) or on
    /// the variable the chain was bound to (`let mut c = Command::new(B);
    /// c.current_dir(d)`) in the same fn.
    fn spawns_without_cwd(
        src: &crate::source_scan::ast::Source,
        f: &crate::source_scan::ast::Facts,
        bins: &std::collections::HashSet<String>,
    ) -> Vec<usize> {
        // The harness helper `proxy_binary()` is a spawn too; its own body
        // (the one direct spawn that the guard below allows) is not.
        let is_spawn = |c: &crate::source_scan::ast::Call| {
            (is_direct_proxy_spawn(c, bins) && src.fn_name(c.func) != PROXY_BINARY_HELPER)
                || (!c.method && c.segs.last().is_some_and(|s| s == PROXY_BINARY_HELPER))
        };
        let has_cwd = |spawn: &crate::source_scan::ast::Call| {
            let bound: Vec<&str> = f
                .bindings
                .iter()
                .filter(|b| b.init_root_pos == Some(spawn.pos) && b.func == spawn.func)
                .map(|b| b.name.as_str())
                .collect();
            f.calls.iter().any(|c| {
                c.method
                    && c.name == "current_dir"
                    && (c.root_pos == Some(spawn.pos)
                        || (c.func == spawn.func
                            && c.root_ident.as_deref().is_some_and(|r| bound.contains(&r))))
            })
        };
        f.calls
            .iter()
            .filter(|c| is_spawn(c) && !has_cwd(c))
            .map(|c| c.line)
            .collect()
    }

    #[test]
    fn the_cwd_guard_checks_each_spawn() {
        use crate::source_scan::ast::parse;
        let bad_in = |texts: &[&str]| {
            let files: Vec<_> = texts.iter().map(|t| parse("fixture.rs", t)).collect();
            let bins = binary_path_names(&files);
            files
                .iter()
                .map(|s| spawns_without_cwd(s, &s.prod, &bins).len())
                .sum::<usize>()
        };
        let bad = |text: &str| bad_in(&[text]);
        let bin = ["CARGO_BIN", "_EXE_deltaglider_proxy"].concat();
        let spawn = |extra: &str| format!("Command::new(env!(\"{bin}\"))\n{extra}.spawn();");
        // One spawn with a cwd: fine.
        assert_eq!(
            bad(&format!("fn a() {{ {} }}", spawn(".current_dir(tmp)\n"))),
            0
        );
        // Two spawns, one `.current_dir(` in the FILE (on a git command in
        // another function): the second spawn has none.
        let two_fns = format!(
            "fn a() {{ {} }}\nfn git() {{ Command::new(\"git\").current_dir(x); }}\nfn b() {{ {} }}",
            spawn(".current_dir(tmp)\n"),
            spawn("")
        );
        assert_eq!(bad(&two_fns), 1);
        // Two spawns in ONE function, a cwd on the first only.
        let one_fn = format!(
            "fn a() {{ {} {} }}",
            spawn(".current_dir(tmp)\n"),
            spawn("")
        );
        assert_eq!(bad(&one_fn), 1);
        // The `BIN` const form.
        let konst = format!(
            "const BIN: &str = env!(\"{bin}\");\nfn a() {{ Command::new(BIN).arg(\"x\"); }}"
        );
        assert_eq!(bad(&konst), 1);
        // Bypasses of the old text guard (a window of lines holding the text
        // `.current_dir(`):
        // 1. an alias of the command type;
        let alias = format!(
            "use std::process::Command as C;\nfn a() {{ C::new(env!(\"{bin}\")).spawn(); }}"
        );
        assert_eq!(bad(&alias), 1, "alias");
        // 2. a `.current_dir(` on ANOTHER command within the same fn;
        let other = format!(
            "fn a() {{ let mut g = Command::new(\"git\"); g.current_dir(x); {} }}",
            spawn("")
        );
        assert_eq!(bad(&other), 1, "other command's cwd");
        // 3. the text in a comment or a string.
        let text = format!(
            "fn a() {{ // .current_dir(\nlet _s = \".current_dir(\"; {} }}",
            spawn("")
        );
        assert_eq!(bad(&text), 1, "comment/string");
        // The cwd set through a variable, and in a split chain, both count.
        let var = format!(
            "fn a() {{ let mut c = Command::new(env!(\"{bin}\")); c.arg(1); c.current_dir(d); c.spawn(); }}"
        );
        assert_eq!(bad(&var), 0, "cwd on the bound variable");
        let split = format!(
            "fn a() {{ let c = Command::new(env!(\"{bin}\"))\n    .arg(1)\n    .current_dir(d); }}"
        );
        assert_eq!(bad(&split), 0, "cwd on a split chain");
        // A helper that returns the path hides the `env!` from the spawn.
        let helper = format!("fn proxy_bin() -> &'static str {{ env!(\"{bin}\") }}\n");
        for (what, text) in [
            ("tail", format!("{helper}fn a() {{ Command::new(proxy_bin()).spawn(); }}")),
            (
                "return",
                format!("fn pb() -> String {{ if x {{ return env!(\"{bin}\").into(); }} y }}\nfn a() {{ Command::new(pb()).spawn(); }}"),
            ),
            (
                "PathBuf::from",
                format!("fn pb() -> PathBuf {{ PathBuf::from(env!(\"{bin}\")) }}\nfn a() {{ Command::new(pb()).spawn(); }}"),
            ),
            (
                "clone of a let",
                format!("fn a() {{ let b = env!(\"{bin}\"); Command::new(b.clone()).spawn(); }}"),
            ),
            (
                "let bound to the helper",
                format!("{helper}fn a() {{ let b = proxy_bin(); Command::new(&b).spawn(); }}"),
            ),
            (
                "helper of a helper",
                format!("{helper}fn pb2() -> &'static str {{ proxy_bin() }}\nfn a() {{ Command::new(pb2()).spawn(); }}"),
            ),
        ] {
            assert_eq!(bad(&text), 1, "{what}");
        }
        // The helper in another file (`tests/common/mod.rs`).
        let user = "fn a() { Command::new(proxy_bin()).spawn(); }";
        assert_eq!(
            bad_in(&[helper.as_str(), user]),
            1,
            "helper in another file"
        );
        // A helper spawn that sets the cwd, and an unrelated fn, pass.
        let ok = format!("{helper}fn a() {{ Command::new(proxy_bin()).current_dir(d).spawn(); }}");
        assert_eq!(bad(&ok), 0);
        let other = format!("{helper}fn a() {{ Command::new(\"git\").arg(other()).spawn(); }}");
        assert_eq!(bad(&other), 0, "not the proxy binary");
    }

    /// Conversion marker P1: `FileMetadata::stored_last_modified` is the
    /// BACKEND's own time of the stored object (S3 `Last-Modified`, file
    /// mtime). A read rule trusts it to tell history from a plant, so only
    /// the backends' read paths set it, and the constructors start it at
    /// `None`. A new assignment elsewhere (a write path, a test fixture, a
    /// wrapper) could hand the rule a time that no backend reported. The
    /// same holds for `FileMetadata::backend_version` (N1): a conditional
    /// rewrite names it, so a version that no backend read could let the
    /// re-encrypt job overwrite a client's write.
    #[test]
    fn stored_last_modified_comes_from_the_backend() {
        for field in [
            ["stored_last", "_modified"].concat(),
            ["backend", "_version"].concat(),
        ] {
            only_backend_reads_set(&field);
        }
    }

    fn only_backend_reads_set(field: &str) {
        const ALLOWED: &[&str] = &[
            "src/types.rs",
            "src/storage/s3/metadata.rs",
            "src/storage/s3/listing.rs",
            "src/storage/filesystem/mod.rs",
        ];
        let mut bad = Vec::new();
        for src in crate::source_scan::ast::src()
            .iter()
            .chain(crate::source_scan::ast::tests())
        {
            if ALLOWED.contains(&src.rel.as_str()) {
                continue;
            }
            for f in [&src.prod, &src.test] {
                bad.extend(
                    field_writes(f, field)
                        .into_iter()
                        .map(|n| format!("{}:{n}", src.rel)),
                );
            }
        }
        assert!(
            bad.is_empty(),
            "{field} set outside the backends' read paths: {bad:?}"
        );
    }

    /// Lines where `f` writes the field `name`: `x.name = ..`, `x.name += ..`,
    /// `&mut x.name` (`mem::replace`, `take`), and a struct literal that
    /// names it, with a value (`name: v`) or shorthand (`name,`). The old
    /// scan wanted `name =` or `name:` on one line: the shorthand form and
    /// `&mut x.name` passed.
    fn field_writes(f: &crate::source_scan::ast::Facts, name: &str) -> Vec<usize> {
        f.field_writes
            .iter()
            .filter(|w| w.name == name)
            .map(|w| w.line)
            .collect()
    }

    #[test]
    fn the_field_write_guard_sees_shorthand_and_mut_borrows() {
        use crate::source_scan::ast::parse;
        let n = |t: &str| field_writes(&parse("f.rs", t).prod, "stored_last_modified").len();
        assert_eq!(n("fn f() { m.stored_last_modified = Some(t); }"), 1);
        assert_eq!(n("fn f() { M { stored_last_modified: None, ..d }; }"), 1);
        // Bypasses of the text scan.
        assert_eq!(
            n("fn f() { M { stored_last_modified, ..d }; }"),
            1,
            "shorthand"
        );
        assert_eq!(
            n("fn f() { std::mem::replace(&mut m.stored_last_modified, x); }"),
            1,
            "&mut"
        );
        // Reads and the declaration are no write.
        assert_eq!(n("fn f() { assert_eq!(m.stored_last_modified, x); if m.stored_last_modified == x {} }"), 0);
        assert_eq!(n("struct M { pub stored_last_modified: Option<u8> }"), 0);
    }

    /// Conversion marker P3: the job's read rule serves plaintext that a
    /// client may not read. Only the re-encrypt job enters it: the proof
    /// that opens the scope is made in `maintenance/worker.rs` alone, and
    /// production code enters the scope nowhere else.
    #[test]
    fn only_the_conversion_job_reads_as_the_job() {
        let mut bad = Vec::new();
        for src in crate::source_scan::ast::src() {
            bad.extend(conversion_scope_breaches(src));
        }
        assert!(bad.is_empty(), "the conversion job's read rule: {bad:?}");
    }

    /// Breaches of the conversion job's read rule in `src`: the proof made
    /// (`ConversionReadProof { .. }`) outside `maintenance/worker.rs`, its
    /// scope entered (`as_conversion_job(..)`) anywhere but the job
    /// dispatcher `run_job` of that file (the re-encrypt and the migrate
    /// arm), the test proof (`for_tests()`) in production code. The old scan
    /// counted the text `as_conversion_job(` per file.
    fn conversion_scope_breaches(src: &crate::source_scan::ast::Source) -> Vec<String> {
        let f = &src.prod;
        let mut bad = Vec::new();
        if src.rel != "src/maintenance/worker.rs" {
            for l in f
                .struct_lits
                .iter()
                .filter(|l| l.path_last == "ConversionReadProof")
            {
                bad.push(format!("{}:{}: makes the proof", src.rel, l.line));
            }
        }
        for c in f.calls_named("as_conversion_job") {
            let home = src.rel == "src/maintenance/worker.rs" && src.fn_name(c.func) == "run_job";
            if !home {
                bad.push(format!("{}:{}: enters the job's scope", src.rel, c.line));
            }
        }
        for c in f.calls.iter().filter(|c| {
            !c.method
                && c.segs.len() >= 2
                && c.segs[c.segs.len() - 2] == "ConversionReadProof"
                && c.segs[c.segs.len() - 1] == "for_tests"
        }) {
            bad.push(format!(
                "{}:{}: the test proof in production code",
                src.rel, c.line
            ));
        }
        bad
    }

    #[test]
    fn the_conversion_guard_sees_a_second_entry() {
        use crate::source_scan::ast::parse;
        let n = |rel: &str, t: &str| conversion_scope_breaches(&parse(rel, t)).len();
        let enter = "fn run_job() { conversion::as_conversion_job(P, fut).await; }";
        assert_eq!(n("src/maintenance/worker.rs", enter), 0);
        // The same file, another fn: the per-file count passed it while the
        // total stayed 2.
        let other =
            "fn run_job() { as_conversion_job(P, a); }\nfn sweep() { as_conversion_job(P, b); }";
        assert_eq!(n("src/maintenance/worker.rs", other), 1);
        assert_eq!(n("src/x.rs", enter), 1);
        assert_eq!(
            n(
                "src/x.rs",
                "fn f() { ConversionReadProof { _private: () }; }"
            ),
            1
        );
        assert_eq!(
            n("src/x.rs", "fn f() { ConversionReadProof::for_tests(); }"),
            1
        );
        assert_eq!(n("src/x.rs", "fn f() { // as_conversion_job(P, a)\n }"), 0);
    }

    /// `(line, what)` of the fixture shapes that test code `f` builds by
    /// hand: a `FileMetadata { .. }` struct literal, and (integration tests
    /// only) DG metadata written onto a stored object: `.metadata("dg-..")`,
    /// `.insert("dg-..")`, `.header("x-amz-meta-dg-..")`, `xattr::set(..)`.
    /// The `dg-no-delta` store hint is no stored metadata. The old scan read
    /// lines of text (and decided `FileMetadata {` by the word before it), so
    /// a call split over lines passed.
    fn hand_built_fixtures(
        f: &crate::source_scan::ast::Facts,
        integration: bool,
    ) -> Vec<(usize, String)> {
        let mut out: Vec<(usize, String)> = f
            .struct_lits
            .iter()
            .filter(|l| l.path_last == "FileMetadata")
            .map(|l| (l.line, "FileMetadata { .. }".to_string()))
            .collect();
        if integration {
            let no_delta = crate::types::meta_keys::NO_DELTA;
            for c in &f.calls {
                let dg = |l: &String| {
                    (l.starts_with("dg-") || l.starts_with("x-amz-meta-dg-"))
                        && !l.ends_with(no_delta)
                };
                let hand_written = c.method
                    && matches!(c.name.as_str(), "metadata" | "insert" | "header")
                    && c.arg_strings.iter().any(dg);
                let xattr = !c.method
                    && c.segs.len() >= 2
                    && c.segs[c.segs.len() - 2] == "xattr"
                    && c.segs[c.segs.len() - 1] == "set";
                if hand_written || xattr {
                    out.push((c.line, format!("{}(..)", c.name)));
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn the_fixture_guard_sees_literals_and_split_metadata_calls() {
        use crate::source_scan::ast::parse_test_file;
        let n = |integration: bool, t: &str| {
            hand_built_fixtures(&parse_test_file("f.rs", t).test, integration).len()
        };
        assert_eq!(n(false, "fn f() { let m = FileMetadata { a: 1 }; }"), 1);
        assert_eq!(
            n(
                false,
                "fn f() -> FileMetadata { FileMetadata::new_passthrough(1) }"
            ),
            0
        );
        assert_eq!(
            n(
                false,
                "impl FileMetadata { fn f() {} }\nstruct FileMetadata { a: u8 }"
            ),
            0
        );
        // A literal behind a type alias or a path: seen by its last segment.
        assert_eq!(
            n(false, "fn f() { crate::types::FileMetadata { a: 1 }; }"),
            1
        );
        assert_eq!(n(true, "fn f() { b.metadata(\"dg-tool\", \"x\"); }"), 1);
        // Split over lines: the one-line scan missed it.
        assert_eq!(
            n(
                true,
                "fn f() { b\n    .metadata(\n        \"dg-tool\",\n        v,\n    ); }"
            ),
            1
        );
        assert_eq!(
            n(
                true,
                "fn f() { h.insert(\"x-amz-meta-dg-sha256\", v); xattr::set(p, k, v); }"
            ),
            2
        );
        assert_eq!(
            n(true, "fn f() { b.metadata(\"dg-no-delta\", \"true\"); }"),
            0,
            "the store hint"
        );
        assert_eq!(
            n(false, "fn f() { b.metadata(\"dg-tool\", \"x\"); }"),
            0,
            "unit tests only"
        );
    }

    /// An S3 client in an integration test never retries. The SDK default is
    /// three attempts, so a proxy that answers 500 once looked like a pass
    /// (probe: a server that failed once gave `ok=true hits=2`). Every
    /// `Config::builder()` in `tests/` has `RetryConfig::disabled()` in its
    /// chain, or a `retries-on: <why>` comment for a client that needs them.
    #[test]
    fn test_sdk_clients_do_not_retry() {
        let builder = ["aws_sdk_s3::Config", "::builder()"].concat();
        let mut offenders = Vec::new();
        let mut seen = 0;
        for file in rust_files("tests") {
            let text = std::fs::read_to_string(&file).unwrap();
            let lines: Vec<&str> = text.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                if !line.contains(builder.as_str()) || line.trim_start().starts_with("//") {
                    continue;
                }
                seen += 1;
                let chain = lines[i..lines.len().min(i + 14)].join("\n");
                let chain = chain.split(".build()").next().unwrap_or("");
                let off = ["RetryConfig", "::disabled()"].concat();
                if !chain.contains(off.as_str()) && !chain.contains("retries-on:") {
                    offenders.push(format!("{}:{}", rel_path(&file), i + 1));
                }
            }
        }
        assert!(seen >= 5, "scan found the test clients ({seen})");
        assert!(
            offenders.is_empty(),
            "an integration-test S3 client must disable retries \
             (`.retry_config(aws_sdk_s3::config::retry::RetryConfig::disabled())`), or say why \
             in a `retries-on:` comment: {offenders:?}"
        );
    }

    /// CI exports `MINIO_ENDPOINT` only after it starts MinIO, and sets
    /// `DGP_TEST_REQUIRE_MINIO=1` on the next line: without it a dead MinIO
    /// turns every S3 test into a skipped pass.
    #[test]
    fn test_ci_requires_minio_wherever_it_starts_it() {
        let mut checked = 0;
        for entry in std::fs::read_dir(root().join(".github/workflows")).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("yml") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let lines: Vec<&str> = text.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                if line.contains("MINIO_ENDPOINT=") && line.contains("GITHUB_ENV") {
                    checked += 1;
                    let next = lines.get(i + 1).copied().unwrap_or("");
                    assert!(
                        next.contains("DGP_TEST_REQUIRE_MINIO=1") && next.contains("GITHUB_ENV"),
                        "{}:{}: export DGP_TEST_REQUIRE_MINIO=1 right after MINIO_ENDPOINT",
                        path.display(),
                        i + 1
                    );
                }
            }
        }
        assert!(checked >= 4, "scan found the MinIO steps ({checked})");
    }

    /// A fault test that injects its fault with mode bits (`0o000`, `0o5xx`,
    /// `0o4xx`) proves nothing as root, and CI runs as root. Such a test
    /// reports the skip through `root_skip` (`tests/common/server.rs`, or
    /// `crate::test_root` in the lib), which fails under
    /// `DGP_TEST_REQUIRE_NONROOT`, so the non-root CI job turns a silent
    /// skip into a red test. A hand-written `return` after `geteuid()` is
    /// the silent skip; only the two helpers may call `geteuid`.
    #[test]
    fn test_mode_bit_fault_tests_skip_through_root_skip() {
        let geteuid = ["get", "euid("].concat();
        let helpers = ["src/test_root.rs", "tests/common/server.rs"];
        let denies =
            ["0o000", "0o400", "0o444", "0o500", "0o555"].map(|m| ["from_mode(", m, ")"].concat());
        let mut files = rust_files("tests");
        let lib_files = rust_files("src");
        let test_files = out_of_line_test_modules(&lib_files);
        let mut offenders = Vec::new();
        let mut faulting = 0;
        let mut scanned: Vec<(String, String)> = Vec::new();
        files.retain(|f| f.extension().is_some_and(|e| e == "rs"));
        for f in &files {
            scanned.push((rel_path(f), std::fs::read_to_string(f).unwrap()));
        }
        for f in &lib_files {
            let text = std::fs::read_to_string(f).unwrap();
            let body = if test_files.contains(f) {
                text.clone()
            } else {
                test_module_lines(&text)
                    .into_iter()
                    .map(|(_, l)| l)
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            scanned.push((rel_path(f), body));
        }
        for (rel, body) in &scanned {
            let uses_root_skip = body.contains("root_skip(");
            for (n, line) in body.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                if line.contains(geteuid.as_str()) && !helpers.contains(&rel.as_str()) {
                    offenders.push(format!("{rel}:{}: call root_skip, not geteuid", n + 1));
                }
                if denies.iter().any(|d| line.contains(d.as_str())) {
                    faulting += 1;
                    if !uses_root_skip {
                        offenders
                            .push(format!("{rel}:{}: mode-bit fault without root_skip", n + 1));
                    }
                }
            }
        }
        assert!(
            faulting >= 6,
            "scan found the mode-bit fault tests ({faulting})"
        );
        assert!(offenders.is_empty(), "{offenders:#?}");
    }

    /// Every test that reports a root skip runs in the non-root CI job
    /// (`test-nonroot` in ci.yml), where `DGP_TEST_REQUIRE_NONROOT=1` turns
    /// the skip into a failure. A new `root_skip("name")` not listed there
    /// would skip silently in CI forever.
    #[test]
    fn test_every_root_skip_runs_in_the_nonroot_ci_job() {
        let ci = std::fs::read_to_string(root().join(".github/workflows/ci.yml")).unwrap();
        let job = ci
            .split("\n  test-nonroot:")
            .nth(1)
            .and_then(|rest| rest.split("\n  test-delta:").next())
            .expect("ci.yml has the test-nonroot job");
        assert!(
            job.contains("DGP_TEST_REQUIRE_NONROOT=1"),
            "the job sets the flag"
        );
        let call = regex_lite::Regex::new(r#"root_skip\(\s*"([A-Za-z0-9_]+)""#).unwrap();
        let mut names = Vec::new();
        for file in rust_files("tests").into_iter().chain(rust_files("src")) {
            let rel = rel_path(&file);
            if rel == "src/lib.rs" {
                continue;
            }
            let text = std::fs::read_to_string(&file).unwrap();
            for c in call.captures_iter(&text) {
                names.push((rel.clone(), c[1].to_string()));
            }
        }
        assert!(names.len() >= 6, "scan found the root skips ({names:?})");
        let missing: Vec<_> = names
            .iter()
            .filter(|(_, n)| !job.contains(n.as_str()))
            .collect();
        assert!(
            missing.is_empty(),
            "add these tests to the test-nonroot job in ci.yml: {missing:?}"
        );
    }

    /// Test fixtures come from the real write path or a recorded fixture,
    /// never from a hand-built shape production does not emit: such
    /// fixtures kept C1 (open-access only), D14 (schema gate) and D17
    /// (backfill) green. So test code (src test modules and tests/) holds
    /// no `FileMetadata { .. }` struct literal (use a `FileMetadata::new_*`
    /// constructor or the engine), and integration tests do not hand-write
    /// DG metadata onto stored objects (S3 user metadata, headers, xattrs).
    ///
    /// Allowed: a test whose subject IS a foreign or corrupt shape, and the
    /// `dg-no-delta` store hint (not stored).
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
        let mut offenders = Vec::new();
        let mut allowed_hits = std::collections::BTreeSet::new();
        for src in crate::source_scan::ast::src()
            .iter()
            .chain(crate::source_scan::ast::tests())
        {
            let integration = src.rel.starts_with("tests/");
            for (n, what) in hand_built_fixtures(&src.test, integration) {
                if ALLOWED.iter().any(|(f, _)| *f == src.rel) {
                    allowed_hits.insert(src.rel.clone());
                } else {
                    offenders.push(format!("{}:{n}: {what}", src.rel));
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

    /// A lockout answer names the wait (`Blocked`: 429, `Retry-After`, the
    /// reason). A guard site that drops it with `Err(_)` sends a bare page.
    #[test]
    fn a_rate_limit_guard_site_keeps_the_lockout_answer() {
        let needle = ["RateLimitGuard::", "enter"].concat();
        let dropped = ["Err(", "_)"].concat();
        let files = rust_files("src");
        let (mut sites, mut offenders) = (0, Vec::new());
        for file in files {
            let text = std::fs::read_to_string(&file).unwrap();
            let lines: Vec<&str> = text.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                if !line.contains(needle.as_str()) || line.trim_start().starts_with("//") {
                    continue;
                }
                sites += 1;
                let window = &lines[i..(i + 15).min(lines.len())];
                if window.iter().any(|l| l.contains(dropped.as_str())) {
                    offenders.push(format!("{}:{}", file.display(), i + 1));
                }
            }
        }
        assert!(sites >= 8, "scan found the guard sites ({sites})");
        assert!(
            offenders.is_empty(),
            "answer a lockout with the Blocked value (into_response / \
             into_html_response), not a hand-written page:\n{}",
            offenders.join("\n")
        );
    }

    /// Unit tests do not read the process environment. A test that reads it
    /// passes or fails (or skips itself) by what the runner exports: the
    /// nightly job used to set `DGP_BACKEND_ALLOW_LOCAL` for the whole job,
    /// and a lib test skipped itself there. Inject the env instead (the
    /// `*_from(env: EnvLookup)` pattern, e.g. `RuntimeTuning::from_env`).
    ///
    /// Allowed: files whose tests test env handling itself, serialised on a
    /// lock, plus two reads that are not config.
    #[test]
    fn test_modules_do_not_read_process_env() {
        const ALLOWED: [(&str, &str); 8] = [
            (
                "src/test_root.rs",
                "reads DGP_TEST_REQUIRE_NONROOT (read-only flag of the non-root CI job)",
            ),
            (
                "src/api/admin/auth.rs",
                "cookie-flag env parsing, under LOCK",
            ),
            (
                "src/storage/s3/tests.rs",
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
                "src/config/tests/general.rs",
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
        let mut offenders = Vec::new();
        let mut allowed_hits = std::collections::BTreeSet::new();
        for src in crate::source_scan::ast::src() {
            for (n, what) in env_reads(&src.test, false) {
                if ALLOWED.iter().any(|(f, _)| *f == src.rel) {
                    allowed_hits.insert(src.rel.clone());
                } else {
                    offenders.push(format!("{}:{n}: {what}", src.rel));
                }
            }
        }
        assert!(
            crate::source_scan::ast::src()
                .iter()
                .any(|s| !s.test.calls.is_empty()),
            "scan found test code"
        );
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

    /// `(line, what)` of every read of the process environment in `f`: a
    /// path that ends in `env::var` / `env::var_os` / `env::vars`, a use of
    /// one of those (`use std::env::var;`, with or without `as`), the
    /// `env_bool` / `env_parse` / `env_parse_with_default` helpers, and
    /// `config::process_env`. A path anywhere counts, not only a call: the
    /// old scan looked for the text `env::var(` on one line, so `use
    /// std::env::var; var("X")` and a split call passed.
    fn env_reads(f: &crate::source_scan::ast::Facts, lookups: bool) -> Vec<(usize, String)> {
        let mut out: Vec<(usize, String)> = f
            .paths
            .iter()
            .filter(|p| {
                let n = p.segs.len();
                let last = p.segs.last().map(String::as_str).unwrap_or("");
                (n >= 2 && p.segs[n - 2] == "env" && matches!(last, "var" | "var_os" | "vars"))
                    || matches!(last, "env_bool" | "env_parse" | "env_parse_with_default")
                    || (lookups && last == "lookup_parse")
                    || (n >= 2 && p.segs[n - 2] == "config" && last == "process_env")
            })
            .map(|p| (p.line, p.segs.join("::")))
            .collect();
        out.sort();
        out.dedup();
        out
    }

    #[test]
    fn the_env_guard_sees_imports_and_split_calls() {
        use crate::source_scan::ast::parse;
        let n = |t: &str| env_reads(&parse("f.rs", t).prod, false).len();
        assert_eq!(n("fn f() { std::env::var(\"X\"); }"), 1);
        assert_eq!(n("fn f() { env_bool(\"X\", false); }"), 1);
        // Bypasses of the one-line text scan.
        assert_eq!(
            n("use std::env::var as v;\nfn f() { v(\"X\"); }"),
            1,
            "alias"
        );
        assert_eq!(
            n("use std::env::{self, var};\nfn f() { var(\"X\"); }"),
            1,
            "group import"
        );
        assert_eq!(n("fn f() { std::env\n    ::var(\"X\"); }"), 1, "split");
        assert_eq!(n("fn f() { crate::config::process_env(\"X\"); }"), 1);
        assert_eq!(n("fn f() { let s = \"env::var(\"; // env::var(\n}"), 0);
        assert_eq!(n("fn f() { other::var(1); config::load(); }"), 0);
    }

    /// P7.1: the request principal is an `Arc<AuthenticatedUser>` in the
    /// request extensions, read and written only through
    /// `iam::request_principal` / `iam::insert_request_principal`. A reader
    /// that names another type (`get::<AuthenticatedUser>()`) finds nothing,
    /// and "no principal" means "not checked" to the authorization
    /// middleware and the s3s identity hook. Production and test code alike:
    /// a test that read the wrong slot compared nothing.
    #[test]
    fn the_request_principal_slot_has_one_reader_and_one_writer() {
        let mut bad = Vec::new();
        for src in crate::source_scan::ast::src()
            .iter()
            .chain(crate::source_scan::ast::tests())
        {
            if src.rel == "src/iam/types.rs" {
                continue;
            }
            for f in [&src.prod, &src.test] {
                for n in typed_principal_slot_uses(f) {
                    bad.push(format!("{}:{n}", src.rel));
                }
            }
        }
        assert!(
            bad.is_empty(),
            "read the principal through iam::request_principal:\n{}",
            bad.join("\n")
        );
    }

    /// Lines of `get::<..AuthenticatedUser..>` / `get_mut` / `remove` /
    /// `insert` calls in `f`: a typed access of the extensions slot that
    /// names the wrong type. The turbofish may hold a path
    /// (`crate::iam::AuthenticatedUser`), sit on its own line, or be an
    /// `Arc<..>` of it. The old scan was a regex over one line.
    fn typed_principal_slot_uses(f: &crate::source_scan::ast::Facts) -> Vec<usize> {
        f.calls
            .iter()
            .filter(|c| {
                matches!(
                    c.segs.last().unwrap_or(&c.name).as_str(),
                    "get" | "get_mut" | "remove" | "insert"
                )
            })
            .filter(|c| c.generics.iter().any(|g| g == "AuthenticatedUser"))
            .map(|c| c.line)
            .collect()
    }

    #[test]
    fn the_principal_slot_guard_sees_paths_and_split_turbofish() {
        use crate::source_scan::ast::parse;
        let n = |t: &str| typed_principal_slot_uses(&parse("f.rs", t).prod).len();
        assert_eq!(
            n("fn f() { req.extensions.get::<AuthenticatedUser>(); }"),
            1
        );
        assert_eq!(
            n("fn f() { cx.extensions_mut().get::<crate::iam::AuthenticatedUser>(); }"),
            1
        );
        // A split turbofish and an Arc: the one-line regex passed both.
        assert_eq!(
            n("fn f() { e.get::<\n    std::sync::Arc<AuthenticatedUser>,\n>(); }"),
            1
        );
        assert_eq!(
            n("fn f() { crate::iam::request_principal(&req.extensions); }"),
            0
        );
        assert_eq!(n("fn f() { req.extensions.get::<Other>(); }"), 0);
    }

    /// P7.1: a request never deep-copies a principal. The IAM index builds
    /// one per user (`IamIndex::principal`) and the bootstrap principal is
    /// built once per key (`AuthenticatedUser::bootstrap_shared`); only the
    /// index build converts an `IamUser`.
    #[test]
    fn requests_share_principals_never_build_them() {
        let mut bad = Vec::new();
        for src in crate::source_scan::ast::src() {
            for (n, what) in principal_builds(src) {
                bad.push(format!("{}:{n}: {what}", src.rel));
            }
        }
        assert!(
            bad.is_empty(),
            "per-request principal build:\n{}",
            bad.join("\n")
        );
    }

    /// `(line, what)` of the builds of an `AuthenticatedUser` in `src`
    /// outside their one home: `AuthenticatedUser::from(..)` (home
    /// `src/iam/mod.rs`) and `::bootstrap(..)` (home `src/iam/types.rs`).
    /// A path call under any name the type was imported as (`use .. as AU`),
    /// split over lines, or as `<AuthenticatedUser as From<_>>::from`, counts;
    /// the old scan took the text `AuthenticatedUser::from(` on one line.
    fn principal_builds(src: &crate::source_scan::ast::Source) -> Vec<(usize, String)> {
        let f = &src.prod;
        let mut names = vec!["AuthenticatedUser".to_string()];
        names.extend(
            f.paths
                .iter()
                .filter(|p| p.segs.last().is_some_and(|s| s == "AuthenticatedUser"))
                .filter_map(|p| p.alias.clone()),
        );
        let mut out = Vec::new();
        for c in f.calls.iter().filter(|c| !c.method && c.segs.len() >= 2) {
            let ty = &c.segs[c.segs.len() - 2];
            let leaf = &c.segs[c.segs.len() - 1];
            if !names.contains(ty) {
                continue;
            }
            let home = match leaf.as_str() {
                "from" => "src/iam/mod.rs",
                "bootstrap" => "src/iam/types.rs",
                _ => continue,
            };
            if src.rel != home {
                out.push((c.line, format!("{ty}::{leaf}(..)")));
            }
        }
        out
    }

    #[test]
    fn the_principal_guard_sees_aliases_and_qualified_forms() {
        use crate::source_scan::ast::parse;
        let n = |rel: &str, t: &str| principal_builds(&parse(rel, t)).len();
        assert_eq!(n("src/x.rs", "fn f() { AuthenticatedUser::from(u); }"), 1);
        assert_eq!(
            n("src/iam/mod.rs", "fn f() { AuthenticatedUser::from(u); }"),
            0
        );
        assert_eq!(
            n(
                "src/iam/mod.rs",
                "fn f() { AuthenticatedUser::bootstrap(k); }"
            ),
            1
        );
        // Bypasses of the old one-line text scan: a line break, an alias,
        // the qualified-trait form, a full path.
        assert_eq!(
            n("src/x.rs", "fn f() { AuthenticatedUser\n    ::from(u); }"),
            1
        );
        assert_eq!(
            n(
                "src/x.rs",
                "use crate::iam::AuthenticatedUser as AU;\nfn f() { AU::from(u); }"
            ),
            1
        );
        assert_eq!(
            n(
                "src/x.rs",
                "fn f() { <AuthenticatedUser as From<IamUser>>::from(u); }"
            ),
            1
        );
        assert_eq!(
            n(
                "src/x.rs",
                "fn f() { crate::iam::AuthenticatedUser::from(u); }"
            ),
            1
        );
        assert_eq!(
            n("src/x.rs", "fn f() { // AuthenticatedUser::from(u)\n }"),
            0
        );
    }

    /// Review B5: a permission's condition block is parsed in ONE place,
    /// `iam::permissions::parse_conditions`, which save-time validation and
    /// the policy build share; a second parser would drift from it.
    #[test]
    fn condition_blocks_are_parsed_by_the_one_rule() {
        let mut bad = Vec::new();
        for src in crate::source_scan::ast::src() {
            for (n, func) in condition_block_parsers(src) {
                if !(src.rel == "src/iam/permissions.rs" && func == "parse_conditions") {
                    bad.push(format!("{}:{n} (in fn {func})", src.rel));
                }
            }
        }
        assert!(
            bad.is_empty(),
            "parse an iam-rs ConditionBlock only in iam::permissions::parse_conditions:\n{}",
            bad.join("\n")
        );
    }

    /// `(line, enclosing fn)` of every deserialisation into an iam-rs
    /// `ConditionBlock` in `src`: a `from_value`/`from_str`/`from_slice`/
    /// `from_reader`/`parse`/`deserialize` call that names the type in its
    /// turbofish, or any such call in a fn that spells the type (a typed
    /// `let`). The old scan counted the text `ConditionBlock>(` per file and
    /// allowed one, so a second parser in `permissions.rs` or a typed `let`
    /// elsewhere passed.
    fn condition_block_parsers(src: &crate::source_scan::ast::Source) -> Vec<(usize, String)> {
        const PARSE: [&str; 6] = [
            "from_value",
            "from_str",
            "from_slice",
            "from_reader",
            "parse",
            "deserialize",
        ];
        let f = &src.prod;
        let spells = |func: usize| {
            f.paths
                .iter()
                .any(|p| p.func == func && p.segs.last().is_some_and(|s| s == "ConditionBlock"))
        };
        f.calls
            .iter()
            .filter(|c| PARSE.contains(&c.segs.last().unwrap_or(&c.name).as_str()))
            .filter(|c| c.generics.iter().any(|g| g == "ConditionBlock") || spells(c.func))
            .map(|c| (c.line, src.fn_name(c.func).to_string()))
            .collect()
    }

    #[test]
    fn the_condition_block_guard_sees_a_second_parser() {
        use crate::source_scan::ast::parse;
        let n = |t: &str| condition_block_parsers(&parse("f.rs", t)).len();
        assert_eq!(
            n("fn parse_conditions() { serde_json::from_value::<iam_rs::ConditionBlock>(v); }"),
            1
        );
        // Bypasses of the text count: a typed `let` and a split turbofish.
        assert_eq!(
            n("fn other() { let b: ConditionBlock = serde_json::from_value(v).unwrap(); }"),
            1
        );
        assert_eq!(
            n("fn other() { serde_json::from_str::<\n    iam_rs::ConditionBlock,\n>(s); }"),
            1
        );
        // The type named without a parse (an evaluation) is not a parser.
        assert_eq!(n("fn eval(b: &ConditionBlock) { b.len(); }"), 0);
    }

    /// Review B2: a rate-limit success clears only the account whose
    /// secret the same fn proved: `record_proven()` follows an
    /// `enter_with_account(` and a secret check in its fn. Nothing clears a
    /// per-IP count (`RateLimiter::record_success` is gone).
    #[test]
    fn a_rate_limit_success_follows_a_proven_secret() {
        let mut sites = 0;
        let mut bad = Vec::new();
        for src in crate::source_scan::ast::src() {
            if src.rel == "src/rate_limiter.rs" {
                continue;
            }
            for c in src.prod.calls_named("record_proven") {
                sites += 1;
                if !proven_before(&src.prod, c) {
                    bad.push(format!("{}:{}", src.rel, c.line));
                }
            }
        }
        assert!(sites >= 3, "the scan found the proven sign-ins ({sites})");
        assert!(
            bad.is_empty(),
            "record_proven() without an account guard and a secret check before it in its fn: {bad:?}"
        );
    }

    /// Is there, in the fn of `c` and before it, an `enter_with_account(..)`
    /// AND a secret check (`bcrypt::verify`, `iam_user_secret_valid`,
    /// `probe_key`)? A mention in a comment or a string is no call, and a
    /// call in an earlier fn is no proof (the old scan took the text between
    /// the last `fn ` and the call).
    fn proven_before(
        f: &crate::source_scan::ast::Facts,
        c: &crate::source_scan::ast::Call,
    ) -> bool {
        let before = |name: &str| {
            f.calls_named(name)
                .any(|o| o.func == c.func && o.pos < c.pos)
        };
        before("enter_with_account")
            && (f.calls.iter().any(|o| {
                o.func == c.func
                    && o.pos < c.pos
                    && o.segs.len() >= 2
                    && o.segs[o.segs.len() - 2] == "bcrypt"
                    && o.segs[o.segs.len() - 1] == "verify"
            }) || before("iam_user_secret_valid")
                || before("probe_key"))
    }

    #[test]
    fn the_proven_secret_guard_needs_the_checks_in_the_same_fn() {
        use crate::source_scan::ast::parse;
        let n = |t: &str| {
            let s = parse("f.rs", t);
            s.prod
                .calls_named("record_proven")
                .filter(|c| !proven_before(&s.prod, c))
                .count()
        };
        let ok = "fn a() { g.enter_with_account(x); bcrypt::verify(p, h); g.record_proven(); }";
        assert_eq!(n(ok), 0);
        assert_eq!(
            n("fn a() { g.enter_with_account(x); g.record_proven(); }"),
            1,
            "no secret check"
        );
        // The checks sit in the fn above: the text between `fn ` and the call held none.
        assert_eq!(
            n("fn a() { g.enter_with_account(x); bcrypt::verify(p, h); }\nfn b() { g.record_proven(); }"),
            1
        );
        // A comment that names the checks.
        assert_eq!(
            n("fn a() { // enter_with_account( bcrypt::verify(\n g.record_proven(); }"),
            1
        );
    }

    /// E10/F8: an HTTP handler that checks a secret (`bcrypt::verify`,
    /// `iam_user_secret_valid`, `probe_key`) enters a `RateLimitGuard`
    /// BEFORE the check, in the same fn. A new password oracle without a
    /// lockout fails here. The check fns themselves are not handlers.
    #[test]
    fn every_secret_check_in_a_handler_enters_a_rate_limit_guard_first() {
        let checks = ["bcrypt::verify(", "iam_user_secret_valid(", "probe_key("];
        let mut sites = 0;
        for (file, text) in crate::source_scan::prod_sources("src/api") {
            let prod = crate::source_scan::prod_text(&text);
            for check in checks {
                for (at, _) in prod.match_indices(check) {
                    let fn_start = prod[..at].rfind("fn ").unwrap_or(0);
                    let head = &prod[fn_start..at];
                    if head.trim_end() == "fn" {
                        continue; // the definition of the check fn
                    }
                    sites += 1;
                    assert!(
                        head.contains("RateLimitGuard::enter"),
                        "{file}: `{check}` without a RateLimitGuard before it in its fn"
                    );
                }
            }
        }
        assert!(sites >= 6, "the scan found the secret checks ({sites})");
    }

    /// E10/F35: `triggered_by` is a display label (a user name, an OIDC name
    /// claim: free text). No admin handler decides ownership by comparing it;
    /// they compare the stored principal (`maintenance::delete::may_cancel`).
    #[test]
    fn no_handler_authorises_by_a_display_name() {
        let mut offenders = Vec::new();
        for (file, text) in crate::source_scan::prod_sources("src/api") {
            let prod = crate::source_scan::prod_text(&text);
            for (i, line) in prod.lines().enumerate() {
                let compares = line.contains("==") || line.contains("!=");
                if compares && (line.contains("triggered_by") || line.contains(".label()")) {
                    offenders.push(format!("{file}:{}: {}", i + 1, line.trim()));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "authorise by principal, not by display label:\n{}",
            offenders.join("\n")
        );
    }

    /// E10/F35, the page side: the browser shows Cancel from the server's
    /// `may_cancel`. It never compares `triggered_by` (a display label) with
    /// the name of the signed-in user.
    #[test]
    fn the_ui_never_authorises_by_a_display_name() {
        let mut offenders = Vec::new();
        for ext in ["ts", "tsx"] {
            for path in crate::source_scan::files("demo/s3-browser/ui/src", ext) {
                let file = crate::source_scan::rel(&path);
                if file.contains("/__tests__/") || file.contains(".test.") {
                    continue;
                }
                for (i, line) in crate::source_scan::read(&file).lines().enumerate() {
                    let compares = line.contains("==") || line.contains("!=");
                    let names_a_user = line.contains("identity")
                        || line.contains("user.name")
                        || line.contains("canCancel");
                    if line.contains("triggered_by") && compares && names_a_user {
                        offenders.push(format!("{file}:{}: {}", i + 1, line.trim()));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "decide a cancel from the server's `may_cancel`, not from a name:\n{}",
            offenders.join("\n")
        );
    }

    /// E10/F54: an outbound HTTP client that an operator-supplied URL can
    /// reach comes from ONE builder, `security::guarded_reqwest_builder`:
    /// the SSRF resolver, no redirects, and no system proxy (a proxy would
    /// resolve the target itself, so the connect-time address check would
    /// never run). Nothing else builds a reqwest client, and a failed build
    /// is an error, never a default client.
    #[test]
    fn outbound_http_clients_come_from_the_one_guarded_builder() {
        // Clients that no operator-supplied URL reaches, with the reason.
        const UNGUARDED: [(&str, &str); 2] = [
            (
                "src/cli/config.rs",
                "the admin CLI signs in to the proxy URL the operator typed on the command line",
            ),
            (
                "src/cli/engine_factory.rs",
                "a 3 s health probe of the endpoint of the CLI verb the operator ran, before it connects",
            ),
        ];
        const CONSTRUCTORS: [&str; 6] = [
            "reqwest::Client::builder(",
            "reqwest::ClientBuilder::new(",
            "reqwest::Client::new(",
            "reqwest::Client::default(",
            "Client::builder(",
            ".dns_resolver(",
        ];
        let mut users = 0;
        for (file, text) in crate::source_scan::prod_sources("src") {
            let prod = crate::source_scan::prod_text(&text);
            if prod.contains("guarded_reqwest_") && file != "src/security.rs" {
                users += 1;
                // A `.build()` followed by a default: the failed build
                // becomes a client without the resolver.
                for (at, _) in prod.match_indices(".build()") {
                    let after: String = prod[at + 8..]
                        .chars()
                        .filter(|c| !c.is_whitespace())
                        .take(40)
                        .collect();
                    assert!(
                        !after.starts_with(".unwrap_or_default()")
                            && !after.starts_with(".ok().unwrap_or_default()")
                            && !after.starts_with(".unwrap_or_else(|_|")
                            && !after.starts_with(".unwrap_or_else(|e|")
                            && !after.starts_with(".ok()"),
                        "{file}: a failed client build falls back to a default client"
                    );
                }
            }
            if file == "src/security.rs" || UNGUARDED.iter().any(|(f, _)| *f == file) {
                continue;
            }
            for c in CONSTRUCTORS {
                assert!(
                    !prod.contains(c),
                    "{file} builds an HTTP client with `{c}`: use \
                     `security::guarded_reqwest_builder`, or list the file in UNGUARDED with a reason"
                );
            }
        }
        assert!(users >= 2, "the scan found the guarded clients ({users})");
        // The one builder keeps its three guards.
        let security = crate::source_scan::prod_text(&crate::source_scan::read("src/security.rs"));
        let at = security
            .find("pub fn guarded_reqwest_builder")
            .expect("the guarded builder exists");
        let builder: String = security[at..].chars().take(900).collect();
        for part in [".no_proxy()", ".redirect(", ".dns_resolver("] {
            assert!(builder.contains(part), "the guarded builder lost `{part}`");
        }
    }

    /// Review A14: every admin path that can remove IAM users decides the
    /// lockout rule (`check_lockout`, or `allow_empty` for the restore)
    /// BEFORE the write, in the same function. A new path that deletes
    /// users without it fails here.
    #[test]
    fn every_user_removing_path_checks_the_lockout_rule_first() {
        let mut sites = 0;
        let mut bad = Vec::new();
        for src in crate::source_scan::ast::src() {
            if !src.rel.starts_with("src/api/") {
                continue;
            }
            for (n, what) in user_removals_without_lockout(src) {
                bad.push(format!("{}:{n}: `{what}`", src.rel));
            }
            sites += user_removal_calls(&src.prod).count();
        }
        assert!(
            sites >= 4,
            "the scan found the user-removing sites ({sites})"
        );
        assert!(
            bad.is_empty(),
            "a user-removing call without a lockout decision before it in its fn:\n{}",
            bad.join("\n")
        );
    }

    fn user_removal_calls(
        f: &crate::source_scan::ast::Facts,
    ) -> impl Iterator<Item = &crate::source_scan::ast::Call> {
        ["delete_user", "reconcile_declarative_iam", "restore_iam"]
            .into_iter()
            .flat_map(|w| f.calls_named(w))
    }

    /// `(line, call)` of the user-removing calls of `src` (`delete_user`,
    /// `reconcile_declarative_iam`, `restore_iam`) with no lockout decision
    /// before them in their fn: a call of `check_lockout` /
    /// `import_lockout_check`, or a use of `allow_empty`. The old scan took
    /// the text between the last `fn ` and the call, so a comment or a
    /// string that named the check passed.
    fn user_removals_without_lockout(
        src: &crate::source_scan::ast::Source,
    ) -> Vec<(usize, String)> {
        let f = &src.prod;
        user_removal_calls(f)
            .filter(|c| {
                let call = |name: &str| {
                    f.calls_named(name)
                        .any(|o| o.func == c.func && o.pos < c.pos)
                };
                let allow = f.paths.iter().any(|p| {
                    p.func == c.func
                        && p.line <= c.line
                        && p.segs.last().is_some_and(|s| s == "allow_empty")
                }) || f
                    .fields
                    .iter()
                    .any(|x| x.func == c.func && x.line <= c.line && x.name == "allow_empty");
                !(call("check_lockout") || call("import_lockout_check") || allow)
            })
            .map(|c| (c.line, c.name.clone()))
            .collect()
    }

    #[test]
    fn the_lockout_guard_ignores_comments_and_earlier_fns() {
        use crate::source_scan::ast::parse;
        let n = |t: &str| user_removals_without_lockout(&parse("src/api/x.rs", t)).len();
        assert_eq!(n("fn a() { check_lockout(&db)?; db.delete_user(1); }"), 0);
        assert_eq!(n("fn a() { db.delete_user(1); }"), 1);
        assert_eq!(n("fn a() { db.restore_iam(z, allow_empty); }"), 0);
        // The text scan accepted a comment or a string that names the check.
        assert_eq!(
            n("fn a() { // check_lockout(\n db.delete_user(1); }"),
            1,
            "comment"
        );
        assert_eq!(
            n("fn a() { let _s = \"check_lockout(\"; db.delete_user(1); }"),
            1,
            "string"
        );
        // The check sits in the fn above.
        assert_eq!(
            n("fn a() { check_lockout(&db); }\nfn b() { db.delete_user(1); }"),
            1
        );
    }

    /// A declarative reconcile writes the `auth_providers` table, and the
    /// login flow reads the in-memory `ExternalAuthManager`. Every admin
    /// file that reconciles must rebuild it, or a provider disabled in YAML
    /// keeps logging users in (B006).
    #[test]
    fn declarative_reconcile_rebuilds_the_live_providers() {
        let mut reconcilers = 0;
        let mut bad = Vec::new();
        for src in crate::source_scan::ast::src() {
            if !src.rel.starts_with("src/api/admin/") {
                continue;
            }
            let f = &src.prod;
            if f.calls_named("reconcile_declarative_iam").next().is_some() {
                reconcilers += 1;
                if f.calls_named("rebuild_external_auth").next().is_none()
                    && f.calls_named("publish_provider_set").next().is_none()
                {
                    bad.push(src.rel.clone());
                }
            }
        }
        assert!(reconcilers >= 2, "the scan found the reconcile call sites");
        assert!(
            bad.is_empty(),
            "{bad:?} reconcile declarative IAM without rebuilding the providers"
        );
    }

    /// A walk of every directory with one request per directory is the HEAD
    /// flood that the savings chip sent on prod: one 404 HEAD per folder of
    /// screenshots. A caller that wants the deltaspaces WITH a reference
    /// lists them with `StorageBackend::list_reference_prefixes` (one listing
    /// of the scope). Only a caller that also needs the directories WITHOUT
    /// a reference calls `list_deltaspaces`.
    #[test]
    fn a_reference_walk_lists_only_the_reference_prefixes() {
        let mut offenders = Vec::new();
        for src in crate::source_scan::ast::src() {
            // The trait, its implementations and their forwarders.
            if src.rel.starts_with("src/storage/") {
                continue;
            }
            for c in src.prod.calls_named("list_deltaspaces") {
                offenders.push(format!("{}:{}", src.rel, c.line));
            }
        }
        assert!(
            offenders.is_empty(),
            "list the deltaspaces that hold a reference with \
             list_reference_prefixes, not every directory:\n{}",
            offenders.join("\n")
        );
    }

    /// The `list_objects(..)` calls of `f` whose last argument (the
    /// `metadata` flag) is not the literal `false`, with their lines. A
    /// definition (`fn list_objects(`), a method of another name
    /// (`bulk_list_objects`) and a mention in a comment are no call. A
    /// variable (`metadata`) or `true` is a breach: the flag must be a
    /// literal `false`, or a reader cannot tell it from a HEAD per object.
    fn list_objects_with_metadata(f: &crate::source_scan::ast::Facts) -> Vec<(usize, String)> {
        f.calls_named("list_objects")
            .filter(|c| c.arg_bools.last() != Some(&Some(false)))
            .map(|c| {
                (
                    c.line,
                    "the last argument is not the literal `false`".to_string(),
                )
            })
            .collect()
    }

    #[test]
    fn the_metadata_listing_guard_reads_multiline_calls() {
        use crate::source_scan::ast::parse;
        let src = "fn list_objects(a: u32, metadata: bool) {}\n\
                   // Cost: walks `engine.list_objects(prefix)` paginated.\n\
                   fn f() {\n\
                   let p = engine.list_objects(\n    b,\n    p,\n    None,\n    1000,\n    \
                   None,\n    true, // metadata=true\n);\n\
                   let q = engine.list_objects(&b, \"\", None, 10, t.as_deref(), false).await;\n\
                   let r = engine.list_objects(b, &f(x, y), None, n, None, metadata);\n\
                   let s = storage.bulk_list_objects(b, p).await;\n\
                   }";
        let got: Vec<usize> = list_objects_with_metadata(&parse("f.rs", src).prod)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(got, [4, 13], "the `true` call and the variable one");
    }

    /// Cockroach scan (ui.md A, limits #5): the savings chip listed with
    /// `metadata=true`, so every folder view sent one HEAD per delta the
    /// metadata cache did not hold. An admin scan reads the sizes from the
    /// listing (the listing-size cache and the listing facts) and HEADs only
    /// what neither knows, through a call that says so. A handler that truly
    /// needs a HEAD per listed object goes in `ALLOWED` with its reason.
    #[test]
    fn admin_scans_never_list_with_metadata() {
        const SCOPE: &[&str] = &["src/api/admin", "src/usage_scanner.rs"];
        const ALLOWED: &[(&str, &str)] = &[];
        let mut offenders = Vec::new();
        for src in crate::source_scan::ast::src() {
            if !SCOPE.iter().any(|s| src.rel.starts_with(s))
                || ALLOWED.iter().any(|(f, _)| *f == src.rel)
            {
                continue;
            }
            for (line, why) in list_objects_with_metadata(&src.prod) {
                offenders.push(format!("{}:{line}: {why}", src.rel));
            }
        }
        assert!(
            offenders.is_empty(),
            "an admin scan lists with metadata (a HEAD per object); read the \
             sizes from the listing instead:\n{}",
            offenders.join("\n")
        );
    }

    /// The production files outside the engine with a per-key engine
    /// delete (`.delete(` / `.delete_if(` whose receiver, on the line or on
    /// the three lines above a chained call, names an engine), and the
    /// sites of the files not in `allowed` (`file:line: code`).
    fn per_key_engine_deletes(
        allowed: &[&str],
    ) -> (std::collections::BTreeSet<String>, Vec<String>) {
        let mut files = std::collections::BTreeSet::new();
        let mut offenders = Vec::new();
        for src in crate::source_scan::ast::src() {
            if src.rel.starts_with("src/deltaglider/engine/") {
                continue;
            }
            for line in engine_key_deletes(&src.prod) {
                files.insert(src.rel.clone());
                if !allowed.contains(&src.rel.as_str()) {
                    offenders.push(format!("{}:{line}", src.rel));
                }
            }
        }
        (files, offenders)
    }

    /// Lines of the `.delete(..)` / `.delete_if(..)` calls in `f` whose
    /// receiver chain names an engine (any field, variable or method with
    /// `engine` in its name), wherever the chain is broken over lines. The
    /// old scan looked at the receiver text on the line, or on the three
    /// lines above when the call started its line.
    fn engine_key_deletes(f: &crate::source_scan::ast::Facts) -> Vec<usize> {
        f.calls
            .iter()
            .filter(|c| c.method && matches!(c.name.as_str(), "delete" | "delete_if"))
            .filter(|c| c.receiver_idents.iter().any(|i| i.contains("engine")))
            .map(|c| c.line)
            .collect()
    }

    #[test]
    fn the_engine_delete_guard_sees_a_long_receiver_chain() {
        use crate::source_scan::ast::parse;
        let n = |t: &str| engine_key_deletes(&parse("f.rs", t).prod).len();
        assert_eq!(n("fn f() { engine.delete(b, k).await; }"), 1);
        assert_eq!(
            n("fn f() { state.engine.load().delete_if(b, k, c).await; }"),
            1
        );
        // The engine is named 6 lines above the call: the 3-line look-back missed it.
        let far = format!(
            "fn f() {{ state\n    .engine{}\n    .delete(b, k); }}",
            "\n    .load()".repeat(6)
        );
        assert_eq!(n(&far), 1, "far receiver");
        assert_eq!(n("fn f() { map.delete(k); cache.delete_if(k, c); }"), 0);
    }

    /// More than one key goes through `DeltaGliderEngine::delete_batch`: it
    /// checks each folder ONCE for a reference.bin to reclaim, runs the
    /// folders in parallel and reports each key as it ends. A per-key
    /// `delete` / `delete_if` in a loop, or in a helper that a loop calls,
    /// lists the folder after every key (one LIST per key, and a cut loop
    /// leaves a folder holding its reference.bin alone). So outside the
    /// engine a per-key engine delete stands only where a request names
    /// one key. (The engine's sweep calls are private to it, so the
    /// compiler refuses them elsewhere.)
    #[test]
    fn deletes_of_many_keys_go_through_delete_batch() {
        // A request that names one key: S3 DeleteObject, and the CLI's
        // `rm s3://b/k` without `-r`.
        const SINGLE_KEY: &[&str] = &["src/s3_adapter_s3s/object.rs", "src/cli/rm.rs"];
        let allowed: Vec<&str> = SINGLE_KEY.to_vec();
        let (files, offenders) = per_key_engine_deletes(&allowed);
        assert!(
            offenders.is_empty(),
            "delete more than one key with DeltaGliderEngine::delete_batch, not a \
             per-key delete:\n{}",
            offenders.join("\n")
        );
        let stale: Vec<&&str> = allowed.iter().filter(|f| !files.contains(**f)).collect();
        assert!(
            stale.is_empty(),
            "these files no longer delete per key; remove them from the allow-list: {stale:?}"
        );
        // The scan sees a real site: the single-key `rm s3://b/k`.
        let without_rm: Vec<&str> = allowed
            .iter()
            .copied()
            .filter(|f| *f != "src/cli/rm.rs")
            .collect();
        let (_, caught) = per_key_engine_deletes(&without_rm);
        eprintln!("without its allow-list entry:\n{}", caught.join("\n"));
        assert!(
            !caught.is_empty() && caught.iter().all(|o| o.starts_with("src/cli/rm.rs:")),
            "{caught:?}"
        );
    }

    /// A replication run checks the backend health gate: it goes through
    /// `replication::run_rule_gated` with a `RunHealthGate`, and defers when
    /// the backend of its source or destination is gated. An ungated run
    /// (`replication::run_rule`) sent its whole volume to a backend that the
    /// health check had closed, which kept the probe failing and the gate
    /// closed for every client (jobs finding 13).
    #[test]
    fn replication_runs_check_the_backend_health_gate() {
        let mut offenders = Vec::new();
        for src in crate::source_scan::ast::src() {
            // The definition and its gated twin.
            if src.rel == "src/replication/worker.rs" {
                continue;
            }
            for n in ungated_replication_runs(src) {
                offenders.push(format!("{}:{n}", src.rel));
            }
        }
        assert!(
            offenders.is_empty(),
            "start a replication run with run_rule_gated and a RunHealthGate:\n{}",
            offenders.join("\n")
        );
    }

    /// Lines where `src` names `replication::run_rule` (any path that ends
    /// in `replication::run_rule`, a `use` or an alias included), or, inside
    /// `src/replication/`, a bare `run_rule`. The old scan wanted the text
    /// `replication::run_rule(` on one line (or `run_rule(` in the module),
    /// so `use crate::replication::run_rule as go; go(..)` passed.
    fn ungated_replication_runs(src: &crate::source_scan::ast::Source) -> Vec<usize> {
        let in_replication = src.rel.starts_with("src/replication/");
        let mut out: Vec<usize> = src
            .prod
            .paths
            .iter()
            .filter(|p| {
                let n = p.segs.len();
                p.segs.last().is_some_and(|s| s == "run_rule")
                    && ((n >= 2 && p.segs[n - 2] == "replication") || (in_replication && p.expr))
            })
            .map(|p| p.line)
            .collect();
        out.extend(
            src.prod
                .calls_named("run_rule")
                .filter(|c| c.method)
                .map(|c| c.line),
        );
        out.sort();
        out.dedup();
        out
    }

    #[test]
    fn the_replication_gate_guard_sees_an_import_and_an_alias() {
        use crate::source_scan::ast::parse;
        let n = |rel: &str, t: &str| ungated_replication_runs(&parse(rel, t)).len();
        assert_eq!(
            n(
                "src/api/x.rs",
                "fn f() { crate::replication::run_rule(a); }"
            ),
            1
        );
        assert_eq!(
            n(
                "src/api/x.rs",
                "fn f() { crate::replication::run_rule_gated(a, g); }"
            ),
            0
        );
        assert_eq!(
            n(
                "src/api/x.rs",
                "use crate::replication::run_rule as go;\nfn f() { go(a); }"
            ),
            1,
            "alias"
        );
        assert_eq!(n("src/replication/y.rs", "fn f() { run_rule(a); }"), 1);
        assert_eq!(
            n("src/api/x.rs", "fn f() { run_rule(a); }"),
            0,
            "another run_rule"
        );
    }

    /// The request path reads no environment variable: a request reads the
    /// config snapshot (`config::RuntimeTuning`, copied into the engine at
    /// build) or a value its owner parsed at construction. `clippy.toml`
    /// refuses `std::env::var` outside `crate::config`; this guard also
    /// catches the `env_parse` helpers, which clippy cannot tell apart from
    /// a boot-time read.
    /// A 204 carries no body. Behind an HTTP/2 proxy, a 204 with a JSON
    /// body (and its `content-length`) is a protocol error, so the browser
    /// fails the request although the server did the work.
    /// Every `If-Match` (and `x-amz-copy-source-if-match`) goes through
    /// `coordination::if_match::IfMatchRequest`, which formats the ETag in
    /// the backend's dialect. Ceph RGW (Hetzner) refuses the correct ETag
    /// quoted on PUT: a raw `.if_match(etag)` with the SDK's quoted ETag
    /// reported every conditional write on it as lost.
    #[test]
    fn if_match_headers_go_through_one_helper() {
        const HELPER: &str = "src/coordination/if_match.rs";
        let mut offenders = Vec::new();
        for src in crate::source_scan::ast::src() {
            if src.rel == HELPER {
                continue;
            }
            for name in ["if_match", "copy_source_if_match"] {
                for c in src.prod.calls_named(name).filter(|c| c.method) {
                    offenders.push(format!("{}:{}: .{name}(..)", src.rel, c.line));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "set If-Match through IfMatchRequest::if_match_etag (the backend's dialect):\n{}",
            offenders.join("\n")
        );
    }

    /// A file body goes to an S3 backend through ONE method
    /// (`put_object_file_with_metadata_fenced_with`), which sends a body
    /// above 5 GiB as a backend multipart upload. Another PUT of a file
    /// body stream would fail above 5 GiB with `EntityTooLarge`: so
    /// `.byte_stream()` is called once in `objects.rs` (the single PUT
    /// behind the switch), once in `mpu_put.rs` (the parts) and nowhere
    /// else in `storage/s3`.
    #[test]
    fn file_bodies_go_through_the_multipart_switch() {
        let mut seen = std::collections::BTreeMap::<String, usize>::new();
        for src in crate::source_scan::ast::src() {
            let n = src
                .prod
                .calls_named("byte_stream")
                .filter(|c| c.method)
                .count();
            if n > 0 {
                seen.insert(src.rel.to_string(), n);
            }
        }
        let expected: std::collections::BTreeMap<String, usize> = [
            ("src/storage/s3/mpu_put.rs".to_string(), 1),
            ("src/storage/s3/objects.rs".to_string(), 1),
        ]
        .into();
        assert_eq!(
            seen, expected,
            "a file body is sent by the single PUT behind the multipart switch (objects.rs) \
             and by the parts of an upload (mpu_put.rs) only"
        );
    }

    /// The S3 requests whose body or whose server-side work grows with the
    /// object (PutObject, UploadPart, CopyObject, UploadPartCopy,
    /// CompleteMultipartUpload), on ANY client in `src/storage/s3`, carry
    /// their own timeouts: the `config_override(..)` that follows the
    /// request in its function names `upload_config`, `part_config` or
    /// `complete_config`. The client's read timeout (60 s) is a deadline for
    /// the whole request, so without them a part, a copy or a Complete that
    /// needs longer never gets its answer. A request that is deliberately
    /// without them is listed in `EXEMPT` with its reason.
    #[test]
    fn size_dependent_requests_carry_their_own_timeouts() {
        const OPS: [&str; 5] = [
            "put_object",
            "upload_part",
            "copy_object",
            "upload_part_copy",
            "complete_multipart_upload",
        ];
        const HELPERS: [&str; 3] = ["upload_config", "part_config", "complete_config"];
        // (file, function, reason): empty or tiny bodies on the plain
        // client, whose 30 s request deadline is the right limit. An empty
        // function name exempts the whole file.
        const EXEMPT: [(&str, &str, &str); 3] = [
            (
                "src/storage/s3/facts.rs",
                "put_facts_object",
                "a listing-facts object: a few bytes",
            ),
            (
                "src/storage/s3/claim.rs",
                "",
                "the claim object of a bucket: a few bytes",
            ),
            (
                "src/storage/s3/mod.rs",
                "put_directory_marker",
                "a directory marker: an empty object",
            ),
        ];
        let mut offenders = Vec::new();
        let mut checked = 0;
        let mut exempted = std::collections::BTreeSet::new();
        for src in crate::source_scan::ast::src() {
            if !src.rel.starts_with("src/storage/s3/") {
                continue;
            }
            let calls = &src.prod.calls;
            for op in calls
                .iter()
                .filter(|c| c.method && OPS.contains(&c.name.as_str()))
            {
                let func = src.fn_name(op.func);
                if let Some((file, f, _)) = EXEMPT
                    .iter()
                    .find(|(file, f, _)| *file == src.rel && (f.is_empty() || *f == func))
                {
                    exempted.insert((*file, *f));
                    continue;
                }
                checked += 1;
                // The next request of the function: the override of this
                // one comes before it.
                let next_op = calls
                    .iter()
                    .filter(|c| {
                        c.method
                            && c.func == op.func
                            && OPS.contains(&c.name.as_str())
                            && c.line > op.line
                    })
                    .map(|c| c.line)
                    .min()
                    .unwrap_or(usize::MAX);
                let named = calls.iter().any(|o| {
                    o.method
                        && o.name == "config_override"
                        && o.func == op.func
                        && o.line >= op.line
                        && o.line < next_op
                        && o.arg_idents_all
                            .iter()
                            .any(|a| HELPERS.contains(&a.as_str()))
                });
                if !named {
                    offenders.push(format!("{}:{}: .{}() in {func}", src.rel, op.line, op.name));
                }
            }
        }
        assert!(
            checked >= 10,
            "the scan saw only {checked} size-dependent requests: it is blind"
        );
        for (file, f, why) in EXEMPT {
            assert!(
                exempted.contains(&(file, f)),
                "the exemption of {file} `{f}` ({why}) matches no request: remove it"
            );
        }
        assert!(
            offenders.is_empty(),
            "a size-dependent request without `.customize().config_override(<upload_config | \
             part_config | complete_config>)`:\n{}",
            offenders.join("\n")
        );
    }

    #[test]
    fn no_content_responses_carry_no_body() {
        let mut offenders = Vec::new();
        for src in crate::source_scan::ast::src() {
            for n in no_content_with_body(&src.prod) {
                offenders.push(format!("{}:{n}", src.rel));
            }
        }
        assert!(
            offenders.is_empty(),
            "a 204 response with a JSON body: {offenders:?} (return StatusCode::NO_CONTENT alone)"
        );
    }

    /// Lines of the tuple responses `(StatusCode::NO_CONTENT, .., Json(..))`
    /// in `f`: a `NO_CONTENT` status with a `Json` body, whatever sits
    /// between them and however the tuple is laid out. The old scan joined
    /// the file's code lines and looked for the text `NO_CONTENT, Json(`.
    fn no_content_with_body(f: &crate::source_scan::ast::Facts) -> Vec<usize> {
        f.tuples
            .iter()
            .filter(|t| {
                t.elems.iter().any(|e| e == "NO_CONTENT") && t.elems.iter().any(|e| e == "Json")
            })
            .map(|t| t.line)
            .collect()
    }

    #[test]
    fn the_no_content_guard_sees_a_header_between_status_and_body() {
        use crate::source_scan::ast::parse;
        let n = |t: &str| no_content_with_body(&parse("f.rs", t).prod).len();
        assert_eq!(
            n("fn f() { (StatusCode::NO_CONTENT, Json(x)).into_response(); }"),
            1
        );
        assert_eq!(
            n("fn f() { (StatusCode::NO_CONTENT, headers, Json(x)); }"),
            1,
            "a header between"
        );
        assert_eq!(
            n("fn f() { (StatusCode::NO_CONTENT,\n    Json(\n x)); }"),
            1,
            "split"
        );
        assert_eq!(
            n("fn f() { StatusCode::NO_CONTENT.into_response(); (StatusCode::OK, Json(x)); }"),
            0
        );
    }

    /// The request path is everything under these roots, so a NEW file there
    /// is guarded by default (the old guard listed the request-path files by
    /// hand, and a new one escaped).
    fn in_request_path(rel: &str) -> bool {
        const ROOTS: &[&str] = &[
            "src/s3_adapter_s3s/",
            "src/api/",
            "src/deltaglider/",
            "src/maintenance/",
        ];
        ROOTS.iter().any(|r| rel.starts_with(r))
    }

    #[test]
    fn a_new_request_path_file_is_in_scope_of_the_env_guard() {
        use crate::source_scan::ast::parse;
        assert!(in_request_path("src/api/handlers/brand_new.rs"));
        assert!(in_request_path("src/deltaglider/engine/brand_new.rs"));
        assert!(!in_request_path("src/cli/cp.rs"));
        let read = "fn f() { crate::config::env_parse_with_default(\"X\", 1); }";
        assert_eq!(
            env_reads(&parse("src/api/handlers/brand_new.rs", read).prod, true).len(),
            1
        );
    }

    /// A re-encrypt replaces the DB file, so it runs only in a process that
    /// holds the state directory lock (a running server keeps its handle on
    /// the old file and would lose every later write). The type carries the
    /// rule where it can: `open_live_db` takes the `DataDirLock`. This guard
    /// names the files that may call each way into a re-encrypt, so a new
    /// caller (a CLI path) has to be added here on purpose, with its lock.
    #[test]
    fn rekey_has_no_caller_outside_a_lock_holder() {
        // (call, files that may contain it outside tests)
        const CALLERS: &[(&str, &[&str])] = &[
            // The re-encrypt itself and the opens that reach it.
            ("rekey_file(", &["src/config_db/mod.rs"]),
            ("rekey_file_hooked(", &["src/config_db/mod.rs"]),
            (
                "open_keyed(",
                &["src/config_db/mod.rs", "src/config_db/jobs_db.rs"],
            ),
            // The config DB opens: the sync download (a running server, which
            // holds the lock) and `open_live_db`.
            (
                "open_with_keys(",
                &["src/config_db/mod.rs", "src/config_db_sync.rs"],
            ),
            // Boot, the recovery promotion and `--set-bootstrap-password`,
            // each with the lock as an argument (the compiler checks that).
            (
                "open_live_db(",
                &["src/config_db_sync.rs", "src/startup.rs"],
            ),
        ];
        for (call, allowed) in CALLERS {
            let name = call.trim_end_matches('(');
            let mut found = 0;
            for src in crate::source_scan::ast::src() {
                let calls = src.prod.calls_named(name).count();
                if calls == 0 {
                    continue;
                }
                found += calls;
                assert!(
                    allowed.contains(&src.rel.as_str()),
                    "{}: `{call}` outside the files that hold the state directory lock \
                     ({allowed:?}). A re-encrypt must run under the lock, and refuse while \
                     a server runs.",
                    src.rel
                );
            }
            assert!(
                found > 0,
                "`{call}` has no caller left: drop it from CALLERS"
            );
        }
    }

    #[test]
    fn request_path_reads_no_env() {
        // A file of the request path that reads the environment on purpose
        // (it runs at boot, at config apply or at engine build, never per
        // request) is named here with its reason.
        const BOOT_TIME: &[(&str, &str)] = &[
            (
                "src/api/admin/auth.rs",
                "DGP_METRICS_BEARER_TOKEN, read once into a OnceLock",
            ),
            (
                "src/api/admin/backup.rs",
                "a restore resolves its ${env:NAME} refs (an admin operation)",
            ),
            (
                "src/api/admin/config/mod.rs",
                "a config apply re-applies the env overrides (an admin operation)",
            ),
            (
                "src/api/admin/config/transition.rs",
                "a config apply checks DGP_CONFIG_DB_KEY (an admin operation)",
            ),
            (
                "src/deltaglider/engine/construction.rs",
                "engine BUILD: warns once that a key is not from env",
            ),
            (
                "src/deltaglider/memory_budget.rs",
                "MemoryBudget::from_env runs once, where the process builds its AppState",
            ),
            (
                "src/deltaglider/spool.rs",
                "SpoolDir::from_env runs once, in startup::claim_environment",
            ),
        ];
        let mut offenders = Vec::new();
        let mut allowed_hits = std::collections::BTreeSet::new();
        let mut scanned = 0;
        for src in crate::source_scan::ast::src() {
            if !in_request_path(&src.rel) {
                continue;
            }
            scanned += 1;
            for (n, what) in env_reads(&src.prod, true) {
                if BOOT_TIME.iter().any(|(f, _)| *f == src.rel) {
                    allowed_hits.insert(src.rel.clone());
                } else {
                    offenders.push(format!("{}:{n}: {what}", src.rel));
                }
            }
        }
        assert!(
            scanned > 40,
            "scan found the request-path files ({scanned})"
        );
        assert!(
            offenders.is_empty(),
            "the request path reads the environment; add the setting to \
             config::RuntimeTuning (or parse it where its owner is built):\n{}",
            offenders.join("\n")
        );
        for (f, why) in BOOT_TIME {
            assert!(
                allowed_hits.contains(*f),
                "{f} ({why}) no longer reads env: drop it from BOOT_TIME"
            );
        }
    }

    /// A stored body (a spool file, relay parts, a ciphertext temp file)
    /// can be any size: the storage and engine code stream it and never
    /// read a whole file into memory. A whole-file read that is bounded
    /// says why on its line or the line above (`// small: <why>`).
    /// `PutBody::into_small_bytes` refuses a file body at run time; this
    /// guard keeps the file reads out of the source.
    /// Why the raw blob accessors of `engine/raw.rs` still read a whole
    /// stored delta or reference into memory.
    const RAW_BLOBS_IN_MEMORY: &str =
        "`get_delta_raw` / `get_reference_raw` / `open_raw` feed the \
         delta fast path of `transfer.rs` (engine-routed copies such as replication) and the \
         re-encrypt job, which copy the stored delta and reference verbatim from memory; moving \
         them to spool files is a follow-up in docs/plan/streaming-ingest.md";

    #[test]
    fn stored_bodies_are_never_heap_loaded() {
        // Known unbounded sites, with the reason they stay: (file,
        // offending lines, why). A stale entry fails too.
        const PENDING: &[(&str, usize, &str)] =
            &[("src/deltaglider/engine/raw.rs", 1, RAW_BLOBS_IN_MEMORY)];
        const SCOPE: &[&str] = &[
            "src/storage",
            "src/deltaglider",
            "src/multipart.rs",
            "src/transfer.rs",
        ];
        // Each needle is split so this guard does not match itself.
        let needles = [
            ["fs::", "read("].concat(),
            ["read_to_", "end"].concat(),
            ["read_to_", "string"].concat(),
            ["try_", "collect"].concat(),
            ["try_fold(", "Vec::new()"].concat(),
            [".con", "cat()"].concat(),
        ];
        let test_files = out_of_line_test_modules(&rust_files("src"));
        let mut offenders: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        for p in SCOPE {
            let files = if p.ends_with(".rs") {
                vec![root().join(p)]
            } else {
                rust_files(p)
            };
            for file in files {
                if test_files.contains(&file) {
                    continue;
                }
                let rel = rel_path(&file);
                let text = std::fs::read_to_string(&file).unwrap();
                let mask = prod_mask(&text);
                let lines: Vec<&str> = text.lines().collect();
                for (i, line) in lines.iter().enumerate() {
                    let marked = line.contains("// small:")
                        || i.checked_sub(1)
                            .is_some_and(|p| lines[p].contains("// small:"));
                    if !mask[i] || marked || line.trim_start().starts_with("//") {
                        continue;
                    }
                    if needles.iter().any(|n| line.contains(n.as_str())) {
                        offenders.entry(rel.clone()).or_default().push(format!(
                            "{rel}:{}: {}",
                            i + 1,
                            line.trim()
                        ));
                    }
                }
            }
        }
        for (file, count, why) in PENDING {
            let found = offenders.remove(*file).unwrap_or_default();
            assert_eq!(
                found.len(),
                *count,
                "{file} ({why}): the pending whole-body reads changed; update PENDING:\n{}",
                found.join("\n")
            );
        }
        assert!(
            offenders.is_empty(),
            "a stored body is read whole into memory; stream it, or mark a \
             bounded read with `// small: <why>`:\n{}",
            offenders
                .into_values()
                .flatten()
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    /// A whole-object read (`engine.retrieve`, the storage `get_object` /
    /// `get_delta` / `get_reference` / `get_passthrough`) holds the whole
    /// object in memory. Outside the storage backends, which implement
    /// them, each one says why its object is bounded: `// small: <why>`
    /// on its line or in the 4 lines above. A large object streams
    /// (`retrieve_stream`, `retrieve_stream_range`, `open_object`).
    #[test]
    fn whole_object_reads_are_marked_small() {
        // Known unbounded sites, with the reason they stay: (file,
        // offending lines, why). A stale entry fails too.
        const PENDING: &[(&str, usize, &str)] = &[
            ("src/deltaglider/engine/raw.rs", 6, RAW_BLOBS_IN_MEMORY),
            (
                "src/deltaglider/engine/rewrite.rs",
                2,
                "the re-encrypt job reads a stored delta and reference whole \
                 (`open_raw`) to rewrite them under the new key; streaming them \
                 through spool files is a follow-up in docs/plan/streaming-ingest.md",
            ),
        ];
        // A stream collected into memory. Split so the guard does not
        // match itself.
        let collect = ["try_", "collect"].concat();
        let fold = ["try_fold(", "Vec::new()"].concat();
        let needles = [
            ".retrieve(",
            ".get_object(",
            ".get_delta(",
            ".get_reference(",
            ".get_passthrough(",
            "open_raw(",
            collect.as_str(),
            fold.as_str(),
        ];
        let files = rust_files("src");
        let test_files = out_of_line_test_modules(&files);
        let mut offenders: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        for file in files {
            let rel = rel_path(&file);
            if test_files.contains(&file) || rel.starts_with("src/storage/") {
                continue;
            }
            let text = std::fs::read_to_string(&file).unwrap();
            let mask = prod_mask(&text);
            let lines: Vec<&str> = text.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                if !mask[i] || line.trim_start().starts_with("//") {
                    continue;
                }
                // The AWS SDK's `client.get_object()` is a request builder.
                let probe = line.replace(".get_object()", "");
                if !needles.iter().any(|n| probe.contains(n)) {
                    continue;
                }
                let marked = lines[i.saturating_sub(4)..=i]
                    .iter()
                    .any(|l| l.contains("// small:"));
                if !marked {
                    offenders.entry(rel.clone()).or_default().push(format!(
                        "{rel}:{}: {}",
                        i + 1,
                        line.trim()
                    ));
                }
            }
        }
        for (file, count, why) in PENDING {
            let found = offenders.remove(*file).unwrap_or_default();
            assert_eq!(
                found.len(),
                *count,
                "{file} ({why}): the pending whole-object reads changed; update PENDING:\n{}",
                found.join("\n")
            );
        }
        assert!(
            offenders.is_empty(),
            "a whole-object read without a bound; stream the object, or say why it \
             is small with `// small: <why>`:\n{}",
            offenders
                .into_values()
                .flatten()
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}
