// SPDX-License-Identifier: BUSL-1.1

//! `${env:NAME}` / `${env:NAME:-default}` config expansion (pre-parse).
//!
//! In-process replacement for an external `envsubst` step: operators ship a
//! secret-free config with `${env:...}` placeholders and inject the values as
//! env vars. Scoped strictly to the `env:` namespace so other `${ns:name}`
//! forms (notably `${iam:username}`) pass through untouched.

use super::ConfigError;

/// Expand `${env:NAME}` and `${env:NAME:-default}` references in a config file
/// against the process environment, BEFORE the YAML is parsed. This is the
/// in-process replacement for an external `envsubst` step — operators ship a
/// secret-free config with `${env:...}` placeholders and inject the values as
/// env vars.
///
/// **The `env:` prefix is mandatory and deliberate.** Other `${ns:...}`
/// namespaces resolve at different times — notably `${iam:username}` /
/// `${iam:access_key_id}` (runtime IAM permission templates, substituted
/// per-request at auth time — NOT here). Scoping config expansion to `${env:..}`
/// keeps the namespaces from colliding: every `${ns:name}` declares when it
/// resolves, and ANY non-`env:` `${...}` (including `${iam:...}` and a bare
/// `${foo}`) passes through this expander untouched.
///
/// Rules (a small, predictable subset — NOT a shell):
/// - `${env:NAME}` → the value of env var `NAME`; **error** if unset (fail loud,
///   never silently leave a hole — a blank secret is worse than a clear error).
/// - `${env:NAME:-default}` → `NAME` if set & non-empty, else the literal
///   `default` (which may itself be empty: `${env:NAME:-}`).
/// - `${anything-else}` (no `env:` prefix) → left VERBATIM (IAM templates etc.).
/// - `$$` → a literal `$` escape.
/// - a bare `$` not followed by `{` or `$` is left untouched (so `$2b$10$...`
///   bcrypt hashes, regex `$`, etc. pass through verbatim).
///
/// `VAR` must match `[A-Za-z_][A-Za-z0-9_]*`. An unterminated `${` or an empty
/// `${}` is a [`ConfigError::BadEnvRef`]. Pure except for the env lookup, which
/// is injected via `lookup` so the whole thing is unit-testable.
///
/// **Substituted values are spliced as raw pre-parse text and are NOT YAML
/// -escaped.** A value containing a newline or control char is rejected
/// ([`ConfigError::UnsafeEnvValue`]) because it could restructure the document;
/// values with YAML indicators (leading `@`, `*`, `:` `, ` etc.) parse as
/// intended only when the field is quoted in the template (`key: "${env:X}"`).
pub fn expand_env_vars(input: &str) -> Result<String, ConfigError> {
    expand_env_doc_with(input, |name| std::env::var(name).ok())
}

/// True if `s` is exactly one `${env:NAME}` / `${env:NAME:-default}`
/// reference. Redactors keep such values: a reference is not a secret, and
/// stripping it would break the IaC round-trip that
/// [`Config::with_env_refs_reinserted`](super::Config::with_env_refs_reinserted) exists to enable.
pub fn is_env_ref(s: &str) -> bool {
    s.starts_with("${env:") && s.ends_with('}') && !s[2..s.len() - 1].contains('}')
}

/// [`expand_env_vars`] that additionally RECORDS which `${env:NAME}` refs
/// resolved from the environment, as `name → resolved value`. The map is the
/// provenance that lets persist/export re-emit the refs instead of the
/// materialized secrets (see [`Config::env_refs`](super::Config::env_refs)). Refs satisfied by their
/// `:-default` (var unset/empty) are NOT recorded — the literal default came
/// from the file, and re-emitting it as a bare `${env:NAME}` would make the
/// next load fail where the original template defaulted.
pub fn expand_env_vars_recording(
    input: &str,
) -> Result<(String, std::collections::BTreeMap<String, String>), ConfigError> {
    expand_env_with_recording(input, |name| std::env::var(name).ok())
}

/// Expansion for documents that arrive over the ADMIN API (`/config/apply`,
/// `/config/validate`, section PUTs). S7: such a document is attacker-shaped
/// input from any admin session, so it must not read the server environment
/// at large — `${env:AWS_SECRET_ACCESS_KEY}` in a field that echoes into an
/// error or an export would hand the admin any env var of the process.
///
/// Rule: a ref resolves only from `known` — the `name → value` provenance
/// the RUNNING config recorded when the file on disk was loaded. A name the
/// file does not already use is treated as unset (its `:-default` applies,
/// else a [`ConfigError::MissingEnvVar`]). To use a new env var, reference it
/// in the file on disk (boot), or expand client-side (`config apply`).
///
/// Opt-in: `DGP_CONFIG_ENV_ALLOWLIST` names extra variables that admin input
/// may resolve from the server env (see [`admin_env_lookup`]).
pub fn expand_env_admin(
    input: &str,
    known: &std::collections::BTreeMap<String, String>,
) -> Result<(String, std::collections::BTreeMap<String, String>), ConfigError> {
    expand_env_with_recording(input, |name| admin_env_lookup(name, known))
}

/// The operator opt-in for extra env names that admin input may resolve.
pub const CONFIG_ENV_ALLOWLIST_VAR: &str = "DGP_CONFIG_ENV_ALLOWLIST";

/// THE env lookup for admin-sourced config (document apply/validate/import/
/// restore and section PUTs): the running config's recorded provenance, else
/// the server env for a name the operator allowlisted.
pub fn admin_env_lookup(
    name: &str,
    known: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    if let Some(v) = known.get(name) {
        return Some(v.clone());
    }
    let allowlist: String = super::env_parse(CONFIG_ENV_ALLOWLIST_VAR)?;
    env_name_allowlisted(name, &allowlist)
        .then(|| std::env::var(name).ok())
        .flatten()
}

/// Pure: does `allowlist` (comma-separated names; a trailing `*` matches a
/// prefix) admit `name`? Bootstrap, encryption-key and secret `DGP_*`
/// variables never match: admin input must not read the proxy's own secrets.
pub fn env_name_allowlisted(name: &str, allowlist: &str) -> bool {
    let never = name.starts_with("DGP_BOOTSTRAP_")
        || (name.starts_with("DGP_")
            && (name.contains("ENCRYPTION_KEY") || name.contains("SECRET")));
    !never
        && allowlist
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .any(|p| match p.strip_suffix('*') {
                Some(prefix) => name.starts_with(prefix),
                None => name == p,
            })
}

/// Replace every recorded env value in `msg` with its `${env:NAME}` ref, so
/// an error or warning built from an expanded document never echoes a value.
/// Values shorter than 4 bytes are left alone (they would mangle ordinary
/// text and are no secret).
pub fn scrub_env_values(msg: &str, refs: &std::collections::BTreeMap<String, String>) -> String {
    let mut out = msg.to_string();
    // Longest first, so a value that contains another value is replaced whole.
    let mut pairs: Vec<(&String, &String)> = refs.iter().filter(|(_, v)| v.len() >= 4).collect();
    pairs.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
    for (name, value) in pairs {
        out = out.replace(value.as_str(), &format!("${{env:{name}}}"));
    }
    out
}

/// Escape every `$` as `$$`: the inverse of expansion for text that is
/// already expanded. `config apply` expands on the operator side and then
/// sends this, so the server's own expansion pass returns the same text
/// (a literal `$$` or a `${env:..}`-shaped secret is not expanded twice).
pub fn escape_dollars(s: &str) -> String {
    s.replace('$', "$$")
}

/// Testable core of [`expand_env_vars_recording`].
pub(crate) fn expand_env_with_recording(
    input: &str,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<(String, std::collections::BTreeMap<String, String>), ConfigError> {
    let mut used = std::collections::BTreeMap::new();
    let expanded = expand_env_doc_with(input, |name| {
        let v = lookup(name);
        if let Some(val) = &v {
            if !val.is_empty() {
                used.insert(name.to_string(), val.clone());
            }
        }
        v
    })?;
    Ok((expanded, used))
}

/// Testable core of [`expand_env_vars`]: `lookup` resolves a var name to its
/// value (`None` = unset).
pub(crate) fn expand_env_with(
    input: &str,
    lookup: impl FnMut(&str) -> Option<String>,
) -> Result<String, ConfigError> {
    expand_core(input, lookup, false)
}

/// [`expand_env_with`] for a whole YAML DOCUMENT: a ref that is an entire
/// plain scalar becomes `!envref "value"`, a string that YAML cannot
/// re-type; an entire quoted scalar (`"…"`, `'…'`) becomes an escaped
/// double-quoted string. Spliced as plain text, an all-digit AES key read as
/// a number, a secret `true` as a bool, and the config failed to load. The
/// typed load (`lenient::from_value`) gives the value the type of its field.
pub(crate) fn expand_env_doc_with(
    input: &str,
    lookup: impl FnMut(&str) -> Option<String>,
) -> Result<String, ConfigError> {
    expand_core(input, lookup, true)
}

/// Pure: where a ref starting at byte `start` (its `$`) and ending at byte
/// `end` (its `}`) sits in `input`. `Some(quote)` when the ref is a whole
/// scalar (`quote` = the enclosing quote char, or `None` for plain), else
/// `None` (mid-string, a key, a block scalar line).
fn whole_scalar_quote(input: &str, start: usize, end: usize) -> Option<Option<char>> {
    let line_start = input[..start].rfind('\n').map_or(0, |i| i + 1);
    let line_end = input[end + 1..]
        .find('\n')
        .map_or(input.len(), |i| end + 1 + i);
    let mut before = &input[line_start..start];
    let mut after = &input[end + 1..line_end];
    let quote = match (before.chars().next_back(), after.chars().next()) {
        (Some(q @ ('"' | '\'')), Some(c)) if c == q => {
            before = &before[..before.len() - 1];
            after = &after[1..];
            Some(q)
        }
        _ => None,
    };
    // What precedes must end a mapping key (`key: `), a sequence dash
    // (`- `) or a flow opener/separator, after whitespace.
    let head = before.trim_end();
    if head.len() == before.len() && !before.ends_with(['[', '{', ',']) {
        return None; // `key:${env:X}` or text glued to the ref
    }
    let opens_scalar = head.ends_with(':')
        || head.ends_with(['[', '{', ','])
        || (!head.is_empty() && head.split_whitespace().all(|t| t == "-"));
    if !opens_scalar || head.contains('#') {
        return None;
    }
    // What follows: end of line, a comment, or a flow separator/closer.
    let tail = after.trim_start();
    let closes = tail.is_empty()
        || (tail.starts_with('#') && tail.len() < after.len())
        || tail.starts_with([',', ']', '}']);
    closes.then_some(quote)
}

/// `value` as a YAML double-quoted scalar.
fn yaml_double_quoted(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn expand_core(
    input: &str,
    mut lookup: impl FnMut(&str) -> Option<String>,
    doc: bool,
) -> Result<String, ConfigError> {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    // `cursor` is the start of the not-yet-copied run of literal text. We only
    // ever break the run at an ASCII `$`, so `&input[cursor..i]` is always a
    // valid UTF-8 slice (UTF-8 continuation bytes are all >= 0x80, never `$`).
    let mut cursor = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            i += 1;
            continue;
        }
        // Flush the literal run before this `$`.
        out.push_str(&input[cursor..i]);
        match bytes.get(i + 1) {
            Some(b'$') => {
                out.push('$'); // `$$` → literal `$`
                i += 2;
            }
            Some(b'{') => {
                let start = i + 2;
                // Find the closing `}`, but stop at an intervening `${` — that
                // means THIS `${` was never closed and we'd otherwise consume a
                // LATER ref's `}`, producing a confusing "invalid name" error.
                let scan = &input[start..];
                let brace = scan.find('}');
                let next_open = scan.find("${");
                let end = match (brace, next_open) {
                    // A `${` appears before the next `}` → this ref is unterminated.
                    (Some(b), Some(o)) if o < b => {
                        return Err(ConfigError::BadEnvRef(format!(
                            "unterminated `${{` near byte {i} (closing `}}` missing before the next `${{`)"
                        )))
                    }
                    (Some(b), _) => start + b,
                    (None, _) => {
                        return Err(ConfigError::BadEnvRef(format!(
                            "unterminated `${{` near byte {i}"
                        )))
                    }
                };
                let inner = &input[start..end];
                // Only `${env:NAME...}` is an env reference. Anything else (IAM
                // permission templates like `${iam:username}`, or any other
                // `${...}`) is emitted VERBATIM for the downstream consumer.
                match inner.strip_prefix("env:") {
                    None => {
                        out.push_str(&input[i..=end]); // copy `${...}` unchanged
                    }
                    Some(spec) => {
                        let (name, default) = match spec.split_once(":-") {
                            Some((n, d)) => (n, Some(d)),
                            None => (spec, None),
                        };
                        if name.is_empty() || !is_valid_env_name(name) {
                            return Err(ConfigError::BadEnvRef(format!(
                                "invalid variable name in `${{env:{spec}}}`"
                            )));
                        }
                        // Resolve to the env value (non-empty), else the default,
                        // else error. set-but-empty falls through to the default.
                        let resolved: String = match lookup(name) {
                            Some(v) if !v.is_empty() => v,
                            _ => match default {
                                Some(d) => d.to_string(),
                                None => return Err(ConfigError::MissingEnvVar(name.to_string())),
                            },
                        };
                        // The value is spliced as RAW pre-parse text, so a
                        // newline/control char would inject YAML structure.
                        // Fail loud rather than silently corrupt the document.
                        if has_unsafe_control_char(&resolved) {
                            return Err(ConfigError::UnsafeEnvValue(name.to_string()));
                        }
                        match doc.then(|| whole_scalar_quote(input, i, end)).flatten() {
                            Some(quote) => {
                                if quote.is_some() {
                                    // A quoted ref is a string: re-quote it
                                    // escaped (a `"` or `\\` in the value broke
                                    // the old splice). A plain ref is typed by
                                    // its field (`lenient::from_value`).
                                    out.pop(); // the opening quote, already copied
                                } else {
                                    out.push_str(super::lenient::ENV_REF_TAG);
                                    out.push(' ');
                                }
                                out.push_str(&yaml_double_quoted(&resolved));
                                i = end + 1 + usize::from(quote.is_some());
                                cursor = i;
                                continue;
                            }
                            None => out.push_str(&resolved),
                        }
                    }
                }
                i = end + 1;
            }
            // bare `$` (EOI, or `$` + ordinary char) → literal `$`
            _ => {
                out.push('$');
                i += 1;
            }
        }
        cursor = i;
    }
    // Flush the trailing literal run.
    out.push_str(&input[cursor..]);
    Ok(out)
}

fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// True if `s` contains a character that, spliced as raw pre-parse text into a
/// YAML scalar, could break or restructure the document: any control char
/// EXCEPT tab (newlines, NUL, etc.). Tab is allowed (legitimate in some values).
fn has_unsafe_control_char(s: &str) -> bool {
    s.chars().any(|c| c.is_control() && c != '\t')
}

#[cfg(test)]
mod tests {

    /// `config apply` expands on the operator side, then the server expands
    /// again. Escaping the client result makes the second pass a no-op:
    /// `pay $$10` stays `pay $10`, and an expanded value that itself looks
    /// like `${env:X}` is not expanded a second time.
    #[test]
    fn client_expand_then_escape_survives_server_pass() {
        let lookup = |n: &str| (n == "TOK").then(|| "a${env:OTHER}b".to_string());
        let doc = "k: pay $$10\nt: ${env:TOK}\nh: $2b$10$x\np: ${iam:username}\n";
        let client = expand_env_with(doc, lookup).unwrap();
        let sent = escape_dollars(&client);
        let server = expand_env_with(&sent, |_| None).unwrap();
        assert_eq!(server, client);
        assert!(server.contains("pay $10") && server.contains("a${env:OTHER}b"));
    }

    #[test]
    fn scrub_replaces_values_longest_first() {
        let refs: std::collections::BTreeMap<String, String> = [
            ("A".to_string(), "secret".to_string()),
            ("B".to_string(), "secret-long".to_string()),
            ("C".to_string(), "x".to_string()),
        ]
        .into();
        assert_eq!(
            scrub_env_values("bad 'secret-long' and secret, x", &refs),
            "bad '${env:B}' and ${env:A}, x"
        );
    }

    #[test]
    fn admin_expansion_ignores_process_env() {
        let known = std::collections::BTreeMap::new();
        assert!(expand_env_admin("${env:HOME}", &known).is_err());
        assert_eq!(expand_env_admin("${env:HOME:-d}", &known).unwrap().0, "d");
    }
    use super::*;

    // ── ${VAR} expansion (expand_env_with) ──────────────────────────────────

    /// Build a lookup from pairs for the pure expander.
    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn expand_basic_var() {
        let r = expand_env_with("a=${env:X} b", env_of(&[("X", "1")])).unwrap();
        assert_eq!(r, "a=1 b");
    }

    #[test]
    fn expand_unset_var_is_error() {
        let err = expand_env_with("a=${env:MISSING}", env_of(&[])).unwrap_err();
        assert!(matches!(err, ConfigError::MissingEnvVar(v) if v == "MISSING"));
    }

    #[test]
    fn expand_default_used_when_unset() {
        let r = expand_env_with("a=${env:X:-fallback}", env_of(&[])).unwrap();
        assert_eq!(r, "a=fallback");
    }

    #[test]
    fn expand_default_used_when_set_but_empty() {
        let r = expand_env_with("a=${env:X:-d}", env_of(&[("X", "")])).unwrap();
        assert_eq!(r, "a=d");
    }

    #[test]
    fn expand_set_value_wins_over_default() {
        let r = expand_env_with("a=${env:X:-d}", env_of(&[("X", "real")])).unwrap();
        assert_eq!(r, "a=real");
    }

    #[test]
    fn expand_empty_default_is_allowed() {
        let r = expand_env_with("a=[${env:X:-}]", env_of(&[])).unwrap();
        assert_eq!(r, "a=[]");
    }

    /// THE collision guard: any non-`env:` `${...}` (IAM permission templates
    /// like `${iam:username}`, or a bare `${foo}`) is NEVER touched by env
    /// expansion — only `${env:...}` expands. This is the bug that crash-looped
    /// a live deploy: `${iam:username}` in a declarative config's resources must
    /// pass through to the IAM layer verbatim.
    #[test]
    fn expand_leaves_iam_templates_untouched() {
        for tmpl in [
            "resources: [debug/scrap/customers/${iam:username}/*]",
            "home/${iam:access_key_id}/*",
            "${iam:username}",
            "${anything_without_env_prefix}",
            "${env_but_no_colon}", // not `env:` → passthrough
        ] {
            assert_eq!(
                expand_env_with(tmpl, env_of(&[("iam:username", "SHOULD_NOT_BE_USED")])).unwrap(),
                tmpl,
                "non-env ${{...}} must pass through unexpanded"
            );
        }
        // Mixed line: only the env: one expands, the IAM one is preserved.
        let r = expand_env_with(
            "key: ${env:AK}  path: home/${iam:username}/*",
            env_of(&[("AK", "realkey")]),
        )
        .unwrap();
        assert_eq!(r, "key: realkey  path: home/${iam:username}/*");
    }

    #[test]
    fn expand_double_dollar_is_literal() {
        let r = expand_env_with("price=$$5 lit=$${env:X}", env_of(&[("X", "no")])).unwrap();
        assert_eq!(r, "price=$5 lit=${env:X}");
    }

    #[test]
    fn expand_bare_dollar_passes_through() {
        // bcrypt hashes / regex anchors: a `$` not followed by `{` or `$` is
        // left verbatim. THE bug this whole feature has to not regress.
        let hash = "$2b$10$abcdEFGHijklMNOpqrstuv";
        let r = expand_env_with(hash, env_of(&[])).unwrap();
        assert_eq!(r, hash);
        assert_eq!(expand_env_with("end$", env_of(&[])).unwrap(), "end$");
        assert_eq!(expand_env_with("a$b$c", env_of(&[])).unwrap(), "a$b$c");
    }

    #[test]
    fn expand_preserves_utf8() {
        let r = expand_env_with("Kołodziejczyk=${env:X}—€", env_of(&[("X", "Zürich")])).unwrap();
        assert_eq!(r, "Kołodziejczyk=Zürich—€");
    }

    #[test]
    fn expand_multiple_and_adjacent() {
        let r = expand_env_with(
            "${env:A}${env:B}-${env:A}",
            env_of(&[("A", "x"), ("B", "y")]),
        )
        .unwrap();
        assert_eq!(r, "xy-x");
    }

    #[test]
    fn expand_unterminated_brace_is_error() {
        let err = expand_env_with("a=${env:X", env_of(&[("X", "1")])).unwrap_err();
        assert!(matches!(err, ConfigError::BadEnvRef(_)));
    }

    #[test]
    fn expand_empty_or_bad_name_is_error() {
        // `env:` with empty/invalid name errors; non-env `${...}` does NOT.
        assert!(matches!(
            expand_env_with("${env:}", env_of(&[])).unwrap_err(),
            ConfigError::BadEnvRef(_)
        ));
        assert!(matches!(
            expand_env_with("${env:1BAD}", env_of(&[])).unwrap_err(),
            ConfigError::BadEnvRef(_)
        ));
        // `${}` and `${1BAD}` (no env: prefix) are NOT env refs → passthrough.
        assert_eq!(expand_env_with("${}", env_of(&[])).unwrap(), "${}");
        assert_eq!(expand_env_with("${1BAD}", env_of(&[])).unwrap(), "${1BAD}");
    }

    #[test]
    fn expand_value_containing_dollar_is_not_reexpanded() {
        // an expanded value that itself contains `${env:...}` is inserted
        // literally, NOT recursively expanded (no loops / injection via values).
        let r = expand_env_with("${env:X}", env_of(&[("X", "${env:Y}")])).unwrap();
        assert_eq!(r, "${env:Y}");
    }

    #[test]
    fn expand_no_dollar_is_identity() {
        let s = "plain: yaml\n  nested: true\n";
        assert_eq!(expand_env_with(s, env_of(&[])).unwrap(), s);
    }

    #[test]
    fn expand_default_can_contain_colon_and_dashes() {
        // only the FIRST `:-` splits; defaults may contain `:` / `-` (URLs!).
        let r = expand_env_with("${env:U:-http://h:9000/a-b}", env_of(&[])).unwrap();
        assert_eq!(r, "http://h:9000/a-b");
    }

    #[test]
    fn expand_rejects_value_with_newline_or_control(/* M6 */) {
        // A value with a newline would inject YAML structure → UnsafeEnvValue.
        let err = expand_env_with("k: ${env:X}", env_of(&[("X", "a\n  b: hijack")])).unwrap_err();
        assert!(matches!(err, ConfigError::UnsafeEnvValue(v) if v == "X"));
        // NUL too.
        assert!(matches!(
            expand_env_with("${env:X}", env_of(&[("X", "a\0b")])).unwrap_err(),
            ConfigError::UnsafeEnvValue(_)
        ));
        // Tab is allowed (legitimate in some values).
        assert_eq!(
            expand_env_with("${env:X}", env_of(&[("X", "a\tb")])).unwrap(),
            "a\tb"
        );
        // A control char in the DEFAULT is rejected too.
        assert!(matches!(
            expand_env_with("${env:X:-a\nb}", env_of(&[])).unwrap_err(),
            ConfigError::UnsafeEnvValue(_)
        ));
    }

    #[test]
    fn expand_unterminated_ref_before_next_ref_is_clear_error(/* M10 */) {
        // `${env:A ${env:B}` — the first ref is unterminated; we must NOT greedily
        // consume B's `}` and report a bogus "invalid name `A ${env:B`".
        let err =
            expand_env_with("${env:A ${env:B}", env_of(&[("A", "1"), ("B", "2")])).unwrap_err();
        match err {
            ConfigError::BadEnvRef(msg) => assert!(
                msg.contains("unterminated"),
                "expected an 'unterminated' message, got: {msg}"
            ),
            other => panic!("expected BadEnvRef(unterminated), got {other:?}"),
        }
    }

    #[test]
    fn expand_missing_var_message_says_unset_or_empty(/* M9 */) {
        // set-but-empty with no default → MissingEnvVar, and the message must not
        // assert the var is "not set" (it IS set, to empty).
        let err = expand_env_with("${env:X}", env_of(&[("X", "")])).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unset or empty"),
            "message should cover the empty case, got: {msg}"
        );
    }

    #[test]
    fn doc_expansion_types_only_whole_scalars() {
        let v = |_: &str| Some("0123".to_string());
        let d = |t: &str| expand_env_doc_with(t, v).unwrap();
        assert_eq!(d("k: ${env:X}\n"), "k: !envref \"0123\"\n");
        assert_eq!(d("k: ${env:X}  # c\n"), "k: !envref \"0123\"  # c\n");
        assert_eq!(d("- ${env:X}"), "- !envref \"0123\"");
        assert_eq!(
            d("k: [${env:X}, ${env:X}]"),
            "k: [!envref \"0123\", !envref \"0123\"]"
        );
        assert_eq!(d("k: \"${env:X}\"\n"), "k: \"0123\"\n");
        assert_eq!(d("k: '${env:X}'"), "k: \"0123\"");
        // Not a whole scalar: spliced as before.
        assert_eq!(d("k: a${env:X}"), "k: a0123");
        assert_eq!(d("k: ${env:X}b"), "k: 0123b");
        assert_eq!(d("k: \"a${env:X}\""), "k: \"a0123\"");
        assert_eq!(d("k: |\n  ${env:X}\n"), "k: |\n  0123\n");
        assert_eq!(d("${env:X}: v"), "0123: v");
        // Escaping of a quoted value.
        let q = |_: &str| Some(r#"a"b\c"#.to_string());
        assert_eq!(
            expand_env_doc_with("k: ${env:X}", q).unwrap(),
            r#"k: !envref "a\"b\\c""#
        );
    }

    #[test]
    fn env_allowlist_truth_table() {
        let list = "LOG_LEVEL, APP_*,DGP_LISTEN_ADDR";
        for yes in ["LOG_LEVEL", "APP_X", "APP_", "DGP_LISTEN_ADDR"] {
            assert!(env_name_allowlisted(yes, list), "{yes}");
        }
        for no in ["LOG", "XAPP_X", "HOME", "LOG_LEVEL2"] {
            assert!(!env_name_allowlisted(no, list), "{no}");
        }
        for never in [
            "DGP_BOOTSTRAP_PASSWORD_HASH",
            "DGP_ENCRYPTION_KEY",
            "DGP_BACKEND_X_ENCRYPTION_KEY",
            "DGP_SECRET_ACCESS_KEY",
            "DGP_BE_AWS_SECRET_ACCESS_KEY",
        ] {
            assert!(!env_name_allowlisted(never, "*"), "{never}");
            assert!(!env_name_allowlisted(never, never), "{never}");
        }
        assert!(env_name_allowlisted("AWS_REGION", "*"));
        assert!(!env_name_allowlisted("AWS_REGION", ""));
    }

    #[test]
    fn is_env_ref_truth_table() {
        assert!(is_env_ref("${env:FOO}"));
        assert!(is_env_ref("${env:FOO:-bar}"));
        assert!(!is_env_ref("plain"));
        assert!(!is_env_ref("${iam:username}"));
        assert!(!is_env_ref("${env:FOO} trailing"));
        assert!(!is_env_ref("prefix ${env:FOO}"));
    }
}
